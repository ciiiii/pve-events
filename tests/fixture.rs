//! End-to-end parse of a real `/api2/json/cluster/tasks` response.
//!
//! The fixture is an actual PVE 9.2 reply (hostnames and UPIDs scrubbed), which
//! is the point: it carries the shapes a hand-written fixture gets wrong —
//! a still-running task with no `endtime`, an `imgdel` whose id is a bare
//! storage name with no `@`, `tokenid` split out from `user`, and failure rows
//! whose `status` is free-form English containing an apostrophe.

use std::process::Command;

fn fixture() -> serde_json::Value {
    let raw = include_str!("fixtures/cluster-tasks.json");
    serde_json::from_str(raw).expect("fixture is valid JSON")
}

#[test]
fn fixture_matches_the_documented_response_shape() {
    let v = fixture();
    let rows = v["data"].as_array().expect("data is an array");
    assert!(rows.len() > 20, "fixture should be a realistic window");

    // The four shapes the parser has to survive.
    assert!(rows.iter().any(|r| r.get("endtime").is_none()), "needs a running task");
    assert!(rows.iter().any(|r| r.get("tokenid").is_some()), "needs a token-run task");
    assert!(rows.iter().any(|r| r["status"] != "OK" && r.get("endtime").is_some()), "needs a failure");
    assert!(
        rows.iter().any(|r| r["type"] == "imgdel" && !r["id"].as_str().unwrap_or("").contains('@')),
        "needs an imgdel whose id is a bare storage name"
    );
    // PVE never puts the token in `user`; the whole identity fix depends on this.
    assert!(
        rows.iter().all(|r| !r["user"].as_str().unwrap_or("").contains('!')),
        "user must never contain '!' — the token lives in tokenid"
    );
}

/// The documented config must actually load. Without this the example drifts out
/// of step with the struct definitions and every new user's first run fails.
#[test]
fn the_shipped_example_config_is_valid() {
    let state = std::env::temp_dir().join("pve-events-example-config.json");
    let out = Command::new(env!("CARGO_BIN_EXE_pve-events"))
        .args(["--config", "config.example.toml", "--once", "--dry-run"])
        .env("PVE_TOKEN_VALUE", "test")
        .env("PVE_EVENTS_STATE", &state)
        // Point at a closed port: the poll is expected to fail, but only AFTER
        // config parsing and sink-template validation have both passed.
        .env("PVE_URL", "http://127.0.0.1:1")
        .output()
        .expect("running the binary");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "example config rejected:\n{stderr}");
    assert!(stderr.contains("poll failed"), "expected the poll, not the config, to fail:\n{stderr}");
}

/// Drives the built binary against the fixture over a throwaway HTTP server, so
/// the assertion covers argument parsing, config, TLS-off, polling, filtering
/// and rendering — not just the library internals.
#[test]
fn binary_renders_the_fixture_end_to_end() {
    let body = include_str!("fixtures/cluster-tasks.json").to_string();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();

    let server = std::thread::spawn(move || {
        use std::io::{Read, Write};
        // One request: the binary runs with --once.
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });

    let state = std::env::temp_dir().join("pve-events-e2e").join("state.json");
    let _ = std::fs::remove_file(&state);

    let out = Command::new(env!("CARGO_BIN_EXE_pve-events"))
        .args(["--once", "--dry-run"])
        .env("PVE_URL", format!("http://{addr}"))
        .env("PVE_USER", "monitor@pve")
        .env("PVE_TOKEN_NAME", "events")
        .env("PVE_TOKEN_VALUE", "test")
        .env("PVE_EVENTS_STATE", &state)
        // The fixture is historical, so without a zeroed watermark every row
        // would sort below "now" and nothing would be emitted.
        .env("PVE_EVENTS_BACKFILL_FROM", "0")
        .output()
        .expect("running the binary");

    let _ = server.join();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "binary failed:\n{stderr}");

    // Guest and disk events render; the natively-notified classes do not.
    assert!(stdout.contains("VM 1440 created"), "missing qmcreate:\n{stdout}");
    assert!(stdout.contains("disk resized"), "missing resize:\n{stdout}");
    assert!(stdout.contains("CT 102 started"), "missing vzstart:\n{stdout}");
    assert!(stdout.contains("FAILED"), "missing a failure:\n{stdout}");
    assert!(!stdout.contains("package index updated"), "aptupdate must be off by default:\n{stdout}");

    // The id-shape bug: an imgdel on storage `local` must not read "VM local".
    assert!(!stdout.contains("VM local"), "storage volume mislabelled as a VM:\n{stdout}");
    assert!(stdout.contains("local disk image deleted"), "missing imgdel:\n{stdout}");
}
