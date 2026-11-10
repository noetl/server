//! Cross-binary end to end for build plan item 9: this server's instance routes on a TCP
//! port, driven by the real `noetl` CLI as two separate instances.
//!
//! Instance B runs `noetl instance serve` (register, heartbeat, long-poll its inbox, run
//! locally, post results). Instance A registers, discovers B, and hands it a pinned playbook
//! with `noetl instance handoff --wait`. Neither instance accepts an inbound connection.
//!
//! Skipped unless `NOETL_CLI_BIN` points at a `noetl` built from noetl/cli
//! `feat/instance-register-discover` or later:
//!
//! ```text
//! NOETL_CLI_BIN=/path/to/cli/target/debug/noetl cargo test --test instances_cli_e2e -- --nocapture
//! ```

use std::process::{Command, Stdio};
use std::time::Duration;

use noetl_server::instances::gate::{Gate, Trust};
use noetl_server::instances::registry::Registry;
use noetl_server::instances::tasks::Board;
use noetl_server::instances::{admin_routes_for, routes_for, InstanceState};
use serde_json::Value;

const HELLO: &str = r#"
apiVersion: noetl.io/v2
kind: Playbook
metadata: { name: hello }
workload: { who: nobody }
workflow:
  - step: start
    next: [ { step: greet } ]
  - step: greet
    tool: { kind: shell, cmds: [ 'echo "hello {{ workload.who }}"' ] }
"#;

fn cli(bin: &str, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(bin).args(args).output().expect("run noetl");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_cli_instances_register_discover_and_hand_off_through_this_server() {
    let Ok(bin) = std::env::var("NOETL_CLI_BIN") else {
        eprintln!("skipped: set NOETL_CLI_BIN to a noetl binary with `noetl instance`");
        return;
    };
    let state = InstanceState::new(
        Gate::new(Trust::Open, "noetl", 120_000),
        Registry::new(60_000, 100),
        Board::new(30_000),
    );
    let app = routes_for(state.clone()).merge(admin_routes_for(state.clone()));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    let (ka, kb) = (dir.path().join("a/key.json"), dir.path().join("b/key.json"));
    let (ka, kb) = (ka.to_str().unwrap(), kb.to_str().unwrap());
    let books = dir.path().join("books");
    std::fs::create_dir_all(books.join("edge")).unwrap();
    std::fs::write(books.join("edge/hello.yaml"), HELLO).unwrap();
    let pin = books.join("edge/hello.yaml");

    let (ok, a_id, e) = cli(&bin, &["instance", "init", "--key", ka]);
    assert!(ok, "{e}");
    let (ok, b_id, e) = cli(&bin, &["instance", "init", "--key", kb]);
    assert!(ok, "{e}");
    let (a_id, b_id) = (a_id.trim().to_string(), b_id.trim().to_string());

    // B serves: one task, then it deregisters and exits.
    let serve = Command::new(&bin)
        .args([
            "instance",
            "serve",
            "--server",
            &server,
            "--key",
            kb,
            "--runtime-kind",
            "daemon",
            "--site",
            "home",
            "--offer",
            "edge/*",
            "--playbook-dir",
            books.to_str().unwrap(),
            "--max-tasks",
            "1",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Wait for B's registration to land.
    let mut seen = false;
    for _ in 0..100 {
        if state.registry.lock().unwrap().get(&b_id).is_some() {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(seen, "B never registered");

    let (ok, out, e) = cli(
        &bin,
        &[
            "instance", "register", "--once", "--server", &server, "--key", ka, "--peer", &b_id,
        ],
    );
    assert!(ok, "{e}");
    let r: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(r["instance_id"], a_id.as_str());

    let (ok, out, e) = cli(
        &bin,
        &[
            "instance",
            "discover",
            "--server",
            &server,
            "--key",
            ka,
            "--runs",
            "edge/hello",
        ],
    );
    assert!(ok, "{e}");
    let found: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(found["count"], 1, "{found}");
    assert_eq!(found["instances"][0]["instance_id"], b_id.as_str());
    assert_eq!(found["instances"][0]["runtime_kind"], "daemon");

    let bin2 = bin.clone();
    let (server2, ka2, pin2) = (
        server.clone(),
        ka.to_string(),
        pin.to_str().unwrap().to_string(),
    );
    let (ok, out, e) = tokio::task::spawn_blocking(move || {
        cli(
            &bin2,
            &[
                "instance",
                "handoff",
                "--server",
                &server2,
                "--key",
                &ka2,
                "--at-site",
                "home",
                "--playbook",
                "edge/hello",
                "--pin",
                &pin2,
                "--payload",
                r#"{"who":"peer"}"#,
                "--wait",
                "60",
            ],
        )
    })
    .await
    .unwrap();
    assert!(ok, "handoff failed: {e}\n{out}");
    let task: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(task["state"], "completed", "{task}");
    assert_eq!(task["from"], a_id.as_str());
    assert_eq!(task["to"], b_id.as_str());
    assert!(
        task["result"]["final_result"]
            .to_string()
            .contains("hello peer"),
        "{task}"
    );

    let status = tokio::task::spawn_blocking(move || serve.wait_with_output().unwrap())
        .await
        .unwrap();
    assert!(
        status.status.success(),
        "serve: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        state.registry.lock().unwrap().get(&b_id).is_none(),
        "B deregistered on exit"
    );
}
