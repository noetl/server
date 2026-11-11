//! Content-addressed catalog + execution pinning against a real Postgres
//! (noetl/ai-meta#548). Skipped unless `NOETL_TEST_DATABASE_URL` is set, like
//! `catalog_list_decodes_against_postgres.rs`.
//!
//! ```text
//! kubectl --context kind-noetl -n postgres port-forward svc/postgres 55432:5432 &
//! NOETL_TEST_DATABASE_URL=postgres://noetl:noetl@127.0.0.1:55432/noetl \
//!   cargo test --test catalog_cas_postgres -- --nocapture
//! ```
//!
//! One test function on purpose: the flags are read once per process
//! (`OnceLock`), so they are set once, before anything reads them.

use noetl_orchestrate_core::catalog::{self as core, Lookup};
use noetl_server::db::models::catalog::CatalogRegisterRequest;
use noetl_server::services::catalog::CatalogService;
use noetl_server::services::catalog_cas as cas;
use serde_json::json;

fn playbook(path: &str, msg: &str, extra_meta: &str) -> String {
    format!(
        "apiVersion: noetl.io/v2\nkind: Playbook\nmetadata:\n  name: cas-test\n  path: {path}\n{extra_meta}workflow:\n  - step: start\n    tool:\n      kind: noop\n      message: \"{msg}\"\n"
    )
}

async fn register(
    svc: &CatalogService,
    content: String,
    source: Option<&str>,
) -> noetl_server::db::models::catalog::CatalogRegisterResponse {
    svc.register(CatalogRegisterRequest {
        content,
        resource_type: "Playbook".into(),
        source: source.map(str::to_string),
    })
    .await
    .expect("register")
}

