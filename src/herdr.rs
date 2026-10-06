//! herdr pane-state reporting's pure half (`docs/herdr.md`).
//!
//! [herdr](https://herdr.dev) is a terminal multiplexer for coding agents: a
//! sidebar shows every pane's agent as blocked, working, done or idle. It
//! cannot recognise Alter Zero on its own, so Alter Zero reports its own
//! state over herdr's socket: `working` while a turn runs, `blocked` while a
//! modal waits on the user, `idle` when it is ready for input, the command
//! that reopens the conversation, and a release at quit.
//!
//! Everything here is pure except [`send`], the one socket write the
//! boundary's worker thread calls (the `telemetry::send_ping` pattern):
//!
//! - [`Pane::from_env`] — whether this process runs in a herdr pane at all;
//! - [`activity`] — what the session is doing, read off [`App`];
//! - [`Reporter`] — the derived state, the failure hold, the dedupe and the
//!   sequence number, producing the request lines;
//! - [`send`] — one request over the socket, every phase bounded.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use crate::agents::AgentRun;
use crate::app::App;
use crate::permission::{PermissionKind, PermissionRequest};

/// The off switch: `ALTER_ZERO_HERDR=0` (or `false`/`no`/`off`) keeps a
/// session inside a herdr pane from reporting. Also what a session's own
/// shells are given (`pty::spawn::TERMINAL_ENV`), so an Alter Zero started
/// inside one never reports as the pane's agent.
pub const HERDR_ENV: &str = "ALTER_ZERO_HERDR";

/// The name herdr shows for this agent, and the integration that reports
/// it. herdr reserves the `herdr:` prefix for its own integrations.
pub const AGENT: &str = "alter-zero";
/// See [`AGENT`].
pub const SOURCE: &str = "alter-zero";

/// The canonical program name — the resume command's first word when the
/// name this process was run as is not one herdr accepts ([`plain_command`]).
pub const DEFAULT_BIN: &str = "alter-zero";

/// The longest `message` a blocked report carries, in characters.
pub const MESSAGE_MAX_CHARS: usize = 80;

/// How long each phase of one request — the connect, the write, the first
/// byte of the reply — may take. herdr answers a local socket in
/// microseconds; this bounds a wedged one.
pub const SEND_TIMEOUT: Duration = Duration::from_millis(500);

/// How long quitting waits for the release to go out before giving up on it
/// (herdr clears an agent whose pane is back at its shell prompt on its own).
pub const RELEASE_WAIT: Duration = Duration::from_millis(500);

/// The herdr pane this process runs in: where to report, and as which pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    /// herdr's API socket (`HERDR_SOCKET_PATH`).
    pub socket: PathBuf,
    /// The pane's id (`HERDR_PANE_ID`), e.g. `w1:p1`.
    pub pane_id: String,
}

impl Pane {
    /// The pane, read through `var` (one environment variable by name):
    /// `None` unless herdr's own marker says this is a herdr pane
    /// (`HERDR_ENV=1`), the socket and the pane id are both set, and the
    /// [`HERDR_ENV`] off switch is not thrown.
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Option<Self> {
        if var("HERDR_ENV")?.trim() != "1" || var(HERDR_ENV).is_some_and(|value| is_off(&value)) {
            return None;
        }
        let socket = var("HERDR_SOCKET_PATH")?;
        let pane_id = var("HERDR_PANE_ID")?;
        let (socket, pane_id) = (socket.trim(), pane_id.trim());
        if socket.is_empty() || pane_id.is_empty() {
            return None;
        }
        Some(Self {
            socket: PathBuf::from(socket),
            pane_id: pane_id.to_string(),
        })
    }
}

/// `0`, `false`, `no` or `off`, in any case and padding — the grammar every
/// `ALTER_ZERO_*` on/off flag uses.
fn is_off(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

/// The states herdr understands. `done` is not one of them: herdr shows an
/// `idle` pane as done until the user has looked at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PaneState {
    /// Ready for input.
    Idle,
    /// A turn — or a subagent — is running.
    Working,
    /// Waiting on the user: a modal, or a turn that failed.
    Blocked,
}

/// What the session is doing, read off [`App`] by [`activity`] at the bottom
/// of every loop iteration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Activity {
    /// A turn is in flight: a user or queued turn, a `!` command, a
    /// summarization, a background notice's follow-up.
    pub turn: bool,
    /// A subagent is still working ([`agent_busy`]).
    pub agents: bool,
    /// What the open modal asks the user, when one waits on them.
    pub waiting: Option<String>,
    /// Which conversation this is — the history generation, which changes
    /// when one is replaced or rewound (`/clear`, `/resume`, a backtrack;
    /// [`App::history_generation`]). A failed turn's block lasts no longer
    /// than the conversation it ended.
    pub conversation: u64,
}

/// What `app` is doing, for herdr.
#[must_use]
pub fn activity(app: &App) -> Activity {
    Activity {
        turn: app.turn_active(),
        agents: app.agents().iter().any(agent_busy),
        waiting: waiting_message(app),
        conversation: app.history_generation(),
    }
}

