//! Cross-instance execution: the version pin holds across remote calls, on
//! both sides, against a real Postgres (noetl/ai-meta#548 follow-up). Skipped
//! unless `NOETL_TEST_DATABASE_URL` is set, like `catalog_cas_postgres.rs`.
//!
//! ```text
//! kubectl --context kind-noetl -n postgres port-forward svc/postgres 55432:5432 &
//! NOETL_TEST_DATABASE_URL=postgres://noetl:noetl@127.0.0.1:55432/noetl \
//!   cargo test --test catalog_remote_postgres -- --nocapture
//! ```
//!
//! One test function: the flags are read once per process.
//!
//! - **Caller (A).** A tree pins what a remote reference resolved to the first
//!   time. A newer import of the same playbook (another tree, or the owner
//!   registering v2) never reaches it, whether the step names it as
//!   `noetl://<B>/…`, as `peer/<B>/…`, or a child of the import names a
//!   sibling by its bare name (home-instance resolution).
//! - **Owner (B).** Every delegated execution from one calling root shares one
//!   pin, so B's own sub-playbooks hold their version across the caller's
//!   calls; a different calling root gets a fresh pin.

use noetl_orchestrate_core::catalog::Reason;
use noetl_server::db::models::catalog::CatalogRegisterRequest;
use noetl_server::services::catalog::CatalogService;
use noetl_server::services::catalog_cas as cas;
use noetl_server::services::catalog_peer;
use noetl_server::services::catalog_remote as remote;

fn playbook(path: &str, msg: &str) -> String {
    format!(
        "apiVersion: noetl.io/v2\nkind: Playbook\nmetadata:\n  name: xc-test\n  path: {path}\nworkflow:\n  - step: start\n    tool:\n      kind: noop\n      message: \"{msg}\"\n"
    )
}

async fn register(svc: &CatalogService, content: String) -> (i64, i16, String) {
    let r = svc
        .register(CatalogRegisterRequest {
            content,
            resource_type: "Playbook".into(),
            source: None,
        })
        .await
        .expect("register");
    (r.catalog_id.parse().unwrap(), r.version, r.digest.unwrap())
}

/// Start an execution the way `/api/execute` does under pinning: resolve, then
/// write the pin. Returns the pin row.
#[allow(clippy::too_many_arguments)]
async fn start(
    pool: &sqlx::PgPool,
    execution_id: i64,
    path: &str,
    digest: Option<&str>,
    parent: Option<i64>,
    pin_override: Option<noetl_orchestrate_core::catalog::Pin>,
) -> cas::PinRow {
    let p = cas::resolve_execute(pool, Some(path), None, None, digest, parent, pin_override)
        .await
        .unwrap_or_else(|e| panic!("resolve {path}: {e}"));
    let yaml = p.row.content.clone().unwrap_or_default();
    let row = p.to_row(execution_id, parent, &yaml);
    cas::insert_pin(pool, &row).await.expect("insert pin");
    row
}

