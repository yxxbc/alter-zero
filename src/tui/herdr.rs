//! herdr at the boundary (`docs/herdr.md`): reading the pane's environment
//! once at startup, feeding the session's state to the reporter at the loop
//! bottom, and handing the pane back on the way out. Everything that decides
//! anything — whether to report, what state the session is in, when a report
//! is due, what goes on the wire, the worker that sends it — is
//! [`alter_zero::herdr`]; this file only reads the environment and the
//! filesystem and calls it at the right moments.
//!
//! Three moments:
//!
//! - **Startup** ([`HerdrLink::from_env`], then [`Session::herdr_tick`] at the
//!   end of `bootstrap`): outside herdr nothing is spawned at all; inside it
//!   the reporter starts and the first report says the pane runs alter-zero
//!   — idle, or working when a `[PROMPT]` started the first turn.
//! - **Every loop iteration** ([`Session::herdr_tick`] at the bottom of
//!   `after_iteration`): by then every way a turn can start or end has
//!   happened — a queued turn chained, an auto-compaction begun, a prompt
//!   answered, the rollout materialized with its id — so the reading is
//!   settled, and the [`Tracker`] turns it into a report only when it
//!   changed. An iteration that changed nothing herdr can see costs a few
//!   compares and no I/O; the send itself is always on the worker.
//! - **Quit** ([`Session::release_herdr`], last in `shutdown`): the release,
//!   waited on for at most [`herdr::RELEASE_WAIT`] so the process does not
//!   exit before it has gone out. A loop that bails out on an error never
//!   reaches `shutdown`; dropping the reporter releases then.

use std::path::Path;

use alter_zero::herdr::{self, Reporter, Tracker};

use super::Session;

/// A session's connection to its herdr pane: the worker, the change check,
/// and the name a resume command starts with.
pub(crate) struct HerdrLink {
    reporter: Reporter,
    tracker: Tracker,
    /// `alter-zero`, or whatever this binary was invoked as — `None` when a
    /// fresh shell could not find it on `PATH` (`herdr::resume_bin`), and the
    /// reports then carry no resume command.
    resume_bin: Option<String>,
}

impl HerdrLink {
    /// The link for the pane this process runs in, or `None` outside herdr,
    /// with `ALTER_ZERO_HERDR=0`, or nested under a session already reporting
    /// for the pane (`herdr::Pane::from_env`). Starting it marks the process
    /// as the pane's reporter, so every shell child spawned from here on
    /// carries the marker (`herdr::stamp_child`).
    pub(crate) fn from_env() -> Option<Self> {
        let pane = herdr::Pane::from_env(|name| std::env::var(name).ok())?;
        let arg0 = std::env::args().next();
        let path = std::env::var("PATH").ok();
        let resume_bin = herdr::resume_bin(arg0.as_deref(), path.as_deref(), is_executable);
        Some(Self {
            reporter: herdr::start(pane),
            tracker: Tracker::default(),
            resume_bin,
        })
    }
}

/// Whether `path` is a file a shell could run — the resume command's `PATH`
/// check.
fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

impl Session<'_> {
    /// Report the session's state to herdr when it changed since the last
    /// report — the loop bottom's last step, and the end of `bootstrap`'s.
    /// Nothing at all outside herdr.
    pub(crate) fn herdr_tick(&mut self) {
        let Some(link) = &mut self.herdr else {
            return;
        };
        let status = herdr::status_of(&self.app);
        let session = self.recorder.session_id();
        if let Some(report) = link
            .tracker
            .next(status, session, link.resume_bin.as_deref())
        {
            link.reporter.report(report);
        }
    }

    /// Hand the pane back to herdr — its agent name, state and resume command
    /// cleared — and wait (bounded) for that to go out. The last thing a quit
    /// does before the terminal is restored.
    pub(crate) fn release_herdr(&mut self) {
        if let Some(mut link) = self.herdr.take() {
            link.reporter.release(herdr::RELEASE_WAIT);
        }
    }
}