/// What the open modal asks, as a report's one-line `message`: a permission
/// prompt's [`permission_message`], a question modal's first question.
fn waiting_message(app: &App) -> Option<String> {
    if let Some(prompt) = app.permission() {
        return Some(one_line(&permission_message(&prompt.request)));
    }
    let ask = app.ask()?;
    Some(one_line(&match ask.request.questions.first() {
        Some(first) => format!("Question: {}", first.question),
        None => "Question".to_string(),
    }))
}

/// Is this subagent still working — announced, running, or settled with a
/// follow-up turn queued to start?
#[must_use]
pub fn agent_busy(run: &AgentRun) -> bool {
    !run.status.is_final() || !run.followups.is_empty()
}

/// The `message` an open permission prompt reports: its title and what it
/// is about — the file's name rather than its whole path, the command, the
/// tool, the program a session input types into.
#[must_use]
pub fn permission_message(request: &PermissionRequest) -> String {
    let subject = match request.kind {
        PermissionKind::Write | PermissionKind::Edit => {
            crate::permission::file_name(&request.target)
        }
        PermissionKind::Session => request.detail.as_deref().unwrap_or(&request.target),
        PermissionKind::Bash | PermissionKind::Mcp => &request.target,
    };
    format!("{}: {subject}", crate::permission::title(request.kind))
}

/// `text` as one line of at most [`MESSAGE_MAX_CHARS`] characters: every run
/// of whitespace and control characters a single space, an over-long line cut
/// with a closing `…`.
#[must_use]
pub fn one_line(text: &str) -> String {
    let words = text
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|word| !word.is_empty());
    let line = words.collect::<Vec<_>>().join(" ");
    if line.chars().count() <= MESSAGE_MAX_CHARS {
        return line;
    }
    let kept: String = line.chars().take(MESSAGE_MAX_CHARS - 1).collect();
    format!("{}…", kept.trim_end())
}

/// Would herdr accept `name` as the first word of a resume command? It types
/// the command into the pane's shell, so it takes a bare name of
/// `[A-Za-z0-9_.-]` that is not an option — never a path.
#[must_use]
pub fn plain_command(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

/// herdr's ceiling on a resume command, in bytes over all its arguments.
const RESUME_ARGV_MAX_BYTES: usize = 8192;

/// The command that reopens this session after a herdr restart: `bin
/// --resume {session}` once the conversation has an id, else `bin` alone (a
/// fresh session comes back fresh). `bin` falls back to [`DEFAULT_BIN`], and
/// a session id herdr would refuse is left out.
#[must_use]
pub fn resume_argv(bin: &str, session: Option<&str>) -> Vec<String> {
    const RESUME: &str = "--resume";
    let bin = if plain_command(bin) { bin } else { DEFAULT_BIN };
    // herdr refuses an argument with an apostrophe (it quotes POSIX-style)
    // or a control character, and a command over its size limit.
    let session = session.filter(|id| {
        !id.is_empty()
            && !id.contains('\'')
            && !id.chars().any(char::is_control)
            && bin.len() + RESUME.len() + id.len() <= RESUME_ARGV_MAX_BYTES
    });
    let mut argv = vec![bin.to_string()];
    if let Some(id) = session {
        argv.extend([RESUME.to_string(), id.to_string()]);
    }
    argv
}

/// The sequence number after `last`: the wall clock in microseconds, or one
/// more than `last` when that is not higher. herdr ignores a report whose
/// number is not above the last one it accepted from this source on this
/// pane — remembered across runs — so the clock keeps a relaunch above its
/// predecessor and `last + 1` keeps a stalled or stepped-back clock moving.
#[must_use]
pub fn next_seq(last: u64, now_ms: u64) -> u64 {
    last.saturating_add(1).max(now_ms.saturating_mul(1000))
}

/// May a request be written to this socket? Only a socket, and only one this
/// process's own user owns — herdr's socket is readable by its owner alone.
#[must_use]
pub const fn socket_acceptable(is_socket: bool, owner: u32, me: u32) -> bool {
    is_socket && owner == me
}

/// One request line: the JSON object herdr reads up to the newline.
#[derive(Serialize)]
struct Request<'a, P> {
    id: String,
    method: &'a str,
    params: P,
}

/// `pane.report_agent`'s parameters. `message` rides a block only.
#[derive(Serialize)]
struct ReportParams<'a> {
    pane_id: &'a str,
    source: &'a str,
    agent: &'a str,
    state: PaneState,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'a str>,
    seq: u64,
    resume_argv: Vec<String>,
}

/// `pane.release_agent`'s parameters.
#[derive(Serialize)]
struct ReleaseParams<'a> {
    pane_id: &'a str,
    source: &'a str,
    agent: &'a str,
    seq: u64,
}

/// `method` with `params` as one newline-terminated line, its id naming the
/// integration and the request's own sequence number.
fn request_line(method: &str, seq: u64, params: impl Serialize) -> String {
    let request = Request {
        id: format!("{SOURCE}:{seq}"),
        method,
        params,
    };
    let mut line = serde_json::to_string(&request).unwrap_or_else(|_| "{}".to_string());
    line.push('\n');
    line
}

/// A turn that ended on a backend error: what the block says, and the
/// conversation it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Failure {
    message: String,
    conversation: u64,
}

