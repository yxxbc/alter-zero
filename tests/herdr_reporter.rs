//! Integration proof of the herdr integration (`docs/herdr.md`) against the
//! **real built binary** (the `mcp_cli.rs` pattern): `integration hook
//! herdr` reads one hook payload on stdin and sends one request line to a
//! stub herdr socket — the pane's state carried between the separate
//! processes the hooks are — and `integration install`/`status`/`uninstall`
//! manage the `hooks.d/` file. Offline: the socket is a local listener.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};

/// The TUI binary this test run just built.
fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_alter-zero"))
}

/// A stand-in for herdr's API socket: each connection's one request line is
/// answered herdr's way and handed to the test.
struct Herdr {
    socket: PathBuf,
    requests: mpsc::Receiver<Value>,
    _dir: tempfile::TempDir,
}

impl Herdr {
    fn start() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket).expect("bind the stub socket");
        let (tx, requests) = mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut line = String::new();
                let Ok(clone) = stream.try_clone() else {
                    continue;
                };
                if BufReader::new(clone).read_line(&mut line).is_ok() && !line.is_empty() {
                    let request: Value = serde_json::from_str(&line).expect("one JSON line");
                    let reply = json!({ "id": request["id"], "result": { "type": "ok" } });
                    let _ = writeln!(stream, "{reply}");
                    if tx.send(request).is_err() {
                        break;
                    }
                }
            }
        });
        Self {
            socket,
            requests,
            _dir: dir,
        }
    }

    /// The next request, or a failure naming what was expected.
    fn next(&self, what: &str) -> Value {
        self.requests
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("herdr never heard {what}"))
    }

    /// Nothing arrives.
    fn silent(&self, why: &str) {
        assert!(
            self.requests
                .recv_timeout(Duration::from_millis(300))
                .is_err(),
            "{why}"
        );
    }
}

