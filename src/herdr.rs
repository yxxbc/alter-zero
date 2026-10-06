//! herdr support — telling the herdr terminal multiplexer what this session is
//! doing (`docs/herdr.md`).
//!
//! [herdr](https://herdr.dev) runs coding agents in panes and keeps a sidebar
//! row per pane saying whether its agent is **working**, **blocked** on the
//! user, **done** or **idle**, notifying on the transitions. It recognises the
//! agents it knows by their screens; alter-zero is not one of them, and an
//! agent herdr does not know shows up as a plain terminal. So the session
//! reports its own state, natively, the way herdr's *Add Herdr support to your
//! agent* guide describes: one newline-terminated JSON request per connection
//! to the socket herdr names in the pane's environment. Nothing to install,
//! nothing to configure, and nothing at all outside herdr.
//!
//! This module is the whole of it but the wiring:
//!
//! - [`Pane::from_env`] decides whether this process reports at all — inside a
//!   herdr pane, not switched off, not nested under another reporting session.
//! - [`status_of`] reads what the session is doing off [`App`]: **blocked**
//!   while a permission prompt or a question waits on the user, else
//!   **working** while a turn or a background agent runs, else **idle**.
//! - [`Tracker`] turns the loop's per-iteration reading into a report only when
//!   something changed, and [`resume_argv`] adds the command that reopens the
//!   conversation, so herdr can bring the session back after it restarts.
//! - [`report_line`]/[`release_line`] are the two requests, [`send`] delivers
//!   one, and [`Reporter`] is the worker thread that sends them in order —
//!   a backlog collapsed to its newest, the release last — so the event loop
//!   never waits on herdr.
//!
//! The boundary (`src/tui/herdr.rs`) calls [`start`] once at startup, feeds
//! the [`Tracker`] at the loop bottom, and releases the pane on the way out.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::app::{App, PathDisplay};
use crate::ask::AskRequest;
use crate::permission::{self, PermissionKind, PermissionRequest};

/// `1` in every process herdr starts in a pane.
pub const HERDR_ENV: &str = "HERDR_ENV";
/// The pane's public id (`w1:p2`) — what every report names.
pub const PANE_ID_ENV: &str = "HERDR_PANE_ID";
/// herdr's API socket — where every report goes.
pub const SOCKET_PATH_ENV: &str = "HERDR_SOCKET_PATH";
/// The off switch: `0`/`false`/`no`/`off` stops the session reporting even
/// inside herdr — the one grammar every `ALTER_ZERO_*` on/off flag uses.
pub const TOGGLE_ENV: &str = "ALTER_ZERO_HERDR";
/// The nested-session marker: set on every shell child of a reporting session
/// to the pane it reports for ([`stamp_child`]), so an alter-zero started
/// under it — by the `bash` tool, say — sees a parent already speaking for
/// that pane and stays quiet ([`Pane::from_env`]).
pub const REPORTER_ENV: &str = "ALTER_ZERO_HERDR_REPORTER";

/// Who is reporting: herdr namespaces reports by source, so several reporters
/// can coexist in one pane. Stable and unique, and clear of the `herdr:`
/// prefix herdr keeps for its own integrations.
pub const SOURCE: &str = "alter-zero";
/// The agent label the sidebar shows — the command's name.
pub const AGENT: &str = "alter-zero";

/// The longest blocked message sent, in characters — herdr's cap on display
/// text elsewhere, and about what a sidebar row can show.
pub const MESSAGE_MAX_CHARS: usize = 80;
/// How long one request may take to write, and its answer to arrive. A local
/// socket answers in milliseconds; this only bounds a wedged server.
pub const IO_TIMEOUT: Duration = Duration::from_millis(500);
/// How long the quit waits for the release to go out — enough for a report in
/// flight and then the release, each bounded by [`IO_TIMEOUT`].
pub const RELEASE_WAIT: Duration = Duration::from_secs(1);
/// herdr's limits on a resume command: at most this many arguments…
pub const MAX_RESUME_ARGS: usize = 64;
/// …totalling at most this many bytes.
pub const MAX_RESUME_ARGV_BYTES: usize = 8192;
/// The most of herdr's one-line answer [`send`] reads.
const ANSWER_MAX_BYTES: u64 = 64 * 1024;

/// The herdr pane this process reports for, read from the environment herdr
/// gives every process it starts in one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    /// `HERDR_PANE_ID`.
    pub id: String,
    /// `HERDR_SOCKET_PATH`.
    pub socket: PathBuf,
}

impl Pane {
    /// The pane to report for, through `var` (the environment, injected):
    /// `None` outside herdr — herdr's own rule is to report only when
    /// `HERDR_ENV=1` and the pane id and socket are both set — when
    /// [`TOGGLE_ENV`] switches reporting off, and when [`REPORTER_ENV`] says a
    /// parent alter-zero already reports for this very pane.
    #[must_use]
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Option<Self> {
        if var(HERDR_ENV).as_deref() != Some("1") {
            return None;
        }
        let set = |name: &str| var(name).filter(|value| !value.trim().is_empty());
        let id = set(PANE_ID_ENV)?;
        let socket = set(SOCKET_PATH_ENV)?;
        if var(TOGGLE_ENV).is_some_and(|value| is_falsy(&value)) {
            return None;
        }
        if var(REPORTER_ENV).is_some_and(|marker| marker == id) {
            return None;
        }
        Some(Self {
            id,
            socket: PathBuf::from(socket),
        })
    }
}

/// The off-switch grammar: `0`/`false`/`no`/`off`, case- and
/// whitespace-insensitive.
fn is_falsy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

/// A pane's agent state, in herdr's words. herdr adds **done** itself — an
/// `idle` the user has not looked at since the agent was last busy — so it is
/// never reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Ready for input.
    Idle,
    /// A turn is running.
    Working,
    /// Waiting on a decision only the user can make.
    Blocked,
}

impl State {
    /// The wire name: `idle`, `working`, `blocked`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
        }
    }
}

/// What the session is doing: the [`State`], and for a blocked one, what it
/// is waiting for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    /// Why a [`State::Blocked`] session waits — the prompt's own title and
    /// subject (`Bash command: cargo test`). `None` otherwise.
    pub message: Option<String>,
}

impl Status {
    const fn of(state: State) -> Self {
        Self {
            state,
            message: None,
        }
    }

    const fn blocked(message: String) -> Self {
        Self {
            state: State::Blocked,
            message: Some(message),
        }
    }
}

