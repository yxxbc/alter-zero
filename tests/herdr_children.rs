//! The nested-session marker (`docs/herdr.md` *Nested sessions*) on the real
//! spawn paths: once a session reports for a herdr pane, every shell child it
//! starts carries `ALTER_ZERO_HERDR_REPORTER={pane}`, so an alter-zero run
//! from one of them stays quiet instead of fighting its parent for the pane.
//!
//! Its own test binary on purpose: [`herdr::start`] marks the whole
//! **process** as the pane's reporter, and every child any test in the binary
//! spawns from then on carries the marker — which the library's unit tests
//! must never see. Offline and deterministic: the reporter's socket does not
//! exist, so its worker fails fast and sends nothing.

#![cfg(unix)]

use std::io::Read;
use std::path::PathBuf;
use std::sync::OnceLock;

use alter_zero::herdr::{self, Pane, Reporter};
use alter_zero::llm::exec::{RealToolExecutor, ToolExecutor};
use alter_zero::llm::tools::ToolCallRequest;
use alter_zero::stream::CancelToken;

/// The pane this test process reports for.
const PANE: &str = "w7:p3";

/// The command every probe runs: the marker, or `unset`.
const PROBE: &str = "printf 'marker=%s\\n' \"${ALTER_ZERO_HERDR_REPORTER-unset}\"";

/// The production detach helper: the TUI binary this test run just built.
fn helper() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_alter-zero"))
}

/// Mark this process as `PANE`'s reporter, once for the whole binary — the
/// boundary's own call at startup.
fn reporting() {
    static REPORTER: OnceLock<std::sync::Mutex<Reporter>> = OnceLock::new();
    REPORTER.get_or_init(|| {
        std::sync::Mutex::new(herdr::start(Pane {
            id: PANE.into(),
            socket: PathBuf::from("/nonexistent/herdr.sock"),
        }))
    });
    assert_eq!(herdr::reporting_pane(), Some(PANE));
}

#[test]
fn a_shell_command_carries_the_marker() {
    // The `!` command's and the lifecycle hooks' path, and the `bash` tool's
    // where no terminal can be had: the detach tier walk.
    reporting();
    let child =
        alter_zero::subprocess::spawn_detached_shell(Some(&helper()), PROBE).expect("spawned");
    let output = child.wait_with_output().expect("ran");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "marker=w7:p3\n");
}

#[test]
fn a_terminal_session_carries_the_marker() {
    // The `bash` tool's own path: a pseudo-terminal of its own.
    reporting();
    let mut process = alter_zero::pty::spawn::spawn(Some(&helper()), PROBE).expect("spawned");
    let mut output = Vec::new();
    let mut chunk = [0u8; 4096];
    // The master reads end in EIO once the session lets go of the terminal.
    while let Ok(read) = process.master.read(&mut chunk) {
        if read == 0 {
            break;
        }
        output.extend_from_slice(&chunk[..read]);
    }
    let _ = process.child.wait();
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("marker=w7:p3"), "{text:?}");
}

#[test]
fn a_bash_tool_call_carries_the_marker() {
    // End to end through the executor the agent loop drives.
    reporting();
    let call = ToolCallRequest {
        id: "c".to_string(),
        name: "bash".to_string(),
        arguments: serde_json::json!({ "command": PROBE }).to_string(),
    };
    let outcome = RealToolExecutor::new()
        .with_detach_helper(Some(helper()))
        .execute(&call, &CancelToken::new(), &mut |_| {});
    assert!(outcome.ok, "{}", outcome.output);
    assert!(
        outcome.output.contains("marker=w7:p3"),
        "{}",
        outcome.output
    );
}