/// Run the reporter on `payload` in pane `w1:p2`, with `extra` environment
/// on top — the pane's variables first, the agent marker scrubbed (a test
/// run from an Alter Zero agent's own terminal would inherit it).
fn report(config: &Path, herdr: &Herdr, payload: &Value, extra: &[(&str, &str)]) -> Output {
    let mut child = Command::new(bin())
        .args(["integration", "hook", "herdr"])
        .env("ALTER_ZERO_CONFIG_DIR", config)
        .env("HERDR_ENV", "1")
        .env("HERDR_PANE_ID", "w1:p2")
        .env("HERDR_SOCKET_PATH", &herdr.socket)
        .env_remove("ALTER_ZERO_AGENT")
        .envs(extra.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary runs");
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(payload.to_string().as_bytes())
        .expect("payload written");
    let out = child.wait_with_output().expect("it exits");
    // A hook's whole contract with the engine: exit 0, say nothing.
    assert!(out.status.success(), "{out:?}");
    assert!(
        out.stdout.is_empty(),
        "stdout becomes model context: {out:?}"
    );
    assert!(out.stderr.is_empty(), "{out:?}");
    out
}

fn event(name: &str) -> Value {
    json!({
        "hook_event_name": name,
        "session_id": "19a8f3c2d4e5f60-1a2b",
        "transcript_path": null,
        "cwd": "/work",
    })
}

fn recorded(name: &str) -> Value {
    let mut value = event(name);
    value["transcript_path"] = json!("/s/rollout-2026-10-06T21-40-00-19a8f3c2d4e5f60-1a2b.jsonl");
    value
}

fn on(mut value: Value, fields: &[(&str, Value)]) -> Value {
    for (key, field) in fields {
        value[*key] = field.clone();
    }
    value
}

#[test]
fn a_turn_reaches_herdr_as_working_then_idle_with_the_resume_command() {
    let config = tempfile::tempdir().unwrap();
    let herdr = Herdr::start();

    report(config.path(), &herdr, &event("SessionStart"), &[]);
    let start = herdr.next("the session start");
    assert_eq!(start["method"], json!("pane.report_agent"));
    assert_eq!(start["params"]["pane_id"], json!("w1:p2"), "{start}");
    assert_eq!(start["params"]["source"], json!("alter-zero"));
    assert_eq!(start["params"]["agent"], json!("alter-zero"));
    assert_eq!(start["params"]["state"], json!("idle"));
    assert_eq!(
        start["params"]["resume_argv"],
        json!(["alter-zero"]),
        "no rollout yet: a restore starts afresh in the same directory"
    );
    assert!(
        start["params"].get("agent_session_id").is_none(),
        "nothing to resume: {start}"
    );

    report(config.path(), &herdr, &recorded("UserPromptSubmit"), &[]);
    let working = herdr.next("the prompt");
    assert_eq!(working["params"]["state"], json!("working"));
    assert_eq!(
        working["params"]["resume_argv"],
        json!(["alter-zero", "--resume", "19a8f3c2d4e5f60-1a2b"])
    );

    report(config.path(), &herdr, &recorded("Stop"), &[]);
    let idle = herdr.next("the stop");
    assert_eq!(idle["params"]["state"], json!("idle"));

    let seqs: Vec<u64> = [&start, &working, &idle]
        .iter()
        .map(|r| r["params"]["seq"].as_u64().expect("a seq"))
        .collect();
    assert!(
        seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "herdr drops a report whose seq does not climb: {seqs:?}"
    );
}

#[test]
fn a_prompt_blocks_across_the_separate_processes_the_hooks_are() {
    let config = tempfile::tempdir().unwrap();
    let herdr = Herdr::start();
    let states =
        |herdr: &Herdr, what: &str| -> Value { herdr.next(what)["params"]["state"].clone() };

    report(config.path(), &herdr, &recorded("UserPromptSubmit"), &[]);
    assert_eq!(states(&herdr, "the prompt"), json!("working"));
    let prompt = on(
        recorded("Notification"),
        &[
            ("notification_type", json!("permission_prompt")),
            ("tool_name", json!("bash")),
            ("tool_use_id", json!("call-1")),
        ],
    );
    report(config.path(), &herdr, &prompt, &[]);
    assert_eq!(states(&herdr, "the permission prompt"), json!("blocked"));
    let answered = on(
        recorded("PermissionResult"),
        &[
            ("tool_name", json!("bash")),
            ("tool_use_id", json!("call-1")),
            ("decision", json!({"behavior": "allow", "source": "user"})),
        ],
    );
    report(config.path(), &herdr, &answered, &[]);
    assert_eq!(
        states(&herdr, "the answer"),
        json!("working"),
        "the turn the last process saw is still running"
    );
    report(config.path(), &herdr, &recorded("Interrupt"), &[]);
    assert_eq!(states(&herdr, "the interrupt"), json!("idle"));
}

#[test]
fn the_lead_and_an_agent_sharing_a_call_id_are_two_prompts_across_processes() {
    // Ollama numbers every round's calls from `call_0`: the lead's prompt
    // and a subagent's share the id, and the pane's state file keeps them
    // apart from one hook process to the next.
    let config = tempfile::tempdir().unwrap();
    let herdr = Herdr::start();
    let states =
        |herdr: &Herdr, what: &str| -> Value { herdr.next(what)["params"]["state"].clone() };
    let by = |mut value: Value, agent: Option<&str>| {
        if let Some(agent) = agent {
            value["agent_id"] = json!(agent);
            value["agent_type"] = json!("general-purpose");
        }
        value
    };
    let prompt = |agent| {
        by(
            on(
                recorded("Notification"),
                &[
                    ("notification_type", json!("permission_prompt")),
                    ("tool_name", json!("bash")),
                    ("tool_use_id", json!("call_0")),
                ],
            ),
            agent,
        )
    };
    let answered = |agent| {
        by(
            on(
                recorded("PermissionResult"),
                &[
                    ("tool_name", json!("bash")),
                    ("tool_use_id", json!("call_0")),
                    ("decision", json!({"behavior": "allow", "source": "user"})),
                ],
            ),
            agent,
        )
    };

    report(config.path(), &herdr, &recorded("UserPromptSubmit"), &[]);
    assert_eq!(states(&herdr, "the prompt"), json!("working"));
    report(config.path(), &herdr, &prompt(None), &[]);
    assert_eq!(states(&herdr, "the lead's prompt"), json!("blocked"));
    report(config.path(), &herdr, &prompt(Some("a1")), &[]);
    assert_eq!(states(&herdr, "the agent's prompt"), json!("blocked"));
    report(config.path(), &herdr, &answered(None), &[]);
    assert_eq!(
        states(&herdr, "the lead's answer"),
        json!("blocked"),
        "the agent's prompt is still on screen"
    );
    report(config.path(), &herdr, &answered(Some("a1")), &[]);
    assert_eq!(states(&herdr, "the agent's answer"), json!("working"));
}

#[test]
fn quitting_releases_the_pane_and_forgets_its_state() {
    let config = tempfile::tempdir().unwrap();
    let herdr = Herdr::start();
    report(config.path(), &herdr, &recorded("UserPromptSubmit"), &[]);
    let _ = herdr.next("the prompt");
    let state_dir = config.path().join("herdr");
    assert_eq!(
        std::fs::read_dir(&state_dir).unwrap().count(),
        1,
        "one state file for the pane"
    );

    let cleared = on(recorded("SessionEnd"), &[("reason", json!("clear"))]);
    report(config.path(), &herdr, &cleared, &[]);
    herdr.silent("a /clear replaces the conversation: the SessionStart after it reports");

    let quit = on(
        recorded("SessionEnd"),
        &[("reason", json!("prompt_input_exit"))],
    );
    report(config.path(), &herdr, &quit, &[]);
    let release = herdr.next("the release");
    assert_eq!(release["method"], json!("pane.release_agent"));
    assert_eq!(
        release["params"],
        json!({
            "pane_id": "w1:p2",
            "source": "alter-zero",
            "agent": "alter-zero",
            "seq": release["params"]["seq"],
        })
    );
    assert_eq!(
        std::fs::read_dir(&state_dir).unwrap().count(),
        0,
        "nothing to remember for a pane the session left"
    );
}

#[test]
fn outside_herdr_or_inside_an_agents_own_terminal_nothing_is_sent() {
    let config = tempfile::tempdir().unwrap();
    let herdr = Herdr::start();
    report(
        config.path(),
        &herdr,
        &recorded("UserPromptSubmit"),
        &[("HERDR_ENV", "0")],
    );
    herdr.silent("HERDR_ENV is not 1: not a herdr pane");
    report(
        config.path(),
        &herdr,
        &recorded("UserPromptSubmit"),
        &[("ALTER_ZERO_AGENT", "1")],
    );
    herdr.silent("a nested Alter Zero leaves the pane to the session that started it");
}

#[test]
fn a_name_or_payload_the_reporter_cannot_read_still_exits_zero_and_silently() {
    let config = tempfile::tempdir().unwrap();
    let herdr = Herdr::start();
    let out = Command::new(bin())
        .args(["integration", "hook", "tmux", "--anything"])
        .env("ALTER_ZERO_CONFIG_DIR", config.path())
        .stdin(Stdio::null())
        .output()
        .expect("runs");
    assert!(
        out.status.success() && out.stdout.is_empty() && out.stderr.is_empty(),
        "{out:?}"
    );
    let mut child = Command::new(bin())
        .args(["integration", "hook", "herdr"])
        .env("ALTER_ZERO_CONFIG_DIR", config.path())
        .env("HERDR_ENV", "1")
        .env("HERDR_PANE_ID", "w1:p2")
        .env("HERDR_SOCKET_PATH", &herdr.socket)
        .env_remove("ALTER_ZERO_AGENT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"not json").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success() && out.stdout.is_empty() && out.stderr.is_empty(),
        "{out:?}"
    );
    herdr.silent("a payload that is not one reports nothing");
}

#[test]
fn install_writes_the_hooks_file_status_reports_it_and_uninstall_removes_it() {
    let config = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| -> Output {
        Command::new(bin())
            .arg("integration")
            .args(args)
            .env("ALTER_ZERO_CONFIG_DIR", config.path())
            .env_remove("HERDR_ENV")
            .output()
            .expect("runs")
    };
    let file = config.path().join("hooks.d").join("herdr.json");

    let out = run(&["install", "herdr"]);
    assert!(out.status.success(), "{out:?}");
    let text = std::fs::read_to_string(&file).expect("the hooks file was written");
    assert_eq!(text, alter_zero::herdr::hooks_file_json());
    assert!(
        alter_zero::hooks::HooksFile::parse(&text).is_ok(),
        "a valid hooks.json"
    );
    let printed = String::from_utf8_lossy(&out.stdout);
    assert!(
        printed.contains("Installed the herdr integration"),
        "{printed}"
    );

    let status = run(&["status"]);
    let printed = String::from_utf8_lossy(&status.stdout);
    assert!(printed.contains("herdr: installed"), "{printed}");
    assert!(printed.contains("not a herdr pane"), "{printed}");

    let out = run(&["uninstall", "herdr"]);
    assert!(out.status.success(), "{out:?}");
    assert!(!file.exists());
    let again = run(&["uninstall", "herdr"]);
    assert!(again.status.success(), "removing twice is not an error");
    assert!(String::from_utf8_lossy(&again.stdout).contains("not installed"));
}

#[test]
fn a_usage_error_names_the_integration_page() {
    let out = Command::new(bin())
        .args(["integration", "install", "tmux"])
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unknown integration 'tmux'"), "{err}");
    assert!(err.contains("alter-zero integration install"), "{err}");
}