/// What `app` is doing, for herdr:
///
/// - **blocked** while a thread is parked on the user — a permission prompt
///   or an `AskUserQuestion` modal, the main turn's or a background agent's
///   (`App::modal_open`'s two halves, read here for the message);
/// - else **working** while a turn is in flight — a reply, a `!` command, a
///   compaction — **or a background agent is still running**: its completion
///   starts a follow-up turn on its own, so calling the pane idle in between
///   would have herdr announce it done before it is. A background *shell* does
///   not count; it may be a server that runs for the rest of the day;
/// - else **idle**.
#[must_use]
pub fn status_of(app: &App) -> Status {
    if let Some(prompt) = app.permission() {
        return Status::blocked(permission_message(&prompt.request, app.path_display()));
    }
    if let Some(prompt) = app.ask() {
        return Status::blocked(ask_message(&prompt.request));
    }
    let agent_running = app.agents().iter().any(|run| !run.status.is_final());
    if app.turn_active() || agent_running {
        Status::of(State::Working)
    } else {
        Status::of(State::Idle)
    }
}

/// The blocked message for a permission prompt: its title and what it is
/// about — the file as the screen shows it (`paths`), the command, the MCP
/// tool's display name, the program a session input goes to.
#[must_use]
pub fn permission_message(request: &PermissionRequest, paths: &PathDisplay) -> String {
    let subject = match request.kind {
        PermissionKind::Write | PermissionKind::Edit => paths.display(&request.target),
        PermissionKind::Bash => request.target.clone(),
        PermissionKind::Mcp => crate::mcp::display_from_wire(&request.target).map_or_else(
            || request.target.clone(),
            |name| crate::mcp::capitalize_display(&name),
        ),
        PermissionKind::Session => request
            .detail
            .clone()
            .unwrap_or_else(|| request.target.clone()),
    };
    clip(&format!("{}: {subject}", permission::title(request.kind)))
}

/// The blocked message for an `AskUserQuestion` call: its first question, and
/// how many there are when there are several.
#[must_use]
pub fn ask_message(request: &AskRequest) -> String {
    let first = request
        .questions
        .first()
        .map_or("", |q| q.question.as_str());
    let text = match request.questions.len() {
        0 | 1 => format!("Question: {first}"),
        count => format!("{count} questions: {first}"),
    };
    clip(&text)
}

/// `text` as one line of at most [`MESSAGE_MAX_CHARS`] characters: its first
/// line, closed by ` …` when more followed, control characters blanked, and
/// cut with `…` at the cap.
fn clip(text: &str) -> String {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or_default().trim_end();
    let more = lines.any(|line| !line.trim().is_empty());
    let mut line: String = first
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if more {
        line.push_str(" …");
    }
    if line.chars().count() > MESSAGE_MAX_CHARS {
        line = line.chars().take(MESSAGE_MAX_CHARS - 1).collect();
        line.push('…');
    }
    line
}

/// One state report: what the session is doing, and the command that resumes
/// its conversation when there is one herdr would accept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub status: Status,
    /// `[bin, "--resume", id]` — herdr runs it in a fresh pane to bring the
    /// session back after a herdr restart ([`resume_argv`]).
    pub resume: Option<Vec<String>>,
}

/// The loop bottom's change check: remembers the last reading reported, so an
/// iteration that changed nothing herdr can see sends nothing.
#[derive(Debug, Default)]
pub struct Tracker {
    last: Option<(Status, Option<String>)>,
}

impl Tracker {
    /// The report to send for this iteration's `status` and recorded
    /// `session` id, or `None` when both match the last report. The resume
    /// command is built from `resume_bin` ([`resume_bin`]) — fixed for the
    /// process, so not part of what counts as a change. A session id appears
    /// once the conversation records something, and changes with a `/clear`
    /// or a `/resume`; each is a new report, carrying the new command.
    pub fn next(
        &mut self,
        status: Status,
        session: Option<&str>,
        resume_bin: Option<&str>,
    ) -> Option<Report> {
        if let Some((last_status, last_session)) = &self.last
            && *last_status == status
            && last_session.as_deref() == session
        {
            return None;
        }
        let resume = session
            .zip(resume_bin)
            .and_then(|(id, bin)| resume_argv(bin, id));
        self.last = Some((status.clone(), session.map(str::to_string)));
        Some(Report { status, resume })
    }
}

/// The `seq` every report and release carries. herdr keeps the highest number
/// it has accepted from a source for as long as the pane lives — across this
/// process's restarts in it — and ignores anything not strictly higher, so
/// late or out-of-order reports cannot overwrite newer ones. A clock is what
/// makes a restarted session start above the last one; this one is
/// nanoseconds since the epoch (herdr's own hooks' scale — a scale once used
/// must never shrink), bumped by one when the clock has not moved on.
#[derive(Debug, Default)]
pub struct SeqClock {
    last: u64,
}

impl SeqClock {
    /// The next number: `now`, or one more than the last when `now` is not
    /// past it.
    pub fn next(&mut self, now: u64) -> u64 {
        let seq = now.max(self.last.saturating_add(1));
        self.last = seq;
        seq
    }
}

/// `now` in nanoseconds since the Unix epoch — `0` before it, saturating far
/// after it.
#[must_use]
pub fn unix_nanos(now: SystemTime) -> u64 {
    now.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
        })
}

/// herdr's own check on a resume command, mirrored so a command it would
/// refuse is never sent (a refused one costs the whole report): not empty, at
/// most [`MAX_RESUME_ARGS`] arguments and [`MAX_RESUME_ARGV_BYTES`] bytes, no
/// control characters or apostrophes, and a first word that is a plain command
/// name rather than a path.
#[must_use]
pub fn valid_resume_argv(argv: &[String]) -> bool {
    let Some(command) = argv.first() else {
        return false;
    };
    argv.len() <= MAX_RESUME_ARGS
        && argv.iter().map(String::len).sum::<usize>() <= MAX_RESUME_ARGV_BYTES
        && !argv
            .iter()
            .any(|arg| arg.contains('\'') || arg.chars().any(char::is_control))
        && is_plain_command(command)
}