#[tokio::test]
async fn the_version_pin_holds_across_remote_calls_against_postgres() {
    let Ok(url) = std::env::var("NOETL_TEST_DATABASE_URL") else {
        eprintln!("skipped: NOETL_TEST_DATABASE_URL not set");
        return;
    };
    // SAFETY: set before any thread reads the environment; single test fn.
    unsafe {
        std::env::set_var(cas::CAS_ENV, "true");
        std::env::set_var(cas::PINNING_ENV, "true");
        std::env::set_var(remote::CALLS_ENV, "true");
        std::env::set_var(remote::SERVE_ENV, "true");
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
    remote::ensure_schema(&pool).await;
    assert!(cas::pinning_active());
    assert!(
        remote::calls_active(),
        "remote tables were not created or not detected"
    );

    cleanup(&pool).await;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = format!("tests/xc/{nonce}");
    let ids = nonce as i64 % 1_000_000_000_000;
    let svc = CatalogService::new(pool.clone());

    // ======================= the owner's side (B) ==========================
    let leaf = format!("{base}/leaf");
    let target = format!("{base}/target");
    let (_, lv1, _) = register(&svc, playbook(&leaf, "leaf one")).await;
    let (_, _, dt) = register(&svc, playbook(&target, "target")).await;

    // A's root 111 delegates for the first time: B pins now.
    let p1 = remote::origin_pin(&pool, ids + 1, "ni-test-a", ids + 111)
        .await
        .unwrap();
    // B registers leaf v2 while A's tree is still running.
    let (_, lv2, _) = register(&svc, playbook(&leaf, "leaf two")).await;
    assert_eq!((lv1, lv2), (1, 2));
    // A's second call from the same root: the same pin, not a fresh one.
    let p1b = remote::origin_pin(&pool, ids + 2, "ni-test-a", ids + 111)
        .await
        .unwrap();
    assert_eq!(p1, p1b, "one calling root, one pin on the owner");
    assert_eq!(p1.root_execution_id, ids + 1);

    // Each delegated run resolves the exact digest A pinned, inside that pin...
    for (e, pin) in [(ids + 10, &p1), (ids + 11, &p1b)] {
        let run = start(&pool, e, &target, Some(&dt), None, Some(pin.clone())).await;
        assert_eq!(
            (run.digest.as_str(), run.root_execution_id),
            (dt.as_str(), ids + 1)
        );
        // ...and its own sub-playbook sees the version current when A's root
        // first called, though v2 exists now.
        let child = start(&pool, e + 100, &leaf, None, Some(e), None).await;
        assert_eq!(
            (child.version, child.reason.as_str()),
            (1, "pinned"),
            "delegated run {e}"
        );
    }
    // A different calling root starts a fresh pin and sees v2.
    let p2 = remote::origin_pin(&pool, ids + 3, "ni-test-a", ids + 222)
        .await
        .unwrap();
    assert!(p2.watermark > p1.watermark);
    start(&pool, ids + 12, &target, Some(&dt), None, Some(p2)).await;
    let child = start(&pool, ids + 112, &leaf, None, Some(ids + 12), None).await;
    assert_eq!(child.version, 2, "a new calling root sees the new version");
    // The same root id from ANOTHER instance is another tree.
    let other = remote::origin_pin(&pool, ids + 4, "ni-test-c", ids + 111)
        .await
        .unwrap();
    assert_ne!(other.root_execution_id, p1.root_execution_id);

    // ======================= the caller's side (A) =========================
    let inst = "ni-test-b";
    let rt_path = format!("{base}/rt");
    let imported = format!("peer/{inst}/{rt_path}");
    let key = format!("noetl://{inst}/{rt_path}");
    let root_pb = format!("{base}/root");
    register(&svc, playbook(&root_pb, "root")).await;
    let root = ids + 500;
    let root_pin = start(&pool, root, &root_pb, None, None, None).await;
    assert_eq!(root_pin.root_execution_id, root);

    // The first call in this tree imported v1 and pinned it (what
    // `resolve_import` does after `locate`).
    let c1 = playbook(&rt_path, "remote one");
    let d1 = noetl_orchestrate_core::catalog::playbook_digest(&c1).unwrap();
    let (_, iv1) = catalog_peer::import(&pool, &imported, &c1, &d1, &format!("{key}@1"), "peer")
        .await
        .unwrap();
    let won = remote::pin_put(
        &pool,
        root,
        &key,
        &remote::RemotePin {
            instance_id: inst.into(),
            path: imported.clone(),
            version: iv1 as i32,
            digest: d1.clone(),
            mode: "import".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(won.digest, d1);

    // B moved to v2 and something imported it (another tree did).
    let c2 = playbook(&rt_path, "remote two");
    let d2 = noetl_orchestrate_core::catalog::playbook_digest(&c2).unwrap();
    let (_, iv2) = catalog_peer::import(&pool, &imported, &c2, &d2, &format!("{key}@2"), "peer")
        .await
        .unwrap();
    assert_eq!((iv1, iv2), (1, 2));

    // Every spelling of the reference, in this tree, still gets v1.
    for (i, spelled) in [key.clone(), imported.clone()].iter().enumerate() {
        let r = cas::resolve_execute(&pool, Some(spelled), None, None, None, Some(root), None)
            .await
            .unwrap_or_else(|e| panic!("{spelled}: {e}"));
        assert_eq!(
            (r.resolution.entry.digest.as_str(), r.resolution.reason),
            (d1.as_str(), Reason::Import),
            "spelling {i}: {spelled}"
        );
    }
    // A later answer cannot overwrite the tree's: first writer wins.
    let again = remote::pin_put(
        &pool,
        root,
        &key,
        &remote::RemotePin {
            instance_id: inst.into(),
            path: imported.clone(),
            version: 2,
            digest: d2.clone(),
            mode: "import".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(again.digest, d1, "first writer wins");
    // Asking this tree for v2's digest explicitly is refused, not swapped.
    let e = cas::resolve_execute(
        &pool,
        Some(&format!("{key}#{d2}")),
        None,
        None,
        None,
        Some(root),
        None,
    )
    .await
    .expect_err("a conflicting explicit digest is refused");
    assert!(e.to_string().contains("this tree pinned"), "{e}");

    // Home-instance resolution: the imported playbook calls a sibling by its
    // bare name; it means the sibling on B.
    let call = start(&pool, root + 1, &key, None, Some(root), None).await;
    assert_eq!(call.path, imported);
    let sib = format!("{base}/sibling");
    let sib_imported = format!("peer/{inst}/{sib}");
    let cs = playbook(&sib, "sibling");
    let ds = noetl_orchestrate_core::catalog::playbook_digest(&cs).unwrap();
    let (_, sv) = catalog_peer::import(&pool, &sib_imported, &cs, &ds, "noetl://x", "peer")
        .await
        .unwrap();
    remote::pin_put(
        &pool,
        root,
        &format!("noetl://{inst}/{sib}"),
        &remote::RemotePin {
            instance_id: inst.into(),
            path: sib_imported.clone(),
            version: sv as i32,
            digest: ds.clone(),
            mode: "import".into(),
        },
    )
    .await
    .unwrap();
    // A local playbook of the same bare name must NOT be picked.
    register(&svc, playbook(&sib, "the local one")).await;
    let r = cas::resolve_execute(&pool, Some(&sib), None, None, None, Some(root + 1), None)
        .await
        .unwrap();
    assert_eq!(
        (r.row.path.as_str(), r.resolution.entry.digest.as_str()),
        (sib_imported.as_str(), ds.as_str()),
        "a child of an import resolves on its home instance"
    );

    // A tree that never pinned the reference, with discovery not running,
    // fails by name instead of picking whatever import is newest.
    let root2 = ids + 600;
    start(&pool, root2, &root_pb, None, None, None).await;
    let e = cas::resolve_execute(&pool, Some(&key), None, None, None, Some(root2), None)
        .await
        .expect_err("no pin, no discovery");
    assert!(e.to_string().contains("NOETL_CATALOG_DISCOVERY"), "{e}");

    // The same tree names the import by its peer/ path. The owner cannot be
    // asked here (no discovery), so the local import it already holds is used
    // AND pinned: the next call in this tree gets the same answer.
    let r = cas::resolve_execute(&pool, Some(&imported), None, None, None, Some(root2), None)
        .await
        .expect("cached import used when the owner cannot answer");
    assert_eq!(
        r.resolution.entry.digest, d2,
        "the newest cached import at root2's start"
    );
    let p = remote::pin_get(&pool, root2, &key)
        .await
        .unwrap()
        .expect("pinned");
    assert_eq!(p.digest, d2);
    let again = cas::resolve_execute(&pool, Some(&key), None, None, None, Some(root2), None)
        .await
        .expect("the noetl:// spelling now hits the pin");
    assert_eq!(again.resolution.entry.digest, d2);

    cleanup(&pool).await;
}

async fn cleanup(pool: &sqlx::PgPool) {
    for like in ["tests/xc/%", "peer/ni-test-b/tests/xc/%"] {
        for stmt in [
            "DELETE FROM noetl.catalog_digest WHERE catalog_id IN (SELECT catalog_id FROM noetl.catalog WHERE path LIKE $1)",
            "DELETE FROM noetl.catalog_blob WHERE first_catalog_id IN (SELECT catalog_id FROM noetl.catalog WHERE path LIKE $1)",
            "DELETE FROM noetl.catalog_source WHERE uri LIKE 'noetl://local/' || $1",
            "DELETE FROM noetl.execution_catalog_pin WHERE path LIKE $1",
            "DELETE FROM noetl.execution_remote_pin WHERE path LIKE $1",
            "DELETE FROM noetl.catalog WHERE path LIKE $1",
        ] {
            sqlx::query(stmt).bind(like).execute(pool).await.expect(stmt);
        }
    }
    for stmt in [
        "DELETE FROM noetl.catalog_source WHERE uri LIKE 'noetl://ni-test-b/%' OR uri = 'noetl://x'",
        "DELETE FROM noetl.catalog_remote_origin WHERE origin_instance IN ('ni-test-a','ni-test-c')",
    ] {
        sqlx::query(stmt).execute(pool).await.expect(stmt);
    }
}
