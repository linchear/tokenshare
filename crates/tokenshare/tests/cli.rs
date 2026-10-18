//! Runs the real `tokenshare` binary against a delegate running in this process.

use std::process::{Command, Output};

use tokenshare_core::daemon::{self, Running};
use tokenshare_core::table::{now_ms, DelegateEntry};
use tokenshare_core::Config;

/// Every peer here lives on a closed loopback port. The delegate's background
/// join/gossip loops may try to contact them, and that must never be able to reach
/// a real network (an earlier version used real addresses and registered test
/// delegates in a live mesh).
async fn delegate_with_peers() -> Running {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ts-cli-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let config = Config {
        port: 0,
        ollama_hosts: vec![],
        // Without an address a delegate is not listed (peers could not use it yet).
        advertise_url: Some("http://127.0.0.1:9".into()),
        ..Config::default()
    };
    let running = daemon::start(config, dir).await.unwrap();
    for (id, url, models) in [
        ("d44294c0-dddd", "http://127.0.0.1:1", vec![]),
        (
            "8bbb822d-bbbb",
            "http://127.0.0.1:2",
            vec!["qwen3:14b".to_string()],
        ),
        ("8bbb9999-cccc", "http://127.0.0.1:3", vec![]),
    ] {
        let mut e = DelegateEntry::new(id.into(), url.into(), models);
        e.last_seen = now_ms();
        running.state.delegates.merge([e]);
    }
    running
}

/// Run `tokenshare <args> --port <port>` without blocking the test's runtime.
async fn run(port: u16, args: &[&str]) -> Output {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_tokenshare"))
            .args(&args)
            .args([
                "--config",
                "/nonexistent/tokenshare.toml",
                "--port",
                &port.to_string(),
            ])
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

#[tokio::test]
async fn delegates_lists_everyone_and_marks_this_one() {
    let d = delegate_with_peers().await;
    let out = run(d.addr.port(), &["delegates"]).await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    let t = text(&out.stdout);
    assert!(t.contains("(this delegate)"), "{t}");
    assert!(
        t.contains("http://127.0.0.1:2") && t.contains("qwen3:14b"),
        "{t}"
    );
    assert!(t.contains("d44294c0"), "{t}");
}

#[tokio::test]
async fn forget_removes_a_delegate_by_id_prefix_and_by_url() {
    let d = delegate_with_peers().await;
    let port = d.addr.port();

    let out = run(port, &["forget", "d442"]).await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("Forgot d44294c0"));
    assert!(d
        .state
        .delegates
        .peers()
        .iter()
        .all(|p| p.id != "d44294c0-dddd"));

    let out = run(port, &["forget", "http://127.0.0.1:3"]).await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(d
        .state
        .delegates
        .peers()
        .iter()
        .all(|p| p.id != "8bbb9999-cccc"));
    assert_eq!(
        d.state.delegates.peers().len(),
        1,
        "the other peer is untouched"
    );
}

#[tokio::test]
async fn forget_refuses_ambiguous_unknown_and_self() {
    let d = delegate_with_peers().await;
    let port = d.addr.port();
    let before = d.state.delegates.peers().len();

    let out = run(port, &["forget", "8bbb"]).await;
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("ambiguous"),
        "{}",
        text(&out.stderr)
    );

    let out = run(port, &["forget", "nothing-like-this"]).await;
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("no delegate matches"));

    let me = d.state.id.clone();
    let out = run(port, &["forget", &me]).await;
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("cannot forget itself"));

    assert_eq!(
        d.state.delegates.peers().len(),
        before,
        "nothing was removed"
    );
}

#[tokio::test]
async fn commands_say_so_when_no_delegate_is_running() {
    // Port 1 has nothing listening.
    let out = run(1, &["delegates"]).await;
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("cannot reach a delegate"),
        "{}",
        text(&out.stderr)
    );
}