/// A command name herdr accepts as the first word: letters, digits, `_`, `-`
/// and `.`, not starting with `-`.
fn is_plain_command(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// The command that reopens recorded session `session_id` —
/// `{bin} --resume {id}`, the exit hint's own (`cli::resume_hint`) — or `None`
/// when herdr would refuse it ([`valid_resume_argv`]).
#[must_use]
pub fn resume_argv(bin: &str, session_id: &str) -> Option<Vec<String>> {
    let argv = vec![
        bin.to_string(),
        "--resume".to_string(),
        session_id.to_string(),
    ];
    valid_resume_argv(&argv).then_some(argv)
}

/// The name a resume command starts with: the one this binary was invoked as
/// (`cli::bin_name` over `arg0`), when it is a plain command name **and** an
/// absolute `path` entry holds an executable by that name (`is_executable`,
/// injected). herdr restores a session by typing the command into a fresh
/// shell in the pane, so a binary run from a checkout (`./target/release/…`)
/// — a name that shell has never heard of — gets no resume command rather
/// than one that restores as "command not found". Relative and empty `PATH`
/// entries are skipped: they would be resolved against a directory that is
/// not this one by then.
#[must_use]
pub fn resume_bin(
    arg0: Option<&str>,
    path: Option<&str>,
    is_executable: impl Fn(&Path) -> bool,
) -> Option<String> {
    let bin = crate::cli::bin_name(arg0);
    if !is_plain_command(&bin) {
        return None;
    }
    std::env::split_paths(path?)
        .filter(|dir| dir.is_absolute())
        .any(|dir| is_executable(&dir.join(&bin)))
        .then_some(bin)
}

/// The request envelope: a string `id` (herdr rejects a number), the method,
/// its parameters.
#[derive(Serialize)]
struct Request<P> {
    id: String,
    method: &'static str,
    params: P,
}

/// `pane.report_agent`'s parameters. `seq` is always sent: herdr drops an
/// unsequenced report from a source once it has accepted a sequenced one.
#[derive(Serialize)]
struct ReportAgent<'a> {
    pane_id: &'a str,
    source: &'static str,
    agent: &'static str,
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'a str>,
    seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    resume_argv: Option<&'a [String]>,
}

/// `pane.release_agent`'s parameters — honoured only when the source and agent
/// match the pane's current reporter, and only for a newer `seq`.
#[derive(Serialize)]
struct ReleaseAgent<'a> {
    pane_id: &'a str,
    source: &'static str,
    agent: &'static str,
    seq: u64,
}

/// One request as the line herdr reads: compact JSON and a closing `\n` —
/// herdr reads up to the newline and drops a request that ends without one.
fn request_line<P: Serialize>(method: &'static str, params: P, seq: u64) -> String {
    let request = Request {
        id: format!("{SOURCE}:{seq}"),
        method,
        params,
    };
    let mut line = serde_json::to_string(&request).unwrap_or_default();
    line.push('\n');
    line
}

/// The `pane.report_agent` request for `report`, numbered `seq`.
#[must_use]
pub fn report_line(pane_id: &str, report: &Report, seq: u64) -> String {
    request_line(
        "pane.report_agent",
        ReportAgent {
            pane_id,
            source: SOURCE,
            agent: AGENT,
            state: report.status.state.as_str(),
            message: report.status.message.as_deref(),
            seq,
            resume_argv: report.resume.as_deref(),
        },
        seq,
    )
}

/// The `pane.release_agent` request, numbered `seq`: the pane is a plain
/// terminal again, its agent name, state and resume command cleared.
#[must_use]
pub fn release_line(pane_id: &str, seq: u64) -> String {
    request_line(
        "pane.release_agent",
        ReleaseAgent {
            pane_id,
            source: SOURCE,
            agent: AGENT,
            seq,
        },
        seq,
    )
}

/// Deliver one request `line` to herdr's socket and return its one-line
/// answer. herdr serves exactly one request per connection, so this connects,
/// writes, reads, and closes; `timeout` bounds the write and the read each.
///
/// # Errors
/// The socket is missing or refuses the connection (herdr gone), the write or
/// the read timed out, or herdr closed the connection without answering.
#[cfg(unix)]
pub fn send(socket: &Path, line: &str, timeout: Duration) -> io::Result<String> {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(socket)?;
    stream.set_write_timeout(Some(timeout))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.write_all(line.as_bytes())?;
    let mut answer = String::new();
    let read = BufReader::new((&stream).take(ANSWER_MAX_BYTES)).read_line(&mut answer)?;
    if read == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "herdr closed the connection without answering",
        ));
    }
    Ok(answer)
}

/// Off Unix herdr's socket is a named pipe, which this client does not speak.
///
/// # Errors
/// Always.
#[cfg(not(unix))]
pub fn send(socket: &Path, line: &str, timeout: Duration) -> io::Result<String> {
    let _ = (socket, line, timeout);
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "herdr's socket is a named pipe on this platform",
    ))
}

/// What the event loop hands the worker.
enum Message {
    Report(Report),
    Release,
}

/// The worker that talks to herdr, so the event loop never does: reports go in
/// through [`report`](Self::report) without blocking and come out in order,
/// each numbered by one [`SeqClock`]; several queued behind a slow send
/// collapse to the newest (herdr's guide: "Send only the latest state"); a
/// report identical to the last one delivered is skipped; failures are
/// dropped. [`release`](Self::release) is the last thing it sends — and
/// dropping the reporter releases too, so a loop that bails out on an error
/// still hands the pane back.
pub struct Reporter {
    tx: Option<mpsc::Sender<Message>>,
    done: Option<mpsc::Receiver<()>>,
}

impl Reporter {
    /// Start the worker for `pane`, each request bounded by [`IO_TIMEOUT`].
    #[must_use]
    pub fn spawn(pane: Pane) -> Self {
        Self::spawn_with(pane, IO_TIMEOUT)
    }

    /// [`spawn`](Self::spawn) with each request bounded by `timeout`. A
    /// thread that cannot be started leaves a reporter that sends nothing.
    #[must_use]
    pub fn spawn_with(pane: Pane, timeout: Duration) -> Self {
        let (tx, rx) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("herdr".into())
            .spawn(move || {
                deliver(&pane, &rx, timeout);
                let _ = done_tx.send(());
            });
        match spawned {
            Ok(_) => Self {
                tx: Some(tx),
                done: Some(done),
            },
            Err(_) => Self {
                tx: None,
                done: None,
            },
        }
    }

    /// Queue `report` — never blocks.
    pub fn report(&self, report: Report) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Message::Report(report));
        }
    }

    /// Release the pane — after whatever is in flight, instead of whatever is
    /// still queued — and wait up to `wait` for the worker to finish, so the
    /// process does not exit before the release has gone out. Only the first
    /// call does anything.
    pub fn release(&mut self, wait: Duration) {
        self.queue_release();
        self.await_worker(wait);
    }

    /// The release's first half: queue it, and close the channel behind it.
    fn queue_release(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Message::Release);
        }
    }

    /// The release's second half: wait up to `wait` for the worker to finish.
    fn await_worker(&mut self, wait: Duration) {
        if let Some(done) = self.done.take() {
            let _ = done.recv_timeout(wait);
        }
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        self.release(RELEASE_WAIT);
    }
}