#[tokio::test]
async fn content_addressing_dedup_and_pinning_against_postgres() {
    let Ok(url) = std::env::var("NOETL_TEST_DATABASE_URL") else {
        eprintln!("skipped: NOETL_TEST_DATABASE_URL not set");
        return;
    };
    // SAFETY: set before any thread reads the environment; single test fn.
    unsafe {
        std::env::set_var(cas::CAS_ENV, "true");
        std::env::set_var(cas::PINNING_ENV, "true");
        std::env::set_var(cas::HOT_RELOAD_ENV, "true");
    }
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect");
    noetl_server::db::queries::catalog::ensure_archived_column(&pool)
        .await
        .ok();
    cas::ensure_schema(&pool).await;
    assert!(cas::ready(), "CAS tables were not created or not detected");
    assert!(cas::pinning_active());

    // A previous run that panicked leaves rows behind; the prefix is test-only.
    cleanup(&pool, "tests/cas/").await;

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = format!("tests/cas/{nonce}");
    let child = format!("{base}/child");
    let alias = format!("{base}/alias");
    let svc = CatalogService::new(pool.clone());

    // --- dedup -------------------------------------------------------------
    let v1 = register(
        &svc,
        playbook(&child, "one", ""),
        Some("gs://bucket/child.yaml"),
    )
    .await;
    assert_eq!((v1.version, v1.deduplicated), (1, Some(false)));
    let d1 = v1.digest.clone().unwrap();
    assert!(core::is_digest(&d1));

    let again = register(&svc, playbook(&child, "one", ""), None).await;
    assert_eq!(
        (again.version, again.deduplicated, again.catalog_id.as_str()),
        (1, Some(true), v1.catalog_id.as_str())
    );

    // Reformatted + commented: same digest, still no new version.
    let reformatted = format!(
        "# a comment\n{}",
        playbook(&child, "one", "").replace("message: \"one\"", "message: one")
    );
    let again2 = register(&svc, reformatted, None).await;
    assert_eq!((again2.version, again2.deduplicated), (1, Some(true)));

    let v2 = register(&svc, playbook(&child, "two", ""), None).await;
    assert_eq!((v2.version, v2.deduplicated), (2, Some(false)));
    let d2 = v2.digest.clone().unwrap();
    assert_ne!(d1, d2);

    // Roll back to v1's body: a real new version (3), same digest as v1.
    let v3 = register(&svc, playbook(&child, "one", ""), None).await;
    assert_eq!((v3.version, v3.digest.as_deref()), (3, Some(d1.as_str())));

    // One body per digest, many sources.
    let blob = cas::blob(&pool, &d1).await.unwrap().expect("blob stored");
    assert_eq!(core::playbook_digest(&blob).unwrap(), d1);
    let srcs = cas::sources(&pool, &d1).await.unwrap();
    let uris: Vec<&str> = srcs.iter().map(|s| s.uri.as_str()).collect();
    assert!(
        uris.contains(&format!("noetl://local/{child}@1").as_str()),
        "{uris:?}"
    );
    assert!(
        uris.contains(&format!("noetl://local/{child}@3").as_str()),
        "{uris:?}"
    );
    assert!(uris.contains(&"gs://bucket/child.yaml"), "{uris:?}");
    let blobs: (i64,) = sqlx::query_as("SELECT count(*) FROM noetl.catalog_blob WHERE digest = $1")
        .bind(&d1)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(blobs.0, 1);

    // Different path, same body: a new name, no new body.
    let a1 = register(
        &svc,
        playbook(&child, "two", "").replace(&format!("path: {child}"), &format!("path: {alias}")),
        None,
    )
    .await;
    assert_eq!(a1.version, 1);
    let named = cas::lookup(
        &pool,
        &Lookup::Digest {
            digest: a1.digest.clone().unwrap(),
            path: Some(alias.clone()),
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(named.path, alias);

    // A declared source that contradicts the body is refused.
    let bad = svc
        .register(CatalogRegisterRequest {
            content: playbook(&child, "four", ""),
            resource_type: "Playbook".into(),
            source: Some(format!("gs://b/k#{d1}")),
        })
        .await;
    assert!(
        bad.is_err(),
        "a source whose #sha256 disagrees must be refused"
    );

    // --- pinning -----------------------------------------------------------
    // A root execution of a parent playbook that hot-reloads nothing.
    let parent = format!("{base}/parent");
    register(&svc, playbook(&parent, "p", ""), None).await;
    let root = cas::resolve_execute(&pool, Some(&parent), None, None, None, None)
        .await
        .unwrap();
    assert_eq!(root.resolution.reason, core::Reason::Latest);
    let root_eid: i64 = (nonce % 1_000_000_000) as i64 * 1000 + 1;
    let root_pin = root.to_row(root_eid, None, &playbook(&parent, "p", ""));
    cas::insert_pin(&pool, &root_pin).await.unwrap();

    // Registered AFTER the root started.
    let v4 = register(&svc, playbook(&child, "four", ""), None).await;
    assert_eq!(v4.version, 4);

    let pinned = cas::resolve_execute(&pool, Some(&child), None, None, None, Some(root_eid))
        .await
        .unwrap();
    assert_eq!(
        (pinned.resolution.entry.version, pinned.resolution.reason),
        (3, core::Reason::Pinned),
        "a child of a pinned root must not see a version registered after the root started"
    );
    let unpinned = cas::resolve_execute(&pool, Some(&child), None, None, None, None)
        .await
        .unwrap();
    assert_eq!(unpinned.resolution.entry.version, 4);

    // Explicit version wins (the version the playbook tool sends).
    let exact = cas::resolve_execute(
        &pool,
        Some(&child),
        None,
        Some(&json!(2)),
        None,
        Some(root_eid),
    )
    .await
    .unwrap();
    assert_eq!(
        (exact.resolution.entry.version, exact.resolution.reason),
        (2, core::Reason::Exact)
    );
    // A digest wins over a version (RFC §5.3 rule 2): child@d1 is v3.
    let by_digest = cas::resolve_execute(
        &pool,
        Some(&child),
        None,
        Some(&json!(2)),
        Some(&d1),
        Some(root_eid),
    )
    .await
    .unwrap();
    assert_eq!(
        (
            by_digest.resolution.entry.version,
            by_digest.resolution.reason
        ),
        (3, core::Reason::Digest)
    );
    // A digest no row of this path has is refused.
    let absent = format!("sha256:{}", "0".repeat(64));
    assert!(cas::resolve_execute(
        &pool,
        Some(&child),
        None,
        None,
        Some(&absent),
        Some(root_eid)
    )
    .await
    .is_err());

    // A grandchild inherits the root's watermark through the child's pin.
    let child_eid = root_eid + 1;
    cas::insert_pin(
        &pool,
        &pinned.to_row(child_eid, Some(root_eid), &playbook(&child, "one", "")),
    )
    .await
    .unwrap();
    let grand = cas::resolve_execute(&pool, Some(&child), None, None, None, Some(child_eid))
        .await
        .unwrap();
    assert_eq!(grand.resolution.entry.version, 3);
    assert_eq!(
        grand
            .to_row(child_eid + 1, Some(child_eid), "")
            .root_execution_id,
        root_eid
    );

    // @latest inside the tree, without the root's policy: refused, not downgraded.
    let refused = cas::resolve_execute(
        &pool,
        Some(&child),
        None,
        Some(&json!("latest")),
        None,
        Some(root_eid),
    )
    .await;
    assert!(refused.is_err());

    // A root that lists the child in metadata.catalog.hot_reload may float it.
    let hot_parent = format!("{base}/hot");
    let hot_yaml = playbook(
        &hot_parent,
        "h",
        &format!("  catalog:\n    hot_reload: [\"{base}/*\"]\n"),
    );
    register(&svc, hot_yaml.clone(), None).await;
    let hot_root = cas::resolve_execute(&pool, Some(&hot_parent), None, None, None, None)
        .await
        .unwrap();
    let hot_eid = root_eid + 10;
    cas::insert_pin(&pool, &hot_root.to_row(hot_eid, None, &hot_yaml))
        .await
        .unwrap();
    let v5 = register(&svc, playbook(&child, "five", ""), None).await;
    let hot = cas::resolve_execute(
        &pool,
        Some(&child),
        None,
        Some(&json!("latest")),
        None,
        Some(hot_eid),
    )
    .await
    .unwrap();
    assert_eq!(
        (hot.resolution.entry.version, hot.resolution.reason),
        (v5.version as i32, core::Reason::HotReload)
    );
    // ...but without @latest it is still pinned.
    let still = cas::resolve_execute(&pool, Some(&child), None, None, None, Some(hot_eid))
        .await
        .unwrap();
    assert_eq!(still.resolution.entry.version, 4);

    // The pin reads back with the policy.
    let back = cas::get_pin(&pool, hot_eid).await.unwrap().unwrap();
    assert_eq!(back.policy.hot_reload, vec![format!("{base}/*")]);

    // --- archive -------------------------------------------------------------
    if noetl_server::db::queries::catalog::archived_column_present() {
        sqlx::query("UPDATE noetl.catalog SET archived_at = now() WHERE path = $1 AND version = 3")
            .bind(&child)
            .execute(&pool)
            .await
            .unwrap();
        // Archived after the root started: still the root tree's version.
        let after = cas::resolve_execute(&pool, Some(&child), None, None, None, Some(root_eid))
            .await
            .unwrap();
        assert_eq!(after.resolution.entry.version, 3);
    } else {
        eprintln!("archived_at absent: archive case not exercised");
    }

    cleanup(&pool, "tests/cas/").await;
}

/// Remove every row the test can create under `prefix`, including pins whose
/// catalog row is under it.
async fn cleanup(pool: &sqlx::PgPool, prefix: &str) {
    let like = format!("{prefix}%");
    for stmt in [
        "DELETE FROM noetl.catalog_digest WHERE catalog_id IN (SELECT catalog_id FROM noetl.catalog WHERE path LIKE $1)",
        "DELETE FROM noetl.catalog_blob WHERE first_catalog_id IN (SELECT catalog_id FROM noetl.catalog WHERE path LIKE $1)",
        "DELETE FROM noetl.catalog_source WHERE uri LIKE 'noetl://local/' || $1",
        "DELETE FROM noetl.execution_catalog_pin WHERE path LIKE $1",
        "DELETE FROM noetl.catalog WHERE path LIKE $1",
    ] {
        sqlx::query(stmt).bind(&like).execute(pool).await.expect(stmt);
    }
    sqlx::query("DELETE FROM noetl.catalog_source WHERE uri = 'gs://bucket/child.yaml'")
        .execute(pool)
        .await
        .unwrap();
}