/// The state, its message, and the resume command herdr is told.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sent {
    state: PaneState,
    message: Option<String>,
    session: Option<String>,
}

/// The report state machine: derives the state from an [`Activity`], holds a
/// failed turn as `blocked` until the next turn, and turns a change into a
/// request line with a fresh sequence number — nothing at all when herdr
/// already holds the same report.
#[derive(Debug, Clone)]
pub struct Reporter {
    pane_id: String,
    bin: String,
    failure: Option<Failure>,
    sent: Option<Sent>,
    seq: u64,
}

impl Reporter {
    /// A reporter for `pane_id`, naming `bin` in the resume command.
    #[must_use]
    pub fn new(pane_id: impl Into<String>, bin: impl Into<String>) -> Self {
        Self {
            pane_id: pane_id.into(),
            bin: bin.into(),
            failure: None,
            sent: None,
            seq: 0,
        }
    }

    /// The turn ended on a backend error in `conversation` (the history
    /// generation, [`Activity::conversation`]): report `blocked` with it until
    /// a turn starts again, or that conversation is replaced or rewound.
    pub fn fail(&mut self, error: &str, conversation: u64) {
        self.failure = Some(Failure {
            message: one_line(error),
            conversation,
        });
    }

    /// The request line to send for `activity` in the conversation `session`
    /// names, or `None` when herdr already holds exactly that report.
    pub fn observe(
        &mut self,
        activity: &Activity,
        session: Option<&str>,
        now_ms: u64,
    ) -> Option<String> {
        // Any turn starting means the user (or the queue) moved past the
        // failure: it is the next turn's to report now. So does the failed
        // turn leaving the screen with its conversation.
        let moved_on = activity.turn
            || self
                .failure
                .as_ref()
                .is_some_and(|failure| failure.conversation != activity.conversation);
        if moved_on {
            self.failure = None;
        }
        let (state, message) = self.desired(activity);
        let held = self.sent.as_ref().is_some_and(|sent| {
            sent.state == state
                && sent.message.as_deref() == message
                && sent.session.as_deref() == session
        });
        if held {
            return None;
        }
        let message = message.map(str::to_string);
        self.seq = next_seq(self.seq, now_ms);
        let line = request_line(
            "pane.report_agent",
            self.seq,
            ReportParams {
                pane_id: &self.pane_id,
                source: SOURCE,
                agent: AGENT,
                state,
                message: message.as_deref(),
                seq: self.seq,
                resume_argv: resume_argv(&self.bin, session),
            },
        );
        self.sent = Some(Sent {
            state,
            message,
            session: session.map(str::to_string),
        });
        Some(line)
    }

    /// The state `activity` puts the pane in, and the message that explains a
    /// block: a modal, then a turn, then a held failure, then a subagent.
    fn desired<'a>(&'a self, activity: &'a Activity) -> (PaneState, Option<&'a str>) {
        if let Some(waiting) = &activity.waiting {
            (PaneState::Blocked, Some(waiting))
        } else if activity.turn {
            (PaneState::Working, None)
        } else if let Some(failure) = &self.failure {
            (PaneState::Blocked, Some(&failure.message))
        } else if activity.agents {
            (PaneState::Working, None)
        } else {
            (PaneState::Idle, None)
        }
    }

    /// The request releasing the pane, sent as the last thing a quit does. Its
    /// sequence number is above every report's.
    pub fn release(&mut self, now_ms: u64) -> String {
        self.seq = next_seq(self.seq, now_ms);
        self.sent = None;
        request_line(
            "pane.release_agent",
            self.seq,
            ReleaseParams {
                pane_id: &self.pane_id,
                source: SOURCE,
                agent: AGENT,
                seq: self.seq,
            },
        )
    }
}

/// Send one request `line` to herdr's socket: connect, write it, and wait for
/// the first byte of the reply — or the close — with the write and the wait
/// each bounded by `timeout`. A local connect fails at once on a dead socket
/// and only waits while herdr's own backlog is full, which is why every
/// caller runs this off the event loop. Refuses a path that is not a socket
/// this user owns ([`socket_acceptable`]). The reply itself is not read: a
/// report is best-effort, and nothing herdr could answer changes what
/// happens next.
///
/// # Errors
///
/// The socket is refused, missing or unreachable, or the write fails.
#[cfg(unix)]
pub fn send(socket: &Path, line: &str, timeout: Duration) -> std::io::Result<()> {
    use std::io::{Read, Write};
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use std::os::unix::net::UnixStream;

    // No symlink is followed: what is checked is what is connected to.
    let metadata = std::fs::symlink_metadata(socket)?;
    let me = rustix::process::geteuid().as_raw();
    if !socket_acceptable(metadata.file_type().is_socket(), metadata.uid(), me) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "not a socket this user owns",
        ));
    }
    let mut stream = UnixStream::connect(socket)?;
    stream.set_write_timeout(Some(timeout))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.write_all(line.as_bytes())?;
    // The reply's first byte says herdr has read the line; its close or the
    // timeout end the wait just the same.
    let _ = stream.read(&mut [0_u8; 1]);
    Ok(())
}

