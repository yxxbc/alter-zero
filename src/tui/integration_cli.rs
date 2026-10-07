//! The `integration` subcommand boundary (`docs/herdr.md`): installing,
//! removing and reporting on an integration's `hooks.d/` file — and, under
//! `integration hook herdr`, the reporter those installed hooks run. Run by
//! [`super::startup::resolve_cli`] in cooked mode, before the tokio runtime
//! and the terminal, like every other CLI resolution (`docs/cli.md`).
//!
//! The decisions are the pure `herdr` module's — which pane, what an event
//! means, the request line, the installed file. This file owns what touches
//! the world: the files, the lock around the pane's state, the clock and
//! herdr's socket.
//!
//! The reporter runs **as a lifecycle hook**, which shapes all of it: it
//! prints nothing (a `SessionStart` or `UserPromptSubmit` hook's plain
//! stdout becomes model context), it always exits `0` (exit 2 is a block —
//! a `Stop` hook would force the turn on), and every failure is silent,
//! because herdr being unreachable is not something the agent should hear
//! about on every tool call (`docs/hooks.md`).

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use alter_zero::cli::{self, HelpPage, Integration, IntegrationCli};
use alter_zero::herdr;

use super::config;
use super::startup::stdout_style;

/// The most of a hook payload the reporter reads: every field it needs sits
/// in the envelope, and a `PostToolUse` payload carries the tool's output.
const PAYLOAD_MAX_BYTES: u64 = 1 << 20;

/// How long a read or a write on herdr's socket may take — herdr's own
/// integrations' timeout.
const SOCKET_TIMEOUT: Duration = Duration::from_millis(500);

/// How long the reporter waits for another one holding the pane's state —
/// the lead's hooks and each subagent's run on their own threads.
const LOCK_WAIT: Duration = Duration::from_secs(2);

/// Run one `integration` subcommand and return the process exit code: `0`
/// success (and always for `hook`), `1` when a file could not be written or
/// removed. Grammar errors never reach here — `parse` reports them and
/// `resolve_cli` exits 2.
pub(crate) fn run(cmd: &IntegrationCli) -> i32 {
    match cmd {
        IntegrationCli::Help => {
            println!("{}", cli::help(HelpPage::Integration, stdout_style()));
            0
        }
        IntegrationCli::Install(integration) => install(*integration),
        IntegrationCli::Uninstall(integration) => uninstall(*integration),
        IntegrationCli::Status => status(),
        IntegrationCli::Hook(name) => {
            // A name this build does not know — a newer build's file, a
            // typo — is ignored: the hook must never fail.
            if name == Integration::Herdr.name() {
                report_herdr();
            }
            0
        }
    }
}

/// `{config_home}/hooks.d/{name}.json` — `None` without a config home.
fn file_path(integration: Integration) -> Option<PathBuf> {
    config::hook_integrations_dir().map(|dir| dir.join(format!("{}.json", integration.name())))
}

/// What installing `integration` writes.
fn contents(integration: Integration) -> String {
    match integration {
        Integration::Herdr => herdr::hooks_file_json(),
    }
}

/// `~`-relative, for the messages.
fn shown(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    alter_zero::ui::display_cwd(path, home.as_deref())
}

fn install(integration: Integration) -> i32 {
    let name = integration.name();
    let Some(path) = file_path(integration) else {
        eprintln!(
            "Cannot install the {name} integration: no config home \
             (set HOME or ALTER_ZERO_CONFIG_DIR)"
        );
        return 1;
    };
    match write_whole(&path, &contents(integration)) {
        Ok(()) => {
            println!("Installed the {name} integration: {}", shown(&path));
            println!(
                "Its hooks run in every directory. Sessions already running pick it up when \
                 restarted."
            );
            0
        }
        Err(err) => {
            eprintln!("Failed to install the {name} integration: {err}");
            1
        }
    }
}

fn uninstall(integration: Integration) -> i32 {
    let name = integration.name();
    let Some(path) = file_path(integration) else {
        println!("The {name} integration is not installed (no config home).");
        return 0;
    };
    match std::fs::remove_file(&path) {
        Ok(()) => {
            println!("Removed the {name} integration: {}", shown(&path));
            0
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            println!("The {name} integration is not installed.");
            0
        }
        Err(err) => {
            eprintln!("Failed to remove {}: {err}", shown(&path));
            1
        }
    }
}