/// The worker's loop: send each message as it comes — a backlog collapsed to
/// its newest ([`newest`]) — until the release (or until the reporter is gone
/// without one).
fn deliver(pane: &Pane, rx: &mpsc::Receiver<Message>, timeout: Duration) {
    let mut clock = SeqClock::default();
    let mut delivered: Option<Report> = None;
    while let Ok(first) = rx.recv() {
        match newest(first, rx) {
            Message::Report(report) => {
                if delivered.as_ref() == Some(&report) {
                    continue;
                }
                let seq = clock.next(unix_nanos(SystemTime::now()));
                let line = report_line(&pane.id, &report, seq);
                delivered = send(&pane.socket, &line, timeout).is_ok().then_some(report);
            }
            Message::Release => {
                let seq = clock.next(unix_nanos(SystemTime::now()));
                let _ = send(&pane.socket, &release_line(&pane.id, seq), timeout);
                return;
            }
        }
    }
}

/// `first` and everything queued behind it collapsed to the one message still
/// worth sending: the newest report, or the release, which supersedes them.
fn newest(first: Message, rx: &mpsc::Receiver<Message>) -> Message {
    let mut newest = first;
    while let Ok(next) = rx.try_recv() {
        if !matches!(newest, Message::Release) {
            newest = next;
        }
    }
    newest
}

/// The pane this process reports for, once [`start`] has run — what
/// [`stamp_child`] marks children with.
static REPORTING_PANE: OnceLock<String> = OnceLock::new();

/// Start reporting for `pane`: mark this process as its reporter, so the shell
/// children it spawns from here on carry [`REPORTER_ENV`], and spawn the
/// [`Reporter`].
#[must_use]
pub fn start(pane: Pane) -> Reporter {
    let _ = REPORTING_PANE.set(pane.id.clone());
    Reporter::spawn(pane)
}

/// The pane this process reports for, if it does.
#[must_use]
pub fn reporting_pane() -> Option<&'static str> {
    REPORTING_PANE.get().map(String::as_str)
}

/// Mark a shell child of this process with [`REPORTER_ENV`] when it reports
/// for a pane — the spawns the model's and the user's commands go through call
/// this (`subprocess`'s tier walk, the `bash` tool's terminal), so an
/// alter-zero run from one of them knows a parent already speaks for the pane.
pub fn stamp_child(command: &mut Command) {
    stamp_child_with(command, reporting_pane());
}

