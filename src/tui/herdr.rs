//! The herdr pane-state report's boundary (`docs/herdr.md`): the reporter
//! [`Session::bootstrap`] builds when the session runs in a herdr pane, the
//! loop-bottom tick that hands its request lines to the writer thread, the
//! failure a backend error leaves behind, and the release at quit.
//!
//! Three rules, the telemetry module's posture applied to a local socket:
//!
//! 1. **Derived, not pushed.** The state comes from one place —
//!    `herdr::activity` read off `App` at the bottom of every iteration — so
//!    no transition can be forgotten by the code path that made it. The one
//!    fact `App` does not keep, that the last turn died on an error, is
//!    handed over where the error lands ([`Session::herdr_turn_failed`]).
//! 2. **Nothing on the loop.** The tick is a few field reads and a compare;
//!    a line that has to go out is handed to `workers::spawn_herdr_reporter`'s
//!    thread, and the quit's release waits on its own thread for at most
//!    `herdr::RELEASE_WAIT`.
//! 3. **Nothing outside herdr.** No pane, no link: every method here is a
//!    no-op.

use std::path::PathBuf;

use alter_zero::herdr::{self, Pane, Reporter};

use super::{Session, host, workers};

/// A herdr pane's reporter and the thread that writes its requests.
pub(crate) struct HerdrLink {
    reporter: Reporter,
    /// The socket the quit's release is sent to directly.
    socket: PathBuf,
    /// The writer thread's queue; dropping it ends the thread.
    tx: std::sync::mpsc::Sender<String>,
}

impl HerdrLink {
    /// Start reporting for `pane`, naming `bin` — the name this process was
    /// run as — in the resume command.
    pub(crate) fn start(pane: Pane, bin: &str) -> Self {
        let tx = workers::spawn_herdr_reporter(pane.socket.clone());
        Self {
            reporter: Reporter::new(pane.pane_id, bin),
            socket: pane.socket,
            tx,
        }
    }
}

impl Session<'_> {
    /// Tell herdr what the session is doing, if that changed since it was
    /// last told: run at the bottom of every loop iteration, and once at
    /// startup so the pane is claimed from the first frame.
    pub(crate) fn herdr_tick(&mut self) {
        let Some(link) = self.herdr.as_mut() else {
            return;
        };
        let activity = herdr::activity(&self.app);
        let session = self.recorder.session_id();
        if let Some(line) = link
            .reporter
            .observe(&activity, session, host::unix_millis())
        {
            // A closed queue means the writer is gone; nothing to do about it.
            let _ = link.tx.send(line);
        }
    }

    /// The turn just ended on a backend error: the pane holds `blocked` with
    /// it until the next turn starts or the conversation is replaced — a
    /// turn that died did not finish, and herdr's "finished" toast would say
    /// it had.
    pub(crate) fn herdr_turn_failed(&mut self, error: &str) {
        let conversation = self.app.history_generation();
        if let Some(link) = self.herdr.as_mut() {
            link.reporter.fail(error, conversation);
        }
    }

    /// Release the pane on the way out: herdr forgets the agent, its state
    /// and its resume command at once. The last thing a quit does, sent on a
    /// thread of its own and waited for at most `herdr::RELEASE_WAIT` — a
    /// wedged herdr cannot keep the terminal from being restored, and a
    /// release that never arrives is cleared by herdr itself once the pane
    /// is back at its shell prompt.
    pub(crate) fn herdr_release(&mut self) {
        let Some(mut link) = self.herdr.take() else {
            return;
        };
        let line = link.reporter.release(host::unix_millis());
        let socket = link.socket.clone();
        // Dropping the link ends the writer thread; a report it still holds
        // carries a lower sequence number than the release, so herdr ignores
        // it whichever lands first.
        drop(link);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = herdr::send(&socket, &line, herdr::SEND_TIMEOUT);
            let _ = done_tx.send(());
        });
        let _ = done_rx.recv_timeout(herdr::RELEASE_WAIT);
    }
}