fn status() -> i32 {
    for &integration in Integration::ALL {
        let name = integration.name();
        let state = match file_path(integration) {
            None => "not installed (no config home)".to_string(),
            Some(path) => match std::fs::read_to_string(&path) {
                Ok(text) if text == contents(integration) => {
                    format!("installed — {}", shown(&path))
                }
                // Written by an older build, or edited by hand: this build's
                // events may differ from what it listens to.
                Ok(_) => format!(
                    "installed, but not this version's — {} (reinstall with: \
                     alter-zero integration install {name})",
                    shown(&path)
                ),
                Err(_) => "not installed".to_string(),
            },
        };
        println!("{name}: {state}");
        if integration == Integration::Herdr {
            let var = |key: &str| std::env::var(key).ok();
            let line = match herdr::Pane::from_env(var) {
                Some(pane) => format!("this terminal is herdr pane {}", pane.id),
                None if var("HERDR_ENV").as_deref() == Some("1") => {
                    "this terminal is inside herdr, in an Alter Zero agent's own terminal — the \
                     session that started it reports for the pane"
                        .to_string()
                }
                None => "this terminal is not a herdr pane".to_string(),
            };
            println!("       {line}");
        }
    }
    0
}

/// Write `contents` to `path` whole — a temp file beside it, then a rename —
/// so a session starting meanwhile never reads half an integration.
fn write_whole(path: &Path, contents: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
    let name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let temp = parent.join(format!(".{name}.tmp-{}", std::process::id()));
    std::fs::write(&temp, contents)
        .and_then(|()| std::fs::rename(&temp, path))
        .map_err(|err| {
            let _ = std::fs::remove_file(&temp);
            format!("cannot write {}: {err}", path.display())
        })
}

// ===== the reporter (docs/herdr.md) =====

/// One hook event, reported to the herdr pane this process runs in: read
/// the payload, step the pane's state under its lock, send what the step
/// says, write the state back. Silent whatever happens.
fn report_herdr() {
    let Some(pane) = herdr::Pane::from_env(|name| std::env::var(name).ok()) else {
        return;
    };
    let mut payload = String::new();
    if std::io::stdin()
        .take(PAYLOAD_MAX_BYTES)
        .read_to_string(&mut payload)
        .is_err()
    {
        return;
    }
    let Some(event) = herdr::Event::parse(&payload) else {
        return;
    };
    let Some(dir) = config::config_home().map(|home| home.join("herdr")) else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(pane.state_file_name());
    let Ok(mut file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
    else {
        return;
    };
    if !lock(&file) {
        return;
    }
    let mut text = String::new();
    let _ = file.read_to_string(&mut text);
    let mut state = herdr::State::parse(&text);
    let Some(report) = herdr::step(&mut state, &event) else {
        return;
    };
    let seq = herdr::next_seq(&mut state, now_ns());
    send(
        &pane.socket,
        &herdr::request_line(&pane, &report, seq, &program_name()),
    );
    if report == herdr::Report::Release {
        // The session is over: nothing to remember for this pane.
        let _ = std::fs::remove_file(&path);
    } else if file.set_len(0).is_ok() && file.seek(SeekFrom::Start(0)).is_ok() {
        let _ = file.write_all(state.to_json().as_bytes());
    }
}

/// Take the state file's lock, waiting at most [`LOCK_WAIT`] for another
/// reporter. A file system that cannot lock at all is reported unlocked —
/// better than reporting nothing.
fn lock(file: &std::fs::File) -> bool {
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) | Err(std::fs::TryLockError::Error(_)) => return true,
            Err(std::fs::TryLockError::WouldBlock) => {
                if started.elapsed() >= LOCK_WAIT {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

/// Nanoseconds since the epoch — the `seq` herdr compares across restarts.
fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
        })
}

/// The running binary's file name — the resume command's first word, which
/// herdr runs from `PATH`.
fn program_name() -> String {
    std::env::current_exe()
        .ok()
        .map(alter_zero::llm::hooks::running_binary)
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| env!("CARGO_PKG_NAME").to_string())
}

/// Send one request line and read herdr's one-line answer (ignored): one
/// connection per request, herdr's protocol. Reading the answer is what
/// keeps herdr from meeting a closed peer before it has replied.
#[cfg(unix)]
fn send(socket: &Path, line: &str) {
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(socket) else {
        return;
    };
    let _ = stream.set_write_timeout(Some(SOCKET_TIMEOUT));
    let _ = stream.set_read_timeout(Some(SOCKET_TIMEOUT));
    if stream.write_all(line.as_bytes()).is_err() {
        return;
    }
    let mut reply = [0u8; 4096];
    let _ = stream.read(&mut reply);
}

/// herdr's socket is a Unix socket: without them there is nothing to send
/// on, and the reporter stays silent like every other failure.
#[cfg(not(unix))]
fn send(_socket: &Path, _line: &str) {}