/// [`stamp_child`] with the pane given: set the marker to `pane`, or leave the
/// command alone when there is none.
pub fn stamp_child_with(command: &mut Command, pane: Option<&str>) {
    if let Some(pane) = pane {
        command.env(REPORTER_ENV, pane);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ask::{AskOption, AskQuestion};
    use crate::permission::PermissionKind;
    use crate::stream::{AgentSpec, StreamEvent};

    /// An environment of `pairs` — the lookup [`Pane::from_env`] reads.
    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name: &str| {
            owned
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    /// The three variables herdr gives every pane.
    const INSIDE: [(&str, &str); 3] = [
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w1:p2"),
        ("HERDR_SOCKET_PATH", "/run/herdr.sock"),
    ];

    fn inside_with(extra: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
        let mut pairs = INSIDE.to_vec();
        pairs.extend_from_slice(extra);
        pairs
    }

    // ----- Pane::from_env -----

    #[test]
    fn a_herdr_pane_is_found_from_the_three_variables_herdr_sets() {
        assert_eq!(
            Pane::from_env(env(&INSIDE)),
            Some(Pane {
                id: "w1:p2".into(),
                socket: PathBuf::from("/run/herdr.sock"),
            })
        );
    }

    #[test]
    fn outside_herdr_nothing_is_found() {
        assert_eq!(Pane::from_env(env(&[])), None);
    }

    #[test]
    fn every_one_of_the_three_variables_is_required() {
        // herdr's guide: "Only report when HERDR_ENV=1 and the other
        // variables are set. Outside Herdr, your integration should do
        // nothing."
        for missing in ["HERDR_ENV", "HERDR_PANE_ID", "HERDR_SOCKET_PATH"] {
            let pairs: Vec<_> = INSIDE
                .iter()
                .copied()
                .filter(|(k, _)| *k != missing)
                .collect();
            assert_eq!(Pane::from_env(env(&pairs)), None, "without {missing}");
        }
    }

    #[test]
    fn herdr_env_must_say_exactly_one() {
        for value in ["0", "", "true", "yes"] {
            let mut pairs = INSIDE.to_vec();
            pairs[0] = ("HERDR_ENV", value);
            assert_eq!(Pane::from_env(env(&pairs)), None, "HERDR_ENV={value:?}");
        }
    }

    #[test]
    fn a_blank_pane_id_or_socket_path_is_no_pane() {
        let blank_pane = [
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "  "),
            ("HERDR_SOCKET_PATH", "/s"),
        ];
        assert_eq!(Pane::from_env(env(&blank_pane)), None);
        let blank_socket = [
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "w1:p1"),
            ("HERDR_SOCKET_PATH", ""),
        ];
        assert_eq!(Pane::from_env(env(&blank_socket)), None);
    }

    #[test]
    fn the_off_switch_turns_reporting_off() {
        for value in ["0", "false", "no", "off", "OFF", " False "] {
            assert_eq!(
                Pane::from_env(env(&inside_with(&[("ALTER_ZERO_HERDR", value)]))),
                None,
                "ALTER_ZERO_HERDR={value:?}"
            );
        }
        for value in ["1", "true", "on", ""] {
            assert!(
                Pane::from_env(env(&inside_with(&[("ALTER_ZERO_HERDR", value)]))).is_some(),
                "ALTER_ZERO_HERDR={value:?} leaves it on"
            );
        }
    }

    #[test]
    fn a_session_nested_under_a_reporting_one_in_the_same_pane_stays_quiet() {
        // An alter-zero started by another one's `bash` tool inherits the
        // pane's variables and the parent's marker: reporting would fight
        // the parent for the pane, and its release would clear the parent's
        // state (`docs/herdr.md` *Nested sessions*).
        assert_eq!(
            Pane::from_env(env(&inside_with(&[("ALTER_ZERO_HERDR_REPORTER", "w1:p2")]))),
            None
        );
    }

    #[test]
    fn a_marker_naming_another_pane_does_not_silence_this_one() {
        assert!(
            Pane::from_env(env(&inside_with(&[("ALTER_ZERO_HERDR_REPORTER", "w9:p9")]))).is_some()
        );
    }

    // ----- State / status_of -----

    #[test]
    fn states_are_herdrs_wire_names() {
        assert_eq!(State::Idle.as_str(), "idle");
        assert_eq!(State::Working.as_str(), "working");
        assert_eq!(State::Blocked.as_str(), "blocked");
    }

    fn idle() -> Status {
        Status {
            state: State::Idle,
            message: None,
        }
    }

    fn working() -> Status {
        Status {
            state: State::Working,
            message: None,
        }
    }

    fn bash_request(command: &str) -> PermissionRequest {
        PermissionRequest {
            id: "p1".into(),
            kind: PermissionKind::Bash,
            target: command.into(),
            body: String::new(),
            detail: None,
            agent: None,
            agent_id: None,
        }
    }

    fn question(text: &str) -> AskQuestion {
        AskQuestion {
            question: text.into(),
            header: "Pick".into(),
            options: vec![
                AskOption {
                    label: "A".into(),
                    description: "a".into(),
                    preview: None,
                },
                AskOption {
                    label: "B".into(),
                    description: "b".into(),
                    preview: None,
                },
            ],
            multi_select: false,
        }
    }

    fn ask_request(questions: &[&str]) -> AskRequest {
        AskRequest {
            id: "q1".into(),
            questions: questions.iter().map(|q| question(q)).collect(),
        }
    }

    fn agent_specs() -> Vec<AgentSpec> {
        vec![AgentSpec {
            id: "a1".into(),
            description: "Survey the repo".into(),
            agent_type: crate::agents::GENERAL_PURPOSE.into(),
            prompt: "Survey it".into(),
            background: true,
            call_id: None,
            arguments: None,
        }]
    }

    #[test]
    fn a_session_with_nothing_running_is_idle() {
        assert_eq!(status_of(&App::new()), idle());
    }

    #[test]
    fn a_turn_in_flight_is_working() {
        let mut app = App::new();
        app.begin_stream();
        assert_eq!(status_of(&app), working());
    }

    #[test]
    fn a_shell_command_and_a_compaction_are_working_too() {
        let mut shell = App::new();
        shell.begin_shell("cargo build");
        assert_eq!(status_of(&shell), working());
        let mut compact = App::new();
        compact.begin_compact(false);
        assert_eq!(status_of(&compact), working());
    }

    #[test]
    fn a_finished_turn_is_idle_again() {
        let mut app = App::new();
        app.begin_stream();
        app.end_turn(3);
        assert_eq!(status_of(&app), idle());
    }

    #[test]
    fn a_background_agent_still_running_keeps_the_session_working() {
        // The lead's turn ended, but the work it launched has not: the
        // agent's completion starts a follow-up turn on its own, so `idle`
        // here would make herdr call the pane done before it is.
        let mut app = App::new();
        app.begin_stream();
        app.start_agent_group(true, &agent_specs());
        app.end_turn(2);
        assert!(!app.turn_active());
        assert_eq!(status_of(&app), working());
    }

    #[test]
    fn a_finished_background_agent_no_longer_counts() {
        let mut app = App::new();
        app.begin_stream();
        app.start_agent_group(true, &agent_specs());
        app.end_turn(2);
        app.apply_agent_event("a1", &StreamEvent::StreamDone);
        assert_eq!(status_of(&app), idle());
    }

    #[test]
    fn an_open_permission_prompt_is_blocked_and_says_what_it_asks() {
        let mut app = App::new();
        app.begin_stream();
        app.open_permission(bash_request("cargo test"));
        assert_eq!(
            status_of(&app),
            Status {
                state: State::Blocked,
                message: Some("Bash command: cargo test".into()),
            }
        );
    }

    #[test]
    fn a_background_agents_prompt_blocks_an_idle_session() {
        // No main turn at all: a subagent asked on its own.
        let mut app = App::new();
        let mut request = bash_request("rm -rf build");
        request.agent = Some("general-purpose".into());
        request.agent_id = Some("a1".into());
        app.open_permission(request);
        assert_eq!(status_of(&app).state, State::Blocked);
    }

    #[test]
    fn an_open_question_is_blocked_and_names_it() {
        let mut app = App::new();
        app.begin_stream();
        app.open_ask(ask_request(&["Which database should the cache use?"]));
        assert_eq!(
            status_of(&app),
            Status {
                state: State::Blocked,
                message: Some("Question: Which database should the cache use?".into()),
            }
        );
    }

    // ----- the blocked messages -----

    fn file_request(kind: PermissionKind, path: &str) -> PermissionRequest {
        PermissionRequest {
            kind,
            target: path.into(),
            body: "1 +x".into(),
            ..bash_request("")
        }
    }

    #[test]
    fn a_file_prompt_names_the_file_the_way_the_screen_shows_it() {
        let paths = PathDisplay::new("/home/u/repo", Some(PathBuf::from("/home/u")));
        assert_eq!(
            permission_message(
                &file_request(PermissionKind::Write, "/home/u/repo/src/new.rs"),
                &paths
            ),
            "Create file: src/new.rs"
        );
        assert_eq!(
            permission_message(
                &file_request(PermissionKind::Edit, "/home/u/notes.md"),
                &paths
            ),
            "Edit file: ~/notes.md"
        );
    }

    #[test]
    fn an_mcp_prompt_names_the_tool_like_the_cell_does() {
        let request = PermissionRequest {
            kind: PermissionKind::Mcp,
            target: "mcp__deepwiki__ask_question".into(),
            ..bash_request("")
        };
        assert_eq!(
            permission_message(&request, &PathDisplay::VERBATIM),
            "Tool use: Deepwiki - ask_question (MCP)"
        );
    }

    #[test]
    fn a_session_input_prompt_names_the_program_it_types_into() {
        let request = PermissionRequest {
            kind: PermissionKind::Session,
            target: "s3".into(),
            body: "y⏎".into(),
            detail: Some("./configure".into()),
            ..bash_request("")
        };
        assert_eq!(
            permission_message(&request, &PathDisplay::VERBATIM),
            "Session input: ./configure"
        );
    }

    #[test]
    fn a_multi_line_command_is_cut_to_its_first_line() {
        assert_eq!(
            permission_message(&bash_request("set -e\nmake all"), &PathDisplay::VERBATIM),
            "Bash command: set -e …"
        );
    }

    #[test]
    fn a_long_message_is_cut_to_the_cap_with_an_ellipsis() {
        let long = format!("echo {}", "x".repeat(200));
        let message = permission_message(&bash_request(&long), &PathDisplay::VERBATIM);
        assert_eq!(message.chars().count(), MESSAGE_MAX_CHARS);
        assert!(message.ends_with('…'));
        assert!(message.starts_with("Bash command: echo xxx"));
    }

    #[test]
    fn several_questions_say_how_many_and_name_the_first() {
        assert_eq!(
            ask_message(&ask_request(&["Coffee or tea?", "Milk?", "Sugar?"])),
            "3 questions: Coffee or tea?"
        );
    }

    // ----- Tracker -----

    #[test]
    fn the_first_status_is_always_reported() {
        let mut tracker = Tracker::default();
        assert_eq!(
            tracker.next(idle(), None, Some("alter-zero")),
            Some(Report {
                status: idle(),
                resume: None,
            })
        );
    }

    #[test]
    fn an_unchanged_status_is_not_reported_again() {
        let mut tracker = Tracker::default();
        assert!(tracker.next(working(), None, None).is_some());
        assert_eq!(tracker.next(working(), None, None), None);
    }

    #[test]
    fn a_new_state_or_message_is_reported() {
        let mut tracker = Tracker::default();
        let _ = tracker.next(working(), None, None);
        let blocked = |message: &str| Status {
            state: State::Blocked,
            message: Some(message.into()),
        };
        assert!(
            tracker
                .next(blocked("Bash command: ls"), None, None)
                .is_some()
        );
        assert!(
            tracker
                .next(blocked("Bash command: pwd"), None, None)
                .is_some(),
            "a second prompt is a new report"
        );
        assert!(tracker.next(idle(), None, None).is_some());
    }

    #[test]
    fn a_session_id_arriving_carries_the_resume_command() {
        // The rollout file — and its id — exists only once the conversation
        // records something, so the first report of a fresh session has no
        // resume command and a later one does.
        let mut tracker = Tracker::default();
        let _ = tracker.next(idle(), None, Some("alter-zero"));
        assert_eq!(
            tracker.next(idle(), Some("18a9f2c3-1a2b"), Some("alter-zero")),
            Some(Report {
                status: idle(),
                resume: Some(vec![
                    "alter-zero".into(),
                    "--resume".into(),
                    "18a9f2c3-1a2b".into()
                ]),
            })
        );
        assert_eq!(
            tracker.next(idle(), Some("18a9f2c3-1a2b"), Some("alter-zero")),
            None
        );
    }

    #[test]
    fn without_a_resume_command_name_a_session_still_reports_its_state() {
        let mut tracker = Tracker::default();
        assert_eq!(
            tracker.next(working(), Some("18a9f2c3-1a2b"), None),
            Some(Report {
                status: working(),
                resume: None,
            })
        );
    }

    // ----- SeqClock / unix_nanos -----

    #[test]
    fn sequence_numbers_follow_the_clock() {
        let mut clock = SeqClock::default();
        assert_eq!(clock.next(1_000), 1_000);
        assert_eq!(clock.next(5_000), 5_000);
    }

    #[test]
    fn sequence_numbers_always_increase_even_when_the_clock_does_not() {
        // herdr ignores a report whose number is not strictly higher than the
        // last it accepted from this source.
        let mut clock = SeqClock::default();
        assert_eq!(clock.next(1_000), 1_000);
        assert_eq!(clock.next(1_000), 1_001);
        assert_eq!(clock.next(400), 1_002);
    }

    #[test]
    fn the_clock_reads_nanoseconds_since_the_epoch() {
        let at = SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, 123);
        assert_eq!(unix_nanos(at), 1_700_000_000_000_000_123);
        assert_eq!(
            unix_nanos(SystemTime::UNIX_EPOCH - Duration::from_secs(1)),
            0
        );
    }

    // ----- resume command -----

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn the_resume_command_is_the_bin_resume_and_the_id() {
        assert_eq!(
            resume_argv("alter-zero", "18a9f2c33d41e5b6-1a2b"),
            Some(argv(&["alter-zero", "--resume", "18a9f2c33d41e5b6-1a2b"]))
        );
    }

    #[test]
    fn herdrs_resume_rules_are_mirrored() {
        assert!(valid_resume_argv(&argv(&["alter-zero", "--resume", "x"])));
        assert!(valid_resume_argv(&argv(&["az_2.1", "-c"])));
        // Not a plain command name.
        assert!(!valid_resume_argv(&argv(&[
            "./alter-zero",
            "--resume",
            "x"
        ])));
        assert!(!valid_resume_argv(&argv(&["/usr/bin/alter-zero"])));
        assert!(!valid_resume_argv(&argv(&["-x"])));
        assert!(!valid_resume_argv(&argv(&[""])));
        assert!(!valid_resume_argv(&argv(&["alter zero"])));
        // Empty, apostrophes, control characters.
        assert!(!valid_resume_argv(&[]));
        assert!(!valid_resume_argv(&argv(&["alter-zero", "it's"])));
        assert!(!valid_resume_argv(&argv(&["alter-zero", "a\nb"])));
        // At most 64 arguments and 8 KiB.
        let mut many = argv(&["alter-zero"]);
        many.extend((0..63).map(|i| i.to_string()));
        assert!(valid_resume_argv(&many));
        many.push("one-too-many".into());
        assert!(!valid_resume_argv(&many));
        assert!(!valid_resume_argv(&argv(&[
            "alter-zero",
            &"x".repeat(8192)
        ])));
    }

    #[test]
    fn a_resume_command_herdr_would_refuse_is_not_offered() {
        assert_eq!(resume_argv("./alter-zero", "abc"), None);
        assert_eq!(resume_argv("alter-zero", "it's"), None);
    }

    #[test]
    fn the_resume_name_is_the_invoked_name_when_path_finds_it() {
        let path = "/usr/local/bin:/home/u/.local/bin";
        let found = |p: &Path| p == Path::new("/home/u/.local/bin/alter-zero");
        assert_eq!(
            resume_bin(Some("/home/u/.local/bin/alter-zero"), Some(path), found),
            Some("alter-zero".into())
        );
        assert_eq!(
            resume_bin(Some("alter-zero"), Some(path), found),
            Some("alter-zero".into())
        );
    }

    #[test]
    fn a_binary_path_cannot_find_has_no_resume_name() {
        // `./target/release/alter-zero` from a checkout: herdr would type
        // `alter-zero --resume …` into a fresh shell, which has never heard
        // of it.
        let nothing = |_: &Path| false;
        assert_eq!(
            resume_bin(
                Some("./target/release/alter-zero"),
                Some("/usr/bin"),
                nothing
            ),
            None
        );
        assert_eq!(resume_bin(Some("alter-zero"), None, |_: &Path| true), None);
    }

    #[test]
    fn relative_and_empty_path_entries_are_not_searched() {
        // A restored pane's shell starts in the session's directory, not ours:
        // a `.`-relative hit here says nothing about what it will find.
        let any = |_: &Path| true;
        assert_eq!(resume_bin(Some("alter-zero"), Some(":bin:./x"), any), None);
    }

    #[test]
    fn a_renamed_install_resumes_under_its_own_name() {
        let found = |p: &Path| p == Path::new("/opt/bin/az");
        assert_eq!(
            resume_bin(Some("/opt/bin/az"), Some("/opt/bin"), found),
            Some("az".into())
        );
    }

    #[test]
    fn no_argv0_falls_back_to_the_canonical_name() {
        let found = |p: &Path| p == Path::new("/usr/bin/alter-zero");
        assert_eq!(
            resume_bin(None, Some("/usr/bin"), found),
            Some("alter-zero".into())
        );
    }

    // ----- request lines -----

    fn parsed(line: &str) -> serde_json::Value {
        assert!(line.ends_with('\n'), "one request per line: {line:?}");
        assert_eq!(line.matches('\n').count(), 1, "a single line: {line:?}");
        serde_json::from_str(line).expect("valid JSON")
    }

    #[test]
    fn a_report_is_a_pane_report_agent_request() {
        let report = Report {
            status: working(),
            resume: None,
        };
        let request = parsed(&report_line("w1:p2", &report, 42));
        assert_eq!(
            request,
            serde_json::json!({
                "id": "alter-zero:42",
                "method": "pane.report_agent",
                "params": {
                    "pane_id": "w1:p2",
                    "source": "alter-zero",
                    "agent": "alter-zero",
                    "state": "working",
                    "seq": 42,
                }
            })
        );
    }

    #[test]
    fn a_blocked_report_carries_its_message_and_a_resume_command_rides_along() {
        let report = Report {
            status: Status {
                state: State::Blocked,
                message: Some("Bash command: make".into()),
            },
            resume: Some(argv(&["alter-zero", "--resume", "abc-1"])),
        };
        let request = parsed(&report_line("w1:p2", &report, 7));
        let params = &request["params"];
        assert_eq!(params["state"], "blocked");
        assert_eq!(params["message"], "Bash command: make");
        assert_eq!(
            params["resume_argv"],
            serde_json::json!(["alter-zero", "--resume", "abc-1"])
        );
    }

    #[test]
    fn a_release_is_a_pane_release_agent_request() {
        assert_eq!(
            parsed(&release_line("w1:p2", 99)),
            serde_json::json!({
                "id": "alter-zero:99",
                "method": "pane.release_agent",
                "params": {
                    "pane_id": "w1:p2",
                    "source": "alter-zero",
                    "agent": "alter-zero",
                    "seq": 99,
                }
            })
        );
    }

    #[test]
    fn the_source_passes_herdrs_strictest_source_rule() {
        // herdr validates a metadata source as at most 80 ASCII letters,
        // digits, `:`, `.`, `_`, `-`; holding ours to it keeps every method
        // open to it.
        assert!(SOURCE.len() <= 80);
        assert!(
            SOURCE
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b":._-".contains(&b))
        );
        assert!(
            !SOURCE.starts_with("herdr:"),
            "herdr reserves its own prefix"
        );
    }

    // ----- the transport, against a real socket -----

    /// How a [`stub_server`] answers each request.
    #[cfg(unix)]
    enum Answers {
        /// At once, `ok` — a healthy herdr.
        Always,
        /// Never: the connection is held open, the way a wedged server would.
        Never,
        /// `ok`, once the test hands over a token — so a test decides exactly
        /// when the worker is let go, and what has queued behind it by then.
        Gated(std::sync::mpsc::Receiver<()>),
    }

    /// A one-connection-per-request stand-in for herdr's server: each
    /// accepted connection's line is sent back on the returned channel, then
    /// answered per `answers`. Returns the socket path (inside `dir`).
    #[cfg(unix)]
    fn stub_server(
        dir: &Path,
        answers: Answers,
    ) -> (PathBuf, std::sync::mpsc::Receiver<serde_json::Value>) {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;

        let socket = dir.join("herdr.sock");
        let listener = UnixListener::bind(&socket).expect("bind");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut line = String::new();
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                if reader.read_line(&mut line).is_err() || line.is_empty() {
                    continue;
                }
                let request: serde_json::Value = serde_json::from_str(&line).expect("json");
                let id = request["id"].clone();
                if tx.send(request).is_err() {
                    return;
                }
                let answer = match &answers {
                    Answers::Always => true,
                    Answers::Never => false,
                    Answers::Gated(go) => go.recv().is_ok(),
                };
                if answer {
                    let response = serde_json::json!({"id": id, "result": {"type": "ok"}});
                    let _ = writeln!(stream, "{response}");
                } else {
                    held.push(stream);
                }
            }
        });
        (socket, rx)
    }

    /// A [`stub_server`] that answers only when the returned sender says so,
    /// one token per request.
    #[cfg(unix)]
    fn gated_server(
        dir: &Path,
    ) -> (
        PathBuf,
        std::sync::mpsc::Receiver<serde_json::Value>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (go, gate) = std::sync::mpsc::channel();
        let (socket, rx) = stub_server(dir, Answers::Gated(gate));
        (socket, rx, go)
    }

    #[cfg(unix)]
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "az-herdr-{name}-{}-{}",
            std::process::id(),
            unix_nanos(SystemTime::now())
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[cfg(unix)]
    #[test]
    fn send_delivers_one_line_and_returns_the_answer() {
        let dir = temp_dir("send");
        let (socket, rx) = stub_server(&dir, Answers::Always);
        let answer = send(&socket, &release_line("w1:p1", 5), IO_TIMEOUT).expect("sent");
        let request = rx.recv_timeout(Duration::from_secs(2)).expect("received");
        assert_eq!(request["method"], "pane.release_agent");
        let answer: serde_json::Value = serde_json::from_str(&answer).expect("json answer");
        assert_eq!(answer["result"]["type"], "ok");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn send_to_a_missing_socket_fails_at_once() {
        let started = std::time::Instant::now();
        let result = send(
            Path::new("/nonexistent/herdr.sock"),
            &release_line("w1:p1", 5),
            IO_TIMEOUT,
        );
        assert!(result.is_err());
        assert!(started.elapsed() < IO_TIMEOUT);
    }

    #[cfg(unix)]
    #[test]
    fn a_server_that_never_answers_costs_at_most_the_timeout() {
        let dir = temp_dir("silent");
        let (socket, rx) = stub_server(&dir, Answers::Never);
        let started = std::time::Instant::now();
        let result = send(
            &socket,
            &release_line("w1:p1", 5),
            Duration::from_millis(150),
        );
        assert!(result.is_err(), "no answer is a timeout");
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(
            rx.recv_timeout(Duration::from_secs(1)).is_ok(),
            "still delivered"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    // ----- the Reporter worker -----

    #[cfg(unix)]
    fn report(state: State) -> Report {
        Report {
            status: Status {
                state,
                message: None,
            },
            resume: None,
        }
    }

    #[cfg(unix)]
    fn drain(rx: &std::sync::mpsc::Receiver<serde_json::Value>) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        while let Ok(request) = rx.recv_timeout(Duration::from_millis(300)) {
            out.push(request);
        }
        out
    }

    #[cfg(unix)]
    #[test]
    fn the_reporter_sends_reports_then_releases_last() {
        let dir = temp_dir("reporter");
        let (socket, rx) = stub_server(&dir, Answers::Always);
        let mut reporter = Reporter::spawn(Pane {
            id: "w1:p2".into(),
            socket,
        });
        // Each lands before the next is queued, so none is coalesced away.
        reporter.report(report(State::Working));
        let first = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first report");
        reporter.report(report(State::Idle));
        let second = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second report");
        reporter.release(RELEASE_WAIT);
        let mut requests = vec![first, second];
        requests.extend(drain(&rx));
        let methods: Vec<_> = requests
            .iter()
            .map(|r| (r["method"].clone(), r["params"]["state"].clone()))
            .collect();
        assert_eq!(
            methods,
            vec![
                ("pane.report_agent".into(), "working".into()),
                ("pane.report_agent".into(), "idle".into()),
                ("pane.release_agent".into(), serde_json::Value::Null),
            ]
        );
        let seqs: Vec<u64> = requests
            .iter()
            .map(|r| r["params"]["seq"].as_u64().expect("seq"))
            .collect();
        assert!(
            seqs.windows(2).all(|w| w[0] < w[1]),
            "strictly increasing: {seqs:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn dropping_the_reporter_releases_the_pane() {
        // The loop's error paths skip `shutdown()`; the drop still says
        // goodbye.
        let dir = temp_dir("drop");
        let (socket, rx) = stub_server(&dir, Answers::Always);
        let reporter = Reporter::spawn(Pane {
            id: "w1:p2".into(),
            socket,
        });
        drop(reporter);
        let requests = drain(&rx);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["method"], "pane.release_agent");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_release_waits_no_longer_than_it_was_told_to() {
        // herdr gone silent: the quit must not hang on it.
        let dir = temp_dir("slow");
        let (socket, _rx) = stub_server(&dir, Answers::Never);
        let mut reporter = Reporter::spawn_with(
            Pane {
                id: "w1:p2".into(),
                socket,
            },
            Duration::from_secs(5),
        );
        reporter.report(report(State::Working));
        let started = std::time::Instant::now();
        reporter.release(Duration::from_millis(200));
        assert!(started.elapsed() < Duration::from_secs(1));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn reports_queued_behind_a_slow_send_collapse_to_the_newest() {
        // "Send only the latest state. If several changes queue up while a
        // report is in flight, drop the older ones." — herdr's guide.
        let dir = temp_dir("coalesce");
        let (socket, rx, go) = gated_server(&dir);
        let mut reporter = Reporter::spawn_with(
            Pane {
                id: "w1:p2".into(),
                socket,
            },
            Duration::from_secs(5),
        );
        reporter.report(report(State::Working));
        let first = rx.recv_timeout(Duration::from_secs(2)).expect("first");
        assert_eq!(first["params"]["state"], "working");
        // The worker now waits on herdr's answer; three more queue behind it.
        reporter.report(report(State::Blocked));
        reporter.report(report(State::Working));
        reporter.report(report(State::Idle));
        go.send(()).expect("answer the first");
        let next = rx.recv_timeout(Duration::from_secs(2)).expect("next");
        assert_eq!(next["params"]["state"], "idle", "only the newest went out");
        go.send(()).expect("answer the idle");
        go.send(()).expect("answer the release ahead of time");
        reporter.release(Duration::from_secs(2));
        let rest = drain(&rx);
        assert_eq!(rest.len(), 1, "{rest:?}");
        assert_eq!(rest[0]["method"], "pane.release_agent");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_release_supersedes_a_report_still_waiting_to_go_out() {
        // Quitting mid-turn: the release clears the pane's state anyway, so
        // a report it overtook in the queue is not worth a round trip.
        let dir = temp_dir("supersede");
        let (socket, rx, go) = gated_server(&dir);
        let mut reporter = Reporter::spawn_with(
            Pane {
                id: "w1:p2".into(),
                socket,
            },
            Duration::from_secs(5),
        );
        reporter.report(report(State::Working));
        let _ = rx.recv_timeout(Duration::from_secs(2)).expect("first");
        reporter.report(report(State::Idle));
        reporter.queue_release();
        go.send(()).expect("answer the first");
        go.send(()).expect("answer the release");
        reporter.await_worker(Duration::from_secs(2));
        let rest = drain(&rx);
        assert_eq!(rest.len(), 1, "{rest:?}");
        assert_eq!(rest[0]["method"], "pane.release_agent");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreachable_herdr_never_blocks_the_caller() {
        let mut reporter = Reporter::spawn(Pane {
            id: "w1:p2".into(),
            socket: PathBuf::from("/nonexistent/herdr.sock"),
        });
        let started = std::time::Instant::now();
        for _ in 0..100 {
            reporter.report(report(State::Working));
            reporter.report(report(State::Idle));
        }
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "reports never wait"
        );
        let started = std::time::Instant::now();
        reporter.release(RELEASE_WAIT);
        assert!(
            started.elapsed() < RELEASE_WAIT,
            "a refused connection fails at once, so the release does not wait it out"
        );
    }

    // ----- the nested-session marker -----

    #[test]
    fn a_reporting_sessions_children_carry_the_marker() {
        let mut command = std::process::Command::new("sh");
        stamp_child_with(&mut command, Some("w1:p2"));
        let envs: Vec<_> = command.get_envs().collect();
        assert_eq!(
            envs,
            vec![(
                std::ffi::OsStr::new("ALTER_ZERO_HERDR_REPORTER"),
                Some(std::ffi::OsStr::new("w1:p2"))
            )]
        );
    }

    #[test]
    fn a_session_that_does_not_report_leaves_its_children_alone() {
        let mut command = std::process::Command::new("sh");
        stamp_child_with(&mut command, None);
        assert_eq!(command.get_envs().count(), 0);
    }
}