/// See the Unix [`send`]: herdr's socket is a named pipe elsewhere, which
/// this does not speak.
///
/// # Errors
///
/// Always.
#[cfg(not(unix))]
pub fn send(socket: &Path, line: &str, timeout: Duration) -> std::io::Result<()> {
    let _ = (socket, line, timeout);
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "herdr's socket is a named pipe here",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentStatus;
    use crate::ask::{AskOption, AskQuestion, AskRequest};
    use crate::stream::AgentSpec;
    use serde_json::{Value, json};

    /// An environment of `pairs`, read the way the boundary reads the real
    /// one.
    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    /// What herdr exports into every pane.
    const IN_HERDR: [(&str, &str); 3] = [
        ("HERDR_ENV", "1"),
        ("HERDR_SOCKET_PATH", "/home/me/.config/herdr/herdr.sock"),
        ("HERDR_PANE_ID", "w1:p1"),
    ];

    fn in_herdr_with(extra: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
        let mut pairs = IN_HERDR.to_vec();
        pairs.extend_from_slice(extra);
        pairs
    }

    fn the_pane() -> Pane {
        Pane {
            socket: PathBuf::from("/home/me/.config/herdr/herdr.sock"),
            pane_id: "w1:p1".to_string(),
        }
    }

    // ===== whether this process reports at all =====

    #[test]
    fn a_herdr_pane_is_read_from_what_herdr_exports() {
        assert_eq!(Pane::from_env(env(&IN_HERDR)), Some(the_pane()));
    }

    #[test]
    fn whitespace_around_the_values_is_not_part_of_them() {
        let padded = [
            ("HERDR_ENV", " 1 "),
            ("HERDR_SOCKET_PATH", " /home/me/.config/herdr/herdr.sock\n"),
            ("HERDR_PANE_ID", " w1:p1 "),
        ];
        assert_eq!(Pane::from_env(env(&padded)), Some(the_pane()));
    }

    #[test]
    fn outside_herdr_there_is_no_pane() {
        assert_eq!(Pane::from_env(env(&[])), None);
        // A socket and a pane id without herdr's own marker: copied by hand,
        // or left over in an environment that is no longer herdr's.
        assert_eq!(Pane::from_env(env(&IN_HERDR[1..])), None);
        for marker in ["0", "", "true", "yes", "11"] {
            let pairs = [
                ("HERDR_ENV", marker),
                ("HERDR_SOCKET_PATH", "/s.sock"),
                ("HERDR_PANE_ID", "w1:p1"),
            ];
            assert_eq!(Pane::from_env(env(&pairs)), None, "HERDR_ENV={marker:?}");
        }
    }

    #[test]
    fn a_missing_or_blank_socket_or_pane_id_is_no_pane() {
        for (key, value) in [
            ("HERDR_SOCKET_PATH", ""),
            ("HERDR_SOCKET_PATH", "  "),
            ("HERDR_PANE_ID", ""),
            ("HERDR_PANE_ID", " \t"),
        ] {
            let pairs: Vec<_> = IN_HERDR
                .iter()
                .map(|&(k, v)| if k == key { (k, value) } else { (k, v) })
                .collect();
            assert_eq!(Pane::from_env(env(&pairs)), None, "{key}={value:?}");
        }
        let without_pane = [("HERDR_ENV", "1"), ("HERDR_SOCKET_PATH", "/s.sock")];
        assert_eq!(Pane::from_env(env(&without_pane)), None);
        let without_socket = [("HERDR_ENV", "1"), ("HERDR_PANE_ID", "w1:p1")];
        assert_eq!(Pane::from_env(env(&without_socket)), None);
    }

    #[test]
    fn the_off_switch_keeps_a_herdr_pane_from_reporting() {
        for off in ["0", "false", "no", "off", " OFF ", "False"] {
            let pairs = in_herdr_with(&[(HERDR_ENV, off)]);
            assert_eq!(Pane::from_env(env(&pairs)), None, "{HERDR_ENV}={off:?}");
        }
        // Every other value leaves it on — the `ALTER_ZERO_*` flag grammar.
        for on in ["1", "true", "yes", ""] {
            let pairs = in_herdr_with(&[(HERDR_ENV, on)]);
            assert_eq!(
                Pane::from_env(env(&pairs)),
                Some(the_pane()),
                "{HERDR_ENV}={on:?}"
            );
        }
    }

    #[test]
    fn an_alter_zero_started_in_a_session_shell_never_reports() {
        // A session's terminal is Alter Zero's own pseudo-terminal, not
        // herdr's pane, whatever the inherited HERDR_* variables say: an
        // Alter Zero the model starts there must not claim the pane.
        let (name, value) = crate::pty::spawn::TERMINAL_ENV
            .iter()
            .copied()
            .find(|&(name, _)| name == HERDR_ENV)
            .expect("a session shell's environment turns the report off");
        let nested = in_herdr_with(&[(name, value)]);
        assert_eq!(Pane::from_env(env(&nested)), None);
    }

    // ===== what the session is doing =====

    fn request(kind: PermissionKind, target: &str) -> PermissionRequest {
        PermissionRequest {
            id: "p1".to_string(),
            kind,
            target: target.to_string(),
            body: String::new(),
            detail: None,
            agent: None,
            agent_id: None,
        }
    }

    fn question(text: &str) -> AskQuestion {
        AskQuestion {
            question: text.to_string(),
            header: "Header".to_string(),
            options: vec![AskOption {
                label: "Yes".to_string(),
                description: "Yes it is".to_string(),
                preview: None,
            }],
            multi_select: false,
        }
    }

    #[test]
    fn a_fresh_session_is_doing_nothing() {
        assert_eq!(activity(&App::new()), Activity::default());
    }

    #[test]
    fn every_kind_of_turn_is_a_turn() {
        let mut reply = App::new();
        reply.begin_stream();
        assert!(activity(&reply).turn, "a reply");
        let mut shell = App::new();
        shell.begin_shell("cargo test");
        assert!(activity(&shell).turn, "a `!` command");
        let mut compact = App::new();
        compact.begin_compact(true);
        assert!(activity(&compact).turn, "a summarization");
    }

    #[test]
    fn a_turn_that_ended_is_no_longer_a_turn() {
        let mut app = App::new();
        app.begin_stream();
        app.fail_stream("boom");
        assert!(!activity(&app).turn);
    }

    #[test]
    fn an_open_permission_prompt_is_what_the_session_waits_on() {
        let mut app = App::new();
        app.open_permission(request(PermissionKind::Bash, "cargo publish"));
        assert_eq!(
            activity(&app).waiting.as_deref(),
            Some("Bash command: cargo publish")
        );
    }

    #[test]
    fn an_open_question_is_what_the_session_waits_on() {
        let mut app = App::new();
        app.open_ask(AskRequest {
            id: "q1".to_string(),
            questions: vec![
                question("Which database should the service use?"),
                question("And which cache?"),
            ],
        });
        assert_eq!(
            activity(&app).waiting.as_deref(),
            Some("Question: Which database should the service use?")
        );
    }

    #[test]
    fn a_subagent_on_the_roster_is_work_in_flight() {
        let mut app = App::new();
        app.begin_stream();
        app.start_agent_group(
            true,
            &[AgentSpec {
                id: "a1".to_string(),
                description: "Survey the repo".to_string(),
                agent_type: crate::agents::GENERAL_PURPOSE.to_string(),
                prompt: "Look around".to_string(),
                background: true,
                call_id: None,
                arguments: None,
            }],
        );
        assert!(activity(&app).agents);
    }

    #[test]
    fn an_agent_is_busy_until_it_settles_with_nothing_queued() {
        let mut run = AgentRun::new("a1", "Survey", "general-purpose", "Look around", true);
        assert!(agent_busy(&run), "announced, about to run");
        run.status = AgentStatus::Running;
        assert!(agent_busy(&run));
        for settled in [
            AgentStatus::Done,
            AgentStatus::Failed,
            AgentStatus::Interrupted,
        ] {
            run.status = settled;
            assert!(!agent_busy(&run), "{settled:?}");
        }
        run.status = AgentStatus::Done;
        run.queue_followup("and now the docs");
        assert!(
            agent_busy(&run),
            "settled with a follow-up queued: it is about to run again"
        );
    }

    #[test]
    fn a_permission_message_names_what_it_asks_about() {
        assert_eq!(
            permission_message(&request(PermissionKind::Bash, "cargo publish")),
            "Bash command: cargo publish"
        );
        // A file is named, not its whole path: the name is what an 80-column
        // message has room for, and what the cut would lose first.
        assert_eq!(
            permission_message(&request(PermissionKind::Edit, "/home/me/repo/src/main.rs")),
            "Edit file: main.rs"
        );
        assert_eq!(
            permission_message(&request(PermissionKind::Write, "/tmp/new.py")),
            "Create file: new.py"
        );
        assert_eq!(
            permission_message(&request(PermissionKind::Mcp, "mcp__deepwiki__ask_question")),
            "Tool use: mcp__deepwiki__ask_question"
        );
        // A session's target is its id; the program it types into says more.
        let mut input = request(PermissionKind::Session, "4f2a");
        assert_eq!(permission_message(&input), "Session input: 4f2a");
        input.detail = Some("python3 -i".to_string());
        assert_eq!(permission_message(&input), "Session input: python3 -i");
    }

    #[test]
    fn a_message_is_one_short_line() {
        assert_eq!(one_line("  two\n\tlines  "), "two lines");
        assert_eq!(
            one_line("bell\u{7}and\u{1b}[0mescape"),
            "bell and [0mescape"
        );
        let exact = "x".repeat(MESSAGE_MAX_CHARS);
        assert_eq!(one_line(&exact), exact, "exactly at the cap is uncut");
        let cut = one_line(&"x".repeat(MESSAGE_MAX_CHARS + 1));
        assert_eq!(cut.chars().count(), MESSAGE_MAX_CHARS);
        assert!(cut.ends_with('…'), "{cut}");
        // Counted in characters, never bytes: a cut can't split one.
        let wide = one_line(&"é".repeat(MESSAGE_MAX_CHARS * 2));
        assert_eq!(wide.chars().count(), MESSAGE_MAX_CHARS);
    }

    // ===== the resume command =====

    #[test]
    fn plain_command_is_herdrs_own_rule() {
        for ok in ["alter-zero", "az", "alter_zero.2", "A-Z9"] {
            assert!(plain_command(ok), "{ok:?}");
        }
        for refused in [
            "",
            "-v",
            "alter zero",
            "./alter-zero",
            "/usr/bin/alter-zero",
            "alter'zero",
            "ältér",
        ] {
            assert!(!plain_command(refused), "{refused:?}");
        }
    }

    #[test]
    fn the_resume_command_reopens_the_conversation() {
        assert_eq!(
            resume_argv("alter-zero", Some("18a9f2c3-1a2b")),
            ["alter-zero", "--resume", "18a9f2c3-1a2b"]
        );
        // Before the first message there is nothing to resume: the pane comes
        // back as a fresh session, which is what it was.
        assert_eq!(resume_argv("alter-zero", None), ["alter-zero"]);
        // A renamed install keeps the name it was run as.
        assert_eq!(resume_argv("az", Some("abc")), ["az", "--resume", "abc"]);
    }

    #[test]
    fn a_name_herdr_would_refuse_falls_back_to_the_canonical_one() {
        for bin in ["", "-x", "my agent", "./alter-zero", "älter"] {
            assert_eq!(
                resume_argv(bin, Some("abc")),
                ["alter-zero", "--resume", "abc"],
                "{bin:?}"
            );
        }
    }

    #[test]
    fn a_session_id_herdr_would_refuse_is_left_out() {
        for id in ["", "it's", "a\nb", "a\u{7}b"] {
            assert_eq!(
                resume_argv("alter-zero", Some(id)),
                ["alter-zero"],
                "{id:?}"
            );
        }
        let huge = "a".repeat(9000);
        assert_eq!(resume_argv("alter-zero", Some(&huge)), ["alter-zero"]);
    }

    // ===== the sequence =====

    /// A wall clock, in milliseconds since the epoch.
    const T: u64 = 1_759_800_000_000;

    #[test]
    fn the_sequence_is_the_clock_in_microseconds_and_never_repeats() {
        assert_eq!(next_seq(0, T), T * 1000);
        assert_eq!(next_seq(T * 1000, T), T * 1000 + 1, "same millisecond");
        assert_eq!(
            next_seq(T * 1000 + 5, T - 60_000),
            T * 1000 + 6,
            "a clock stepping back cannot take it below the last"
        );
        assert_eq!(next_seq(0, u64::MAX), u64::MAX, "saturates, never wraps");
        assert_eq!(next_seq(u64::MAX, T), u64::MAX, "saturates, never wraps");
    }

    // ===== the reports =====

    fn reporter() -> Reporter {
        Reporter::new("w1:p1", "alter-zero")
    }

    fn idle() -> Activity {
        Activity::default()
    }

    fn turn() -> Activity {
        Activity {
            turn: true,
            ..Activity::default()
        }
    }

    fn waiting(message: &str) -> Activity {
        Activity {
            waiting: Some(message.to_string()),
            ..Activity::default()
        }
    }

    fn agents() -> Activity {
        Activity {
            agents: true,
            ..Activity::default()
        }
    }

    /// The request a line carries, parsed back.
    fn parsed(line: &str) -> Value {
        assert!(line.ends_with('\n'), "a request is a line: {line:?}");
        assert_eq!(line.matches('\n').count(), 1, "exactly one: {line:?}");
        serde_json::from_str(line).expect("the line is JSON")
    }

    fn state(line: &str) -> String {
        parsed(line)["params"]["state"]
            .as_str()
            .expect("a state")
            .to_string()
    }

    fn seq(line: &str) -> u64 {
        parsed(line)["params"]["seq"].as_u64().expect("a seq")
    }

    #[test]
    fn the_first_observation_claims_the_pane() {
        let line = reporter()
            .observe(&idle(), None, T)
            .expect("a first report");
        assert_eq!(
            parsed(&line),
            json!({
                "id": format!("alter-zero:{}", T * 1000),
                "method": "pane.report_agent",
                "params": {
                    "pane_id": "w1:p1",
                    "source": "alter-zero",
                    "agent": "alter-zero",
                    "state": "idle",
                    "seq": T * 1000,
                    "resume_argv": ["alter-zero"],
                },
            })
        );
    }

    #[test]
    fn nothing_is_sent_while_nothing_changes() {
        let mut reporter = reporter();
        assert!(reporter.observe(&idle(), None, T).is_some());
        assert_eq!(reporter.observe(&idle(), None, T + 1), None);
        assert_eq!(reporter.observe(&idle(), None, T + 2), None);
    }

    #[test]
    fn a_turn_is_working_and_its_end_is_idle() {
        let mut reporter = reporter();
        reporter.observe(&idle(), None, T);
        let started = reporter.observe(&turn(), None, T + 1).expect("a start");
        assert_eq!(state(&started), "working");
        assert_eq!(
            reporter.observe(&turn(), None, T + 2),
            None,
            "still working"
        );
        let ended = reporter.observe(&idle(), None, T + 3).expect("an end");
        assert_eq!(state(&ended), "idle");
    }

    #[test]
    fn a_modal_is_blocked_and_says_what_it_asks() {
        let mut reporter = reporter();
        let line = reporter
            .observe(&waiting("Bash command: cargo publish"), None, T)
            .expect("a block");
        assert_eq!(state(&line), "blocked");
        assert_eq!(
            parsed(&line)["params"]["message"],
            "Bash command: cargo publish"
        );
        // The next prompt in a batch is news even in the same state.
        let next = reporter
            .observe(&waiting("Edit file: main.rs"), None, T + 1)
            .expect("a different question");
        assert_eq!(parsed(&next)["params"]["message"], "Edit file: main.rs");
        // Answered: back to the turn, no message.
        let resumed = reporter.observe(&turn(), None, T + 2).expect("answered");
        assert_eq!(state(&resumed), "working");
        assert!(parsed(&resumed)["params"].get("message").is_none());
    }

    #[test]
    fn a_running_subagent_keeps_the_pane_working() {
        let mut reporter = reporter();
        reporter.observe(&turn(), None, T);
        assert_eq!(
            reporter.observe(&agents(), None, T + 1),
            None,
            "the lead's turn ended but a subagent still works: no change"
        );
        assert_eq!(
            state(&reporter.observe(&idle(), None, T + 2).unwrap()),
            "idle"
        );
    }

    #[test]
    fn a_failed_turn_holds_blocked_until_the_next_turn_starts() {
        let mut reporter = reporter();
        reporter.observe(&turn(), None, T);
        reporter.fail("API error 429: rate limited\n  try again later", 0);
        let line = reporter.observe(&idle(), None, T + 1).expect("the failure");
        assert_eq!(state(&line), "blocked");
        assert_eq!(
            parsed(&line)["params"]["message"],
            "API error 429: rate limited try again later"
        );
        assert_eq!(
            reporter.observe(&idle(), None, T + 2),
            None,
            "held, not resent"
        );
        let retried = reporter.observe(&turn(), None, T + 3).expect("a new turn");
        assert_eq!(state(&retried), "working");
        let done = reporter.observe(&idle(), None, T + 4).expect("its end");
        assert_eq!(
            state(&done),
            "idle",
            "the failure went with the turn that cleared it"
        );
    }

    #[test]
    fn a_failure_goes_with_the_conversation_it_ended() {
        // `/clear`, `/resume` and a backtrack each replace or rewind the
        // conversation (a new history generation): the failed turn is no
        // longer on screen, so neither is the block.
        let mut reporter = reporter();
        reporter.fail("boom", 3);
        let same = Activity {
            conversation: 3,
            ..idle()
        };
        assert_eq!(state(&reporter.observe(&same, None, T).unwrap()), "blocked");
        let replaced = Activity {
            conversation: 4,
            ..idle()
        };
        assert_eq!(
            state(&reporter.observe(&replaced, None, T + 1).unwrap()),
            "idle"
        );
        assert_eq!(
            reporter.observe(&same, None, T + 2),
            None,
            "still idle: the failure is gone for good, not hidden while the generation differs"
        );
    }

    #[test]
    fn the_activity_names_the_conversation() {
        let mut app = App::new();
        let before = activity(&app).conversation;
        app.begin_stream();
        app.fail_stream("boom");
        assert_eq!(activity(&app).conversation, before, "a failure appends");
        app.load_session(Vec::new());
        assert_ne!(activity(&app).conversation, before, "/resume replaces");
    }

    #[test]
    fn the_states_rank_modal_turn_failure_agents_idle() {
        let mut reporter = reporter();
        reporter.fail("boom", 0);
        let everything = Activity {
            turn: true,
            agents: true,
            waiting: Some("Question: which?".to_string()),
            conversation: 0,
        };
        let line = reporter.observe(&everything, None, T).unwrap();
        assert_eq!(
            (state(&line), parsed(&line)["params"]["message"].clone()),
            ("blocked".to_string(), json!("Question: which?")),
            "a modal outranks everything"
        );
        // A modal still open over a failed turn's hold names the modal.
        reporter.fail("boom", 0);
        let modal = reporter.observe(&waiting("Edit file: a.rs"), None, T + 1);
        assert_eq!(
            parsed(&modal.unwrap())["params"]["message"],
            "Edit file: a.rs"
        );
        // The failure outranks a subagent still running beside it.
        let line = reporter.observe(&agents(), None, T + 2).unwrap();
        assert_eq!(
            (state(&line), parsed(&line)["params"]["message"].clone()),
            ("blocked".to_string(), json!("boom"))
        );
    }

    #[test]
    fn the_resume_command_follows_the_conversation() {
        let mut reporter = reporter();
        let fresh = reporter.observe(&idle(), None, T).unwrap();
        assert_eq!(
            parsed(&fresh)["params"]["resume_argv"],
            json!(["alter-zero"])
        );
        let named = reporter
            .observe(&idle(), Some("18a9f2c3-1a2b"), T + 1)
            .expect("the first message gave the conversation an id: news");
        assert_eq!(
            parsed(&named)["params"]["resume_argv"],
            json!(["alter-zero", "--resume", "18a9f2c3-1a2b"])
        );
        assert_eq!(
            reporter.observe(&idle(), Some("18a9f2c3-1a2b"), T + 2),
            None
        );
        let cleared = reporter
            .observe(&idle(), None, T + 3)
            .expect("/clear: the next restore is a fresh launch");
        assert_eq!(
            parsed(&cleared)["params"]["resume_argv"],
            json!(["alter-zero"])
        );
        let resumed = reporter
            .observe(&idle(), Some("0c1d"), T + 4)
            .expect("/resume switched conversations");
        assert_eq!(
            parsed(&resumed)["params"]["resume_argv"],
            json!(["alter-zero", "--resume", "0c1d"])
        );
    }

    #[test]
    fn every_request_carries_a_higher_sequence_than_the_last() {
        let mut reporter = reporter();
        let first = reporter.observe(&idle(), None, T).unwrap();
        let same_ms = reporter.observe(&turn(), None, T).unwrap();
        let clock_back = reporter.observe(&idle(), None, T - 10_000).unwrap();
        let release = reporter.release(T - 20_000);
        let seqs = [seq(&first), seq(&same_ms), seq(&clock_back), seq(&release)];
        assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]), "{seqs:?}");
    }

    #[test]
    fn the_release_names_the_pane_and_the_integration() {
        let mut reporter = reporter();
        reporter.observe(&idle(), None, T);
        let line = reporter.release(T + 1);
        assert_eq!(
            parsed(&line),
            json!({
                "id": format!("alter-zero:{}", (T + 1) * 1000),
                "method": "pane.release_agent",
                "params": {
                    "pane_id": "w1:p1",
                    "source": "alter-zero",
                    "agent": "alter-zero",
                    "seq": (T + 1) * 1000,
                },
            })
        );
    }

    #[test]
    fn a_request_line_escapes_what_the_user_typed() {
        // A message with quotes, a backslash and a newline is still one line
        // of valid JSON — herdr reads up to the first newline.
        let mut reporter = reporter();
        let line = reporter
            .observe(&waiting("Bash command: echo \"a\\b\"\nrm"), None, T)
            .unwrap();
        assert_eq!(
            parsed(&line)["params"]["message"],
            "Bash command: echo \"a\\b\"\nrm"
        );
    }

    // ===== the socket =====

    #[test]
    fn only_a_socket_this_user_owns_is_written_to() {
        assert!(socket_acceptable(true, 1000, 1000));
        assert!(!socket_acceptable(false, 1000, 1000), "not a socket");
        assert!(!socket_acceptable(true, 0, 1000), "someone else's");
    }

    #[cfg(unix)]
    mod send {
        use super::super::*;
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        use std::time::Instant;

        /// A fresh directory for one test's socket, removed when dropped — a
        /// failing assertion included. Unix socket paths are short (108
        /// bytes), so it sits directly in the temp dir.
        struct Scratch(PathBuf);

        impl Scratch {
            fn new(tag: &str) -> Self {
                static NEXT: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(0);
                let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let dir = std::env::temp_dir()
                    .join(format!("alter-zero-herdr-{tag}-{}-{n}", std::process::id()));
                std::fs::create_dir_all(&dir).expect("scratch dir");
                Self(dir)
            }

            fn socket(&self) -> PathBuf {
                self.0.join("herdr.sock")
            }
        }

        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        #[test]
        fn a_request_is_one_line_answered_by_herdr() {
            let scratch = Scratch::new("reply");
            let socket = scratch.socket();
            let listener = UnixListener::bind(&socket).expect("bind");
            let server = std::thread::spawn(move || {
                let (stream, _) = listener.accept().expect("accept");
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).expect("read");
                reader
                    .get_mut()
                    .write_all(b"{\"id\":\"x\",\"result\":{\"type\":\"ok\"}}\n")
                    .expect("reply");
                line
            });
            let request = "{\"id\":\"alter-zero:1\",\"method\":\"ping\",\"params\":{}}\n";
            send(&socket, request, SEND_TIMEOUT).expect("sent");
            assert_eq!(server.join().expect("server"), request);
        }

        #[test]
        fn a_silent_herdr_costs_at_most_the_timeout() {
            let scratch = Scratch::new("silent");
            let socket = scratch.socket();
            let listener = UnixListener::bind(&socket).expect("bind");
            let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
            let server = std::thread::spawn(move || {
                // Accept, never answer, and hold the connection open until
                // the test is done with it.
                let (_stream, _) = listener.accept().expect("accept");
                let _ = hold_rx.recv();
            });
            let started = Instant::now();
            send(&socket, "{}\n", Duration::from_millis(100)).expect("written");
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "the wait for a reply is bounded: {:?}",
                started.elapsed()
            );
            let _ = hold_tx.send(());
            server.join().expect("server");
        }

        #[test]
        fn no_socket_is_an_error_not_a_hang() {
            let scratch = Scratch::new("missing");
            assert!(send(&scratch.socket(), "{}\n", SEND_TIMEOUT).is_err());
        }

        #[test]
        fn a_regular_file_in_the_sockets_place_is_never_written() {
            let scratch = Scratch::new("file");
            let socket = scratch.socket();
            std::fs::write(&socket, "").expect("a plain file");
            assert!(send(&socket, "{}\n", SEND_TIMEOUT).is_err());
            assert_eq!(std::fs::read(&socket).expect("still there"), b"");
        }
    }
}
