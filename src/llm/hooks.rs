//! Running the user's lifecycle hooks — the boundary half (`docs/hooks.md`).
//!
//! The pure half ([`crate::hooks`]) decides *which* handlers an event selects,
//! *what* JSON they are fed, and *how* their answers combine. This module owns
//! the only part that touches the world: spawning each handler, writing the
//! payload to its stdin, waiting against a deadline while the turn's
//! [`CancelToken`] stays answerable, and capturing what came back.
//!
//! Two things here are load-bearing and easy to get wrong:
//!
//! 1. **A blocking hook must poll the cancel token.** These run on the
//!    backend's own OS thread, where blocking is normal — the permission gate
//!    parks there on a `Condvar` today — but only because every such park
//!    re-checks cancellation. A hook runner that waited without that would
//!    silently break Esc for as long as the hook took. The wait loop below
//!    polls on `POLL_INTERVAL`, the same 20 ms cadence `llm::exec` uses.
//! 2. **A hook child is detached from the terminal like every other.** It
//!    goes through [`crate::subprocess::spawn_shell_with`], so a hook that
//!    tries to read `/dev/tty` (a stray `sudo`) fails fast instead of printing
//!    over the live region and fighting the event loop for the keyboard.
//!
//! The [`HookSink`] trait at the bottom is the seam the agent loop sees. Every
//! method defaults to doing nothing, so [`NoHooks`] costs nothing, tests need
//! no fixture, and **adding an event is one defaulted method plus one call
//! site** rather than a wider `run_agent` signature.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::hooks::{
    CommandHook, HOOK_OUTPUT_MAX_BYTES, HookContext, HookEvent, HookOutcome, HookPermission,
    HookRun, HooksFile, ParsedHook, merge, parse_run, truncate_output,
};
use crate::stream::CancelToken;
use crate::subprocess;

use super::tools::{ToolCallRequest, ToolOutcome};

/// How often the wait loop wakes to re-check the deadline and the turn's
/// cancel flag — `llm::exec`'s `BASH_POLL_INTERVAL`, for the same reason:
/// short enough that Esc reaps promptly, long enough not to spin.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// The whole-event hard time budget of the two events that must never hold
/// anything up — `SessionEnd` (quitting, `/clear`) and `Interrupt` (an Esc):
/// Claude Code defaults the first to 1.5 s and codex clamps both to 3 s,
/// against the 600 s every other event gets.
const BOUNDED_EVENT_BUDGET: Duration = Duration::from_secs(2);

/// How long an event that ends a turn (`Interrupt`, `SessionEnd`) waits for
/// that turn's cancelled hooks to be reaped before it runs ([`InFlight`]).
/// A cancelled hook dies within a poll; the bound is for the case where it
/// cannot, so the event is late rather than lost.
const REAP_WAIT: Duration = Duration::from_secs(1);

/// The project-root variable a hook script reads. The `CLAUDE_PROJECT_DIR`
/// alias is set beside it deliberately: the point of implementing Claude
/// Code's contract is that a script written for it works here unchanged, and
/// finding the project root is the first thing such a script does.
const PROJECT_DIR_VARS: &[&str] = &["ALTER_ZERO_PROJECT_DIR", "CLAUDE_PROJECT_DIR"];

/// The running binary, as every hook child is told it — herdr's
/// `HERDR_BIN_PATH` idea (`docs/herdr.md`): an integration's hook command
/// names `"$ALTER_ZERO_BIN_PATH" …` instead of a path written down when it
/// was installed, so it never goes stale when the binary moves or updates.
const BIN_PATH_VAR: &str = "ALTER_ZERO_BIN_PATH";

/// The path `exe` (`std::env::current_exe`) names, minus the ` (deleted)`
/// Linux appends once the file under a running process is replaced — an
/// `alter-zero update` — since the path is exactly where the new one is.
#[must_use]
pub fn running_binary(exe: PathBuf) -> PathBuf {
    match exe
        .to_str()
        .and_then(|text| text.strip_suffix(" (deleted)"))
    {
        Some(path) => PathBuf::from(path),
        None => exe,
    }
}

/// What the agent loop does about a `PreToolUse` verdict.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreToolVerdict {
    /// Refuse the call. The cell shows the short form, the model reads the
    /// long one — the [`crate::permission::Approval::Reject`] split.
    pub blocked: Option<String>,
    /// Skip the permission gate for this call (`permissionDecision: "allow"`).
    pub pre_approved: bool,
    /// Put it to the user even if a standing rule would have allowed it
    /// (`permissionDecision: "ask"`).
    pub force_ask: bool,
    /// A rewritten `tool_input`, as JSON object text.
    pub updated_input: Option<String>,
    /// Appended to what the model reads back from the call.
    pub context: Option<String>,
    /// A dim `⎿` row for the user, and the record of why this happened.
    pub note: Option<String>,
}

/// What the agent loop does about a `PostToolUse` verdict. The call already
/// ran, so there is nothing to refuse — only things to say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PostToolVerdict {
    /// Appended to what the model reads back from the call.
    pub context: Option<String>,
    /// A dim `⎿` row for the user, and the record.
    pub note: Option<String>,
}

impl PostToolVerdict {
    /// Nothing to do downstream.
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        self.context.is_none() && self.note.is_none()
    }
}

/// What a `UserPromptSubmit` hook said about the prompt (`docs/hooks.md`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptVerdict {
    /// Refuse the prompt: the turn never reaches the model, the submission
    /// rolls back, the reason is the red notice. (`continue: false` maps
    /// here too — codex's reading; the prompt is never recorded.)
    pub blocked: Option<String>,
    /// `additionalContext` entries, in handler order — injected **even on a
    /// block**, both references' rule.
    pub contexts: Vec<String>,
}

/// What a `PermissionRequest` hook decided in the user's stead, if anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookPermissionVerdict {
    /// Run it; `note` records that a hook — not the user — said so.
    Allow { note: String },
    /// Refuse it, with the cell text and the model-facing text.
    Deny { display: String, result: String },
}

/// Who decided a permission request — `PermissionResult`'s
/// `decision.source` (`docs/hooks.md`), so an observer can tell a prompt the
/// user answered from one nobody was ever shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionSource {
    /// The user answered the prompt.
    User,
    /// A `PermissionRequest` hook decided in the user's stead.
    Hook,
    /// Auto mode's classifier decided (`docs/permissions.md`).
    Classifier,
}

impl PermissionSource {
    /// The wire spelling.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Hook => "hook",
            Self::Classifier => "classifier",
        }
    }
}

/// `StopFailure`'s `error` — Claude Code's error types, read off what the
/// backend reported (`docs/hooks.md`). Also the event's matcher, so a hook
/// can select `rate_limit` alone. A failure this app classifies no further
/// is `unknown`, the reference's own catch-all.
#[must_use]
pub fn stop_failure_kind(error: &super::LlmError) -> &'static str {
    use super::LlmError;
    match error {
        LlmError::Api { status, .. } => match status {
            429 => "rate_limit",
            529 => "overloaded",
            401 | 403 => "authentication_failed",
            402 => "billing_error",
            404 => "model_not_found",
            400..=499 => "invalid_request",
            _ => "server_error",
        },
        LlmError::Http(_) | LlmError::Unreachable { .. } => "server_error",
        LlmError::Decode(_) | LlmError::Cancelled => "unknown",
    }
}

/// The `Notification` a permission prompt raises — Claude Code's title and
/// message, with this app's name in it (`crate::APP_NAME`).
const PERMISSION_PROMPT_TITLE: &str = "Permission needed";

/// The seam the agent loop and the approval gate consult.
///
/// Every method defaults to "nothing happened", which is why [`NoHooks`] is a
/// unit struct and why no existing test needed touching. A new event is a new
/// defaulted method plus its one call site — if a later change makes that
/// untrue, the change is wrong (`docs/hooks.md`).
pub trait HookSink: Send + Sync + std::fmt::Debug {
    /// Before a call is put to the permission gate.
    fn pre_tool_use(&self, _call: &ToolCallRequest, _cancel: &CancelToken) -> PreToolVerdict {
        PreToolVerdict::default()
    }

    /// After a call ran, with the outcome it produced.
    fn post_tool_use(
        &self,
        _call: &ToolCallRequest,
        _outcome: &ToolOutcome,
        _cancel: &CancelToken,
    ) -> PostToolVerdict {
        PostToolVerdict::default()
    }

    /// After a call ran and **failed** — `PostToolUse`'s twin, with the same
    /// answers (Claude Code's `PostToolUseFailure`).
    fn post_tool_use_failure(
        &self,
        _call: &ToolCallRequest,
        _outcome: &ToolOutcome,
        _cancel: &CancelToken,
    ) -> PostToolVerdict {
        PostToolVerdict::default()
    }

    /// Instead of asking the user. `None` falls through to whatever would
    /// have happened anyway — the classifier, then the prompt.
    fn permission_request(
        &self,
        _call: &ToolCallRequest,
        _cancel: &CancelToken,
    ) -> Option<HookPermissionVerdict> {
        None
    }

    /// The permission prompt for `call` is on screen now — the
    /// `Notification` event's `permission_prompt` type. Fired after the
    /// prompt is raised and before the gate waits; it must not hold the gate
    /// up (the command sink queues it on the [`HookLane`], behind which the
    /// `Interrupt` of an Esc on this prompt waits). Observation only.
    fn permission_prompt(&self, _call: &ToolCallRequest, _cancel: &CancelToken) {}

    /// A `PermissionRequest` was decided — by the user, a hook or the
    /// classifier (`PermissionResult`). Every request that is decided gets
    /// one, which is what lets a tracker pair them; one reaped by a cancel
    /// gets none (the `Interrupt` that follows is the record). Observation
    /// only.
    fn permission_result(
        &self,
        _call: &ToolCallRequest,
        _allowed: bool,
        _source: PermissionSource,
        _cancel: &CancelToken,
    ) {
    }

    /// A subagent was launched; the returned strings are extra context for
    /// **that** agent's own conversation.
    ///
    /// Takes the agent's own [`CancelToken`], like every other blocking method
    /// here: an Esc while a subagent is starting must reap its hook rather
    /// than wait the handler's timeout out.
    fn subagent_start(
        &self,
        _agent_id: &str,
        _agent_type: &str,
        _cancel: &CancelToken,
    ) -> Vec<String> {
        Vec::new()
    }

    /// The model finished answering — `Stop` for the lead sink,
    /// `SubagentStop` for a [`for_subagent`](Self::for_subagent)-tagged one
    /// (Claude Code chooses the event the same way, by the presence of an
    /// agent id). Fired by `run_agent` at the point the turn would complete
    /// — never on a cancel, an error, a compact turn or a `!` shell.
    ///
    /// `Some(feedback)` is a **block**: the agent keeps going, the feedback
    /// its next user message. `stop_hook_active` is true on the second and
    /// later firings within one turn — the loop guard's flag, which the hook
    /// itself checks to avoid looping forever (the engine never refuses a
    /// re-block; that is Claude Code's exact posture, and Esc stays the stop
    /// button).
    fn stop(
        &self,
        _stop_hook_active: bool,
        _last_message: &str,
        _cancel: &CancelToken,
    ) -> Option<String> {
        None
    }

    /// The turn ended on a backend error instead of an answer — Claude
    /// Code's `StopFailure`, `error` the [`stop_failure_kind`] (the matcher)
    /// and `details` the text the conversation shows. Fired by `run_agent`
    /// just before it reports the error, the lead's turn only (a subagent's
    /// failed round is not the session's turn ending). Observation only.
    fn stop_failure(&self, _error: &str, _details: &str, _cancel: &CancelToken) {}

    /// The user interrupted the running turn (codex's `Interrupt`) — fired
    /// by the boundary, never by the loop it interrupted, and bounded like
    /// [`session_end`](Self::session_end): an Esc must never wait on a hook.
    /// Observation only.
    fn interrupt(&self) {}

    /// The `SessionStart` context for the turn about to run, at its top on
    /// the backend thread. The boundary fires the event itself, in the
    /// background as it crosses `startup` / `resume` / `clear`
    /// ([`CommandHooks::fire_session_start`]) — never blocking the first
    /// paint, the checkpoint probe's lesson — so this waits for those hooks
    /// and takes what they returned, then runs any source still queued (one
    /// crossed before a real backend existed). Returns the hooks'
    /// `additionalContext` strings; the matcher gates on the source.
    fn session_start(&self, _cancel: &CancelToken) -> Vec<String> {
        Vec::new()
    }

    /// The user submitted `prompt` — fired right after the SessionStart
    /// drain, before the first request. Never fired for a loop-initiated
    /// turn (a background completion's follow-up), a `/compact`
    /// summarization, a subagent's prompt, or a `!` command — none of those
    /// is a user prompt (Claude Code's bash-mode skips it too).
    fn user_prompt_submit(&self, _prompt: &str, _cancel: &CancelToken) -> PromptVerdict {
        PromptVerdict::default()
    }

    /// A view of this sink that reports its calls as coming from a subagent,
    /// so `agent_id` / `agent_type` reach every payload it builds. `None` —
    /// the default — means there is nothing to re-tag and the caller keeps
    /// the sink it has. Object-safe on purpose: the backend holds an
    /// `Arc<dyn HookSink>` and must be able to call this through it.
    fn for_subagent(&self, _agent_id: &str, _agent_type: &str) -> Option<Arc<dyn HookSink>> {
        None
    }

    /// The session is closing (`reason`: `clear` — a `/clear` starting a
    /// fresh conversation — or `prompt_input_exit`, a quit). Envelope-only,
    /// output ignored, and **bounded**: quitting must never hang on a hook,
    /// so the whole event runs under a hard budget (Claude Code caps it at
    /// 1.5 s, codex clamps to 3 s; ours is `BOUNDED_EVENT_BUDGET`, 2 s). No
    /// cancel token — the session is past cancellation.
    fn session_end(&self, _reason: &str) {}

    /// The event name [`turn_start`] stamps on this sink's prompt-time
    /// context — `UserPromptSubmit` everywhere except the compact wrapper,
    /// whose "prompt" hook is really `PreCompact` (`docs/hooks.md`).
    fn prompt_hook_label(&self) -> &'static str {
        "UserPromptSubmit"
    }
}

/// The no-op sink: what a session with hooks disabled (or an embedder that
/// built the backend directly) uses. Costs one vtable dispatch per call site
/// and nothing else.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHooks;

impl HookSink for NoHooks {}

/// The compact turn's sink (`docs/hooks.md`): the summarization spawn maps
/// the turn lifecycle onto the compact events — its "prompt" hook is
/// `PreCompact` (whose context strings become extra summarization
/// instructions) and its completion is `PostCompact`. Everything else stays
/// quiet: the compact backend is tools-free, drains no session sources, and
/// never fires `Stop` (the wrapper's `stop` **is** PostCompact and never
/// blocks — a summarization must not be continued by a hook).
#[derive(Debug)]
pub struct CompactHooks {
    inner: CommandHooks,
    /// `manual` or `auto` — the payload's `trigger`, and its matcher.
    trigger: &'static str,
}

impl CompactHooks {
    #[must_use]
    pub fn new(inner: CommandHooks, auto: bool) -> Self {
        Self {
            inner,
            trigger: if auto { "auto" } else { "manual" },
        }
    }
}

impl HookSink for CompactHooks {
    fn user_prompt_submit(&self, _prompt: &str, cancel: &CancelToken) -> PromptVerdict {
        PromptVerdict {
            blocked: None,
            contexts: self.inner.pre_compact(self.trigger, "", cancel),
        }
    }

    fn stop(
        &self,
        _stop_hook_active: bool,
        last_message: &str,
        cancel: &CancelToken,
    ) -> Option<String> {
        self.inner.post_compact(self.trigger, last_message, cancel);
        None
    }

    /// A summarization that failed still ended a turn the session started:
    /// the lead's `StopFailure`, so a tracker that saw `PreCompact` sees its
    /// end (`docs/hooks.md`).
    fn stop_failure(&self, error: &str, details: &str, cancel: &CancelToken) {
        self.inner.stop_failure(error, details, cancel);
    }

    fn prompt_hook_label(&self) -> &'static str {
        "PreCompact"
    }
}

/// The conversation the recorder is writing, as the payloads report it
/// (`docs/hooks.md`): its **id** — the rollout file's, the handle `--resume`
/// takes, minted before the file exists so a fresh conversation has one from
/// its first event — and its **file** once anything was recorded (deferred
/// create). Both move together: a `/clear` starts a new id with no file, a
/// `/resume` adopts the resumed file's pair.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Conversation {
    /// `None` until the recorder publishes — the payload then keeps the
    /// context's stored id.
    pub id: Option<String>,
    /// `None` while the conversation has no file yet.
    pub transcript_path: Option<String>,
}

/// The [`Conversation`] the recorder publishes for hook payloads. Shared
/// because the sink outlives — and predates — the file, and is rebuilt on
/// every `/model` switch while the conversation goes on.
pub type ConversationCell = Arc<std::sync::RwLock<Conversation>>;

/// The pending `SessionStart` sources (`startup` / `resume` / `clear`),
/// queued by the boundary — fired in the background on the [`HookLane`] the
/// moment the boundary can ([`CommandHooks::fire_session_start`]), and
/// drained at the next turn's top for whatever could not be (a session that
/// had no real backend yet). Nothing ever blocks the first paint.
pub type SessionSources = Arc<std::sync::Mutex<Vec<String>>>;

/// The ordered lane the **boundary's** events run on (`docs/hooks.md`):
/// `SessionStart` at launch, `/clear` and `/resume`, `Interrupt` on an Esc,
/// `SessionEnd` — events the UI thread fires and must never wait on — and
/// the permission prompt's `Notification`, which an Esc on that prompt must
/// never overtake.
///
/// Two guarantees, and they are the point. Each job runs **off the caller's
/// thread** and **in submission order**, on a thread of its own that waits
/// its ticket. And every hook the backend dispatches afterwards
/// (`CommandHooks::dispatch`) first waits for the lane to drain — so an
/// `Interrupt` fired on an Esc can never land after the `UserPromptSubmit`
/// of the turn the user started right after it, which is the order a status
/// tracker reads the session in (`docs/herdr.md`). A job must never wait on
/// the lane itself (it is the lane); the jobs dispatch without waiting.
#[derive(Debug, Clone, Default)]
pub struct HookLane {
    inner: Arc<LaneInner>,
}

#[derive(Debug, Default)]
struct LaneInner {
    state: std::sync::Mutex<LaneState>,
    /// Woken whenever a ticket finishes.
    turn: std::sync::Condvar,
}

#[derive(Debug, Default)]
struct LaneState {
    /// The next ticket to hand out.
    issued: u64,
    /// The ticket whose turn it is — every one below it has finished.
    serving: u64,
    /// Tickets below this are skipped when their turn comes
    /// ([`HookLane::cancel_pending`]).
    dropped_below: u64,
    /// The running job's token, so a cancel can reap it.
    running: Option<CancelToken>,
}

impl HookLane {
    fn lock(&self) -> std::sync::MutexGuard<'_, LaneState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Queue `job` behind every job submitted before it. Never blocks: the
    /// job runs on a thread of its own, handed a token
    /// [`cancel_pending`](Self::cancel_pending) cancels.
    pub fn submit(&self, job: impl FnOnce(&CancelToken) + Send + 'static) {
        self.submit_under(CancelToken::new(), job);
    }

    /// [`submit`](Self::submit), with the job run under `token` — a turn's,
    /// for a job that turn's Esc must reap. [`cancel_pending`](Self::cancel_pending)
    /// cancels it all the same: a quit tears that turn down anyway.
    pub fn submit_under(
        &self,
        token: CancelToken,
        job: impl FnOnce(&CancelToken) + Send + 'static,
    ) {
        let ticket = {
            let mut state = self.lock();
            let ticket = state.issued;
            state.issued += 1;
            ticket
        };
        let lane = self.clone();
        std::thread::spawn(move || {
            {
                let mut state = lane.lock();
                while state.serving != ticket {
                    state = lane
                        .inner
                        .turn
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                if ticket < state.dropped_below {
                    state.serving += 1;
                    lane.inner.turn.notify_all();
                    return;
                }
                state.running = Some(token.clone());
            }
            job(&token);
            let mut state = lane.lock();
            state.running = None;
            state.serving += 1;
            lane.inner.turn.notify_all();
        });
    }

    /// Block until every submitted job has run — `false` when `cancelled`
    /// gave up first. Polled on `POLL_INTERVAL`, so a turn's Esc reaps a
    /// wait on a slow boundary hook the way it reaps a slow hook of its own.
    pub fn wait_idle(&self, cancelled: &dyn Fn() -> bool) -> bool {
        let mut state = self.lock();
        loop {
            if state.serving == state.issued {
                return true;
            }
            if cancelled() {
                return false;
            }
            let (next, _) = self
                .inner
                .turn
                .wait_timeout(state, POLL_INTERVAL)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
        }
    }

    /// [`wait_idle`](Self::wait_idle) under a deadline.
    pub fn wait_idle_for(&self, budget: Duration) -> bool {
        let started = Instant::now();
        self.wait_idle(&|| started.elapsed() >= budget)
    }

    /// Reap the running job and drop every queued one — what quitting does
    /// before its `SessionEnd`, which must not wait behind a slow
    /// `SessionStart`. Jobs submitted afterwards run as usual.
    pub fn cancel_pending(&self) {
        let mut state = self.lock();
        state.dropped_below = state.issued;
        if let Some(token) = &state.running {
            token.cancel();
        }
    }
}

/// The backend's hook dispatches running right now, each by its turn's
/// token (`docs/hooks.md` *The lane*). An Esc cancels the turn, and the
/// runner reaps its hooks within a poll — but until then one can still
/// write, so the event that ends the turn waits here for every dispatch
/// whose token is cancelled to finish ([`wait_reaped`](Self::wait_reaped)),
/// and no tracker hears the turn's last *working* after its *idle*.
/// Dispatches of a turn still running (a background agent's) are not
/// waited for: nothing ended them.
#[derive(Debug, Clone, Default)]
pub struct InFlight {
    inner: Arc<(std::sync::Mutex<Vec<CancelToken>>, std::sync::Condvar)>,
}

impl InFlight {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<CancelToken>> {
        self.inner
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Count a dispatch under `cancel` as running until the guard drops.
    pub fn enter(&self, cancel: &CancelToken) -> InFlightGuard {
        self.lock().push(cancel.clone());
        InFlightGuard {
            in_flight: self.clone(),
            cancel: cancel.clone(),
        }
    }

    /// Block until no running dispatch belongs to a cancelled turn, or
    /// `budget` passes — `false` then.
    pub fn wait_reaped(&self, budget: Duration) -> bool {
        let started = Instant::now();
        let mut running = self.lock();
        loop {
            if running.iter().all(|token| !token.is_cancelled()) {
                return true;
            }
            let Some(left) = budget.checked_sub(started.elapsed()) else {
                return false;
            };
            // Re-checked on the poll as well as on a wake: a token can be
            // cancelled while nothing here changes.
            running = self
                .inner
                .1
                .wait_timeout(running, left.min(POLL_INTERVAL))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

/// A running dispatch, counted in its [`InFlight`] until dropped.
#[derive(Debug)]
pub struct InFlightGuard {
    in_flight: InFlight,
    cancel: CancelToken,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut running = self.in_flight.lock();
        if let Some(at) = running.iter().position(|token| *token == self.cancel) {
            running.swap_remove(at);
        }
        drop(running);
        self.in_flight.inner.1.notify_all();
    }
}

/// The live handles a [`CommandHooks`] resolves per dispatch — shared with
/// the boundary so the payloads report the session as it **is**, not as it
/// was when the sink was built (a `/model` switch rebuilds sinks; these
/// survive the rebuild).
#[derive(Debug, Clone, Default)]
pub struct HookHandles {
    /// The permission gate, read per payload so a Shift+Tab cycle reaches the
    /// very next call. `None` (gate off, an embedder) keeps the context's
    /// own value.
    pub gate: Option<crate::permission::PermissionGate>,
    /// The conversation's id and rollout path, as the recorder publishes them.
    pub conversation: ConversationCell,
    /// The queued `SessionStart` sources.
    pub sources: SessionSources,
    /// The lane the boundary's events run on, in order, off the UI thread —
    /// every backend dispatch waits for it first.
    pub lane: HookLane,
    /// `additionalContext` from `SessionStart` hooks fired at the boundary,
    /// waiting for the next turn to inject it (`docs/hooks.md`).
    pub held: Arc<std::sync::Mutex<Vec<String>>>,
    /// The backend dispatches running now — what an `Interrupt` or a
    /// `SessionEnd` waits on, so none of the ended turn's hooks lands after.
    pub in_flight: InFlight,
    /// Set by the boundary just before it dispatches a **loop-initiated**
    /// turn (a background completion's follow-up): the next
    /// `user_prompt_submit` is skipped — that prompt is synthesized, not the
    /// user's. Cleared by the skip.
    pub synthetic_turn: Arc<std::sync::atomic::AtomicBool>,
}

impl HookHandles {
    /// Forget the boundary-fired `SessionStart` context no turn has read yet
    /// — the conversation it was for is gone (`/clear`, `/resume`). Ordered
    /// on the lane, so a firing still running is forgotten too.
    pub fn forget_held_contexts(&self) {
        let held = Arc::clone(&self.held);
        self.lane.submit(move |_| {
            if let Ok(mut held) = held.lock() {
                held.clear();
            }
        });
    }
}

/// The real sink: a loaded `hooks.json` plus everything a payload needs.
#[derive(Debug, Clone)]
pub struct CommandHooks {
    file: Arc<HooksFile>,
    context: HookContext,
    /// The detach-helper path threaded from `main` — see [`subprocess`].
    detach_helper: Option<PathBuf>,
    /// Where handlers run, and what `*_PROJECT_DIR` is set to.
    cwd: PathBuf,
    /// The live handles — gate, transcript path, pending session sources —
    /// resolved at **dispatch** time, so a payload reports the session as it
    /// is when the hook fires.
    handles: HookHandles,
}

impl CommandHooks {
    /// Build a sink over `file`. Returns `None` when there is nothing that
    /// could ever run, so the caller keeps [`NoHooks`] and pays nothing.
    #[must_use]
    pub fn new(
        file: Arc<HooksFile>,
        context: HookContext,
        detach_helper: Option<PathBuf>,
        cwd: PathBuf,
        handles: HookHandles,
    ) -> Option<Self> {
        if file.is_empty() {
            return None;
        }
        Some(Self {
            file,
            context,
            detach_helper,
            cwd,
            handles,
        })
    }

    /// The context as of **now**: the stored session facts with the
    /// permission mode and the conversation — its id and its rollout path —
    /// resolved from their live handles. Every payload builds from this,
    /// never from the frozen snapshot.
    fn live_context(&self) -> HookContext {
        let mut context = self.context.clone();
        if let Some(gate) = &self.handles.gate {
            context.permission_mode = Some(gate.mode().label().to_string());
        }
        if let Ok(conversation) = self.handles.conversation.read() {
            if let Some(id) = &conversation.id {
                context.session_id.clone_from(id);
            }
            // The file's absence is a fact too: a `/clear`'s fresh
            // conversation has an id and no file yet.
            if conversation.id.is_some() || conversation.transcript_path.is_some() {
                context
                    .transcript_path
                    .clone_from(&conversation.transcript_path);
            }
        }
        context
    }

    /// Run every handler `event` selects for `query`, in order, and merge
    /// their answers. Handlers run **sequentially**: they can block each
    /// other's tool call anyway, and a bounded pool is a phase-two concern.
    ///
    /// The boundary's events go first: whatever is on the [`HookLane`] —
    /// an `Interrupt`, a `SessionStart` fired at launch — drains before this
    /// event's handlers run, so no hook ever reaches a tracker ahead of one
    /// the session fired before it.
    fn dispatch(
        &self,
        event: HookEvent,
        query: Option<&str>,
        payload: &str,
        cancel: &CancelToken,
    ) -> HookOutcome {
        self.handles.lane.wait_idle(&|| cancel.is_cancelled());
        let _running = self.handles.in_flight.enter(cancel);
        self.dispatch_now(event, query, payload, cancel)
    }

    /// [`dispatch`](Self::dispatch) without waiting on the lane — what a job
    /// **on** the lane uses, since waiting there would wait on itself.
    fn dispatch_now(
        &self,
        event: HookEvent,
        query: Option<&str>,
        payload: &str,
        cancel: &CancelToken,
    ) -> HookOutcome {
        let selection = self.file.select(event, query);
        let mut parsed: Vec<ParsedHook> = selection
            .warnings
            .into_iter()
            .map(|warning| ParsedHook {
                warning: Some(warning),
                ..ParsedHook::default()
            })
            .collect();
        for handler in &selection.handlers {
            if cancel.is_cancelled() {
                break;
            }
            let run = self.run_handler(handler, payload, cancel);
            parsed.push(parse_run(&run, event));
        }
        merge(parsed)
    }

    /// Spawn one handler, feed it `payload` on stdin, and wait for it.
    fn run_handler(&self, handler: &CommandHook, payload: &str, cancel: &CancelToken) -> HookRun {
        let mut run = HookRun {
            command: handler.command.clone(),
            ..HookRun::default()
        };
        let cwd = self.cwd.clone();
        let bin = std::env::current_exe().ok().map(running_binary);
        let spawned = subprocess::spawn_shell_with(
            self.detach_helper.as_deref(),
            &handler.command,
            move |cmd| {
                cmd.stdin(std::process::Stdio::piped()).current_dir(&cwd);
                for var in PROJECT_DIR_VARS {
                    cmd.env(var, &cwd);
                }
                if let Some(bin) = &bin {
                    cmd.env(BIN_PATH_VAR, bin);
                }
            },
        );
        let mut child = match spawned {
            Ok(child) => child,
            Err(err) => {
                run.error = Some(format!("failed to spawn: {err}"));
                return run;
            }
        };

        // Write the payload on its own thread and close the pipe, so a handler
        // blocked on EOF (`cat`, `jq`, `read`) proceeds. A `BrokenPipe` means
        // the handler exited without reading its input, which is legitimate —
        // a guard that only cares *that* it was called never reads stdin.
        //
        // A thread, not an inline `write_all`: a payload past the pipe buffer
        // (a `PostToolUse` carrying a 64 KiB tool output) fed to a handler
        // that never reads stdin would park an inline write until the child
        // exited — before the deadline loop below, so neither Esc nor the
        // timeout could reap it. The wait loop owns cancellation; the writer
        // just finishes (or breaks) when the child is gone.
        let stdin_writer = child.stdin.take().map(|mut stdin| {
            let payload = payload.to_string();
            std::thread::spawn(move || {
                let wrote = stdin
                    .write_all(payload.as_bytes())
                    .and_then(|()| stdin.flush());
                match wrote {
                    Ok(()) => None,
                    Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => None,
                    Err(err) => Some(format!("failed to write payload: {err}")),
                }
            })
        });

        // Drain both pipes on their own threads: a handler that prints more
        // than a pipe buffer would otherwise deadlock against our wait.
        let out_pipe = child.stdout.take();
        let out_reader = std::thread::spawn(move || read_pipe(out_pipe));
        let err_pipe = child.stderr.take();
        let err_reader = std::thread::spawn(move || read_pipe(err_pipe));

        let deadline = Duration::from_secs(handler.timeout_secs);
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(err) => {
                    // Reap before bailing: the readers (and the writer) hold
                    // the pipes, and a child left running would park their
                    // joins below — codex kills on a wait error for the same
                    // reason.
                    subprocess::kill_process_group(&mut child);
                    run.error = Some(format!("failed to wait: {err}"));
                    break None;
                }
            }
            // Esc, `/clear`, a quit: reap the whole tree and stop. The turn is
            // being torn down, so the verdict is moot — but a hook left
            // running would keep a reader thread alive holding the pipe.
            if cancel.is_cancelled() {
                subprocess::kill_process_group(&mut child);
                run.error = Some("interrupted".to_string());
                break None;
            }
            if started.elapsed() >= deadline {
                subprocess::kill_process_group(&mut child);
                run.error = Some(format!("timed out after {}s", handler.timeout_secs));
                break None;
            }
            std::thread::sleep(POLL_INTERVAL);
        };

        // The group kill above has reaped anything holding the pipes, so the
        // joins terminate — the writer's pipe end is closed by the child's
        // death too, so it can never outlive this point.
        run.stdout = out_reader.join().unwrap_or_default();
        run.stderr = err_reader.join().unwrap_or_default();
        let write_error = stdin_writer.and_then(|writer| writer.join().unwrap_or_default());
        if run.error.is_none() {
            run.error = write_error;
        }
        if run.error.is_none() {
            run.exit_code = status.and_then(|s| s.code());
            if run.exit_code.is_none() {
                run.error = Some("killed by a signal".to_string());
            }
        }
        run
    }

    /// `PreCompact` (`docs/hooks.md`): fired by the compact wrapper on the
    /// summarization spawn's thread. It cannot block — **in either
    /// reference** (Claude Code's own in-app docs claim exit 2 blocks
    /// compaction, but the `blocked` field is never read at any call site;
    /// codex has no exit-2 arm for it) — its real contract is that stdout /
    /// `additionalContext` become extra instructions for the summarization
    /// prompt, which is what the returned strings are.
    fn pre_compact(&self, trigger: &str, custom: &str, cancel: &CancelToken) -> Vec<String> {
        let payload = crate::hooks::pre_compact_payload(&self.live_context(), trigger, custom);
        self.dispatch(HookEvent::PreCompact, Some(trigger), &payload, cancel)
            .additional_context
    }

    /// `PostCompact` (`docs/hooks.md`): the summary that replaced the
    /// conversation, envelope-only — nothing to return.
    fn post_compact(&self, trigger: &str, summary: &str, cancel: &CancelToken) {
        let payload = crate::hooks::post_compact_payload(&self.live_context(), trigger, summary);
        let _ = self.dispatch(HookEvent::PostCompact, Some(trigger), &payload, cancel);
    }

    /// Drain the queued `SessionStart` sources and fire each, returning the
    /// hooks' context strings — the turn-top drain, for the sources the
    /// boundary could not fire itself.
    fn run_session_sources(&self, cancel: &CancelToken) -> Vec<String> {
        let sources: Vec<String> = match self.handles.sources.lock() {
            Ok(mut queue) => queue.drain(..).collect(),
            Err(_) => Vec::new(),
        };
        let mut contexts = Vec::new();
        for source in sources {
            let payload = crate::hooks::session_start_payload(&self.live_context(), &source);
            let outcome = self.dispatch(HookEvent::SessionStart, Some(&source), &payload, cancel);
            contexts.extend(outcome.additional_context);
        }
        contexts
    }

    /// A copy that reports the session **as it is now**, whenever it runs:
    /// the context resolved from the live handles once, and those handles
    /// let go. What a boundary event queued on the lane runs as — `/resume`
    /// fires `SessionEnd` and adopts the next conversation in the same
    /// breath, and the event must still name the conversation that ended.
    fn pinned(&self) -> Self {
        let mut pinned = self.clone();
        pinned.context = self.live_context();
        pinned.handles.conversation = ConversationCell::default();
        pinned.handles.gate = None;
        pinned
    }

    /// Fire the queued `SessionStart` sources **now**, in the background on
    /// the lane — Claude Code's timing (*runs in the background at
    /// launch/resume, allowing immediate user input*), so a session tracker
    /// hears of the session before its first prompt. The context the hooks
    /// return waits in [`HookHandles::held`] for the next turn, whose
    /// [`session_start`](HookSink::session_start) waits for this first.
    pub fn fire_session_start(&self) {
        let pinned = self.pinned();
        // The queue is drained now, against the conversation as it is now —
        // a `/clear` before the job runs queues (and fires) its own.
        let jobs: Vec<(String, String)> = match self.handles.sources.lock() {
            Ok(mut queue) => queue
                .drain(..)
                .map(|source| {
                    let payload = crate::hooks::session_start_payload(&pinned.context, &source);
                    (source, payload)
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        if jobs.is_empty() {
            return;
        }
        self.handles.lane.submit(move |cancel| {
            let mut contexts = Vec::new();
            for (source, payload) in jobs {
                let outcome =
                    pinned.dispatch_now(HookEvent::SessionStart, Some(&source), &payload, cancel);
                contexts.extend(outcome.additional_context);
            }
            if let Ok(mut held) = pinned.handles.held.lock() {
                held.extend(contexts);
            }
        });
    }

    /// `Interrupt`, on the lane: the UI thread never waits on it, and no hook
    /// the next turn dispatches can overtake it.
    pub fn fire_interrupt(&self) {
        let pinned = self.pinned();
        self.handles.lane.submit(move |_| {
            // The interrupted turn's own hooks go first ([`InFlight`]).
            pinned.handles.in_flight.wait_reaped(REAP_WAIT);
            pinned.interrupt();
        });
    }

    /// `SessionEnd`, on the lane — ahead of whatever the boundary fires next
    /// (the `SessionStart` of the conversation that replaces this one).
    pub fn fire_session_end(&self, reason: &str) {
        let pinned = self.pinned();
        let reason = reason.to_string();
        self.handles.lane.submit(move |_| {
            // A turn the session's end cancelled ends first ([`InFlight`]).
            pinned.handles.in_flight.wait_reaped(REAP_WAIT);
            pinned.session_end(&reason);
        });
    }

    /// `SessionEnd` at a **quit**: the lane's queued work is dropped and its
    /// running job reaped (a slow `SessionStart` must not eat the budget),
    /// then the event runs and the caller waits for it — bounded, so a quit
    /// never hangs on a hook. `true` when it finished inside the budget.
    pub fn end_session_at_exit(&self, reason: &str) -> bool {
        self.handles.lane.cancel_pending();
        self.fire_session_end(reason);
        self.handles
            .lane
            .wait_idle_for(BOUNDED_EVENT_BUDGET + Duration::from_millis(500))
    }

    /// Run an event's handlers under the **whole-event** budget
    /// ([`BOUNDED_EVENT_BUDGET`]) rather than each handler's own timeout —
    /// the events that must never hold anything up (`SessionEnd`: quitting;
    /// `Interrupt`: an Esc). Output is ignored; past cancellation, so the
    /// budget is the only stop.
    fn run_bounded(&self, event: HookEvent, query: Option<&str>, payload: &str) {
        let selection = self.file.select(event, query);
        let started = Instant::now();
        let cancel = CancelToken::new();
        for handler in &selection.handlers {
            let remaining = BOUNDED_EVENT_BUDGET.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                break;
            }
            // Each handler gets what is left of the event budget, never more
            // than its own timeout (and at least the 1 s floor a zero would
            // defeat).
            let capped = CommandHook {
                timeout_secs: handler.timeout_secs.min(remaining.as_secs().max(1)),
                ..handler.clone()
            };
            let _ = self.run_handler(&capped, payload, &cancel);
        }
    }

    /// A `HookContext` naming the subagent that is acting.
    fn tagged(&self, agent_id: &str, agent_type: &str) -> Self {
        let mut clone = self.clone();
        clone.context.agent_id = Some(agent_id.to_string());
        clone.context.agent_type = Some(agent_type.to_string());
        clone
    }
}

/// Read a pipe to a lossy `String`, **capped in memory as it is read**
/// (`tui::shell::append_capped`'s rule — a hook that spews gigabytes must not
/// spike RSS): the first [`HOOK_OUTPUT_MAX_BYTES`] are kept, the rest is
/// drained and dropped so the child never blocks on a full pipe, and
/// [`truncate_output`] stamps the marker. An absent pipe reads as empty.
fn read_pipe<R: Read>(pipe: Option<R>) -> String {
    let Some(mut pipe) = pipe else {
        return String::new();
    };
    // One byte past the cap, so `truncate_output` can tell "exactly full"
    // from "overflowed" and only mark the latter.
    let mut kept: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = (HOOK_OUTPUT_MAX_BYTES + 1).saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..n.min(room)]);
            }
        }
    }
    truncate_output(&String::from_utf8_lossy(&kept))
}

/// The cell/model text pair for a hook refusal — the same two-text shape the
/// permission gate's rejections use, so the whole recording, transcript and
/// `/resume` path is the one that already exists.
#[must_use]
pub fn block_texts(reason: &str) -> (String, String) {
    (
        format!("Blocked by hook: {reason}"),
        format!(
            "A configured lifecycle hook blocked this tool call.\n\nReason: {reason}\n\n\
             Do not retry the same call. Address the reason, or explain to the user why the \
             call is needed and wait for their instruction."
        ),
    )
}

/// What the turn's opening hooks decided (`docs/hooks.md`): the notes to
/// record + inject (SessionStart's first, then UserPromptSubmit's — each a
/// `(label, wire text)` pair), and the block reason when the prompt was
/// refused. The notes come back even on a block, both references' rule. The
/// glue in `LlmBackend::spawn` sends each note as a
/// [`crate::stream::StreamEvent::HookNote`] and appends it as a user message
/// after the prompt; a block sends `PromptBlocked` and returns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnStart {
    pub notes: Vec<(String, String)>,
    pub blocked: Option<String>,
}

/// Run the turn's opening hooks — the SessionStart drain, then
/// UserPromptSubmit — and fold their answers into one [`TurnStart`]. Pure
/// over the sink, so the whole spawn-top dance is testable with a fake.
#[must_use]
pub fn turn_start(hooks: &dyn HookSink, prompt: &str, cancel: &CancelToken) -> TurnStart {
    let mut notes = Vec::new();
    if let Some(note) = context_note_texts("SessionStart", &hooks.session_start(cancel)) {
        notes.push(note);
    }
    let verdict = hooks.user_prompt_submit(prompt, cancel);
    if let Some(note) = context_note_texts(hooks.prompt_hook_label(), &verdict.contexts) {
        notes.push(note);
    }
    TurnStart {
        notes,
        blocked: verdict.blocked,
    }
}

/// A hook's `additionalContext` as the transcript label + the user-role wire
/// text the model reads — Claude Code's exact `{hookName} hook additional
/// context:` wording inside its system-reminder wrapper, so a script's
/// injected facts land in the shape models already know.
#[must_use]
pub fn context_note_texts(event: &str, contexts: &[String]) -> Option<(String, String)> {
    if contexts.is_empty() {
        return None;
    }
    Some((
        format!("{event} hook"),
        format!(
            "<system-reminder>\n{event} hook additional context: {}\n</system-reminder>",
            contexts.join("\n")
        ),
    ))
}

/// A `Stop`/`SubagentStop` block's continuation feedback, as the transcript
/// label + the verbatim user-role message the model reads — Claude Code's
/// exact `Stop hook feedback:` wording, so a hook written against it drives
/// the same conversation here. The dummy scenario calls this too, keeping the
/// offline cell byte-for-byte the live one.
#[must_use]
pub fn stop_feedback_texts(reason: &str) -> (String, String) {
    (
        "Stop hook".to_string(),
        format!("Stop hook feedback:\n{reason}"),
    )
}

/// The dim `⎿` provenance row a hook's context or note leaves on the cell —
/// the auto-mode classifier's `⎿ Allowed by auto mode classifier` pattern.
fn note_line(warnings: &[String], context: Option<&String>, systems: &[String]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if context.is_some() {
        parts.push("Context added by hook".to_string());
    }
    parts.extend(systems.iter().cloned());
    parts.extend(warnings.iter().cloned());
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// A post-call event's answer, folded for the loop: the call already ran, so
/// a `block` is not a refusal — it is feedback the model must read, folded in
/// beside any context, and the user's `⎿` row says a hook spoke. Shared by
/// `PostToolUse` and `PostToolUseFailure`, which answer the same way.
fn post_verdict(hooked: HookOutcome) -> PostToolVerdict {
    if hooked.is_quiet() {
        return PostToolVerdict::default();
    }
    let mut context: Vec<String> = Vec::new();
    if let Some(reason) = &hooked.block_reason {
        context.push(format!("A lifecycle hook flagged this result: {reason}"));
    }
    context.extend(hooked.additional_context.iter().cloned());
    let context = (!context.is_empty()).then(|| context.join("\n\n"));
    let mut warnings = hooked.warnings.clone();
    if let Some(reason) = hooked.block_reason {
        warnings.insert(0, format!("Flagged by hook: {reason}"));
    }
    PostToolVerdict {
        note: note_line(&warnings, context.as_ref(), &hooked.system_messages),
        context,
    }
}

impl HookSink for CommandHooks {
    fn pre_tool_use(&self, call: &ToolCallRequest, cancel: &CancelToken) -> PreToolVerdict {
        let payload = crate::hooks::pre_tool_use_payload(
            &self.live_context(),
            &call.name,
            &call.arguments,
            &call.id,
        );
        let outcome = self.dispatch(HookEvent::PreToolUse, Some(&call.name), &payload, cancel);
        if outcome.is_quiet() {
            return PreToolVerdict::default();
        }
        let context = outcome.context_note();
        PreToolVerdict {
            blocked: outcome.block_reason,
            pre_approved: outcome.permission == Some(HookPermission::Allow),
            force_ask: outcome.permission == Some(HookPermission::Ask),
            updated_input: outcome.updated_input,
            note: note_line(
                &outcome.warnings,
                context.as_ref(),
                &outcome.system_messages,
            ),
            context,
        }
    }

    fn post_tool_use(
        &self,
        call: &ToolCallRequest,
        outcome: &ToolOutcome,
        cancel: &CancelToken,
    ) -> PostToolVerdict {
        let payload = crate::hooks::post_tool_use_payload(
            &self.live_context(),
            &call.name,
            &call.arguments,
            &outcome.output,
            outcome.ok,
            &call.id,
        );
        post_verdict(self.dispatch(HookEvent::PostToolUse, Some(&call.name), &payload, cancel))
    }

    fn post_tool_use_failure(
        &self,
        call: &ToolCallRequest,
        outcome: &ToolOutcome,
        cancel: &CancelToken,
    ) -> PostToolVerdict {
        let payload = crate::hooks::post_tool_use_failure_payload(
            &self.live_context(),
            &call.name,
            &call.arguments,
            &outcome.output,
            &call.id,
        );
        post_verdict(self.dispatch(
            HookEvent::PostToolUseFailure,
            Some(&call.name),
            &payload,
            cancel,
        ))
    }

    fn permission_request(
        &self,
        call: &ToolCallRequest,
        cancel: &CancelToken,
    ) -> Option<HookPermissionVerdict> {
        let payload = crate::hooks::permission_request_payload(
            &self.live_context(),
            &call.name,
            &call.arguments,
            &call.id,
        );
        let outcome = self.dispatch(
            HookEvent::PermissionRequest,
            Some(&call.name),
            &payload,
            cancel,
        );
        match outcome.permission? {
            HookPermission::Allow => Some(HookPermissionVerdict::Allow {
                // A broken sibling's warning rides the note — the one channel
                // this verdict has — so "my guard is broken" is never silent.
                // (A verdict-less outcome has no channel at all; its warnings
                // are dropped, the documented fail-open posture.)
                note: std::iter::once(outcome.permission_reason.map_or_else(
                    || "Allowed by hook".to_string(),
                    |reason| format!("Allowed by hook: {reason}"),
                ))
                .chain(outcome.warnings)
                .collect::<Vec<_>>()
                .join(" · "),
            }),
            HookPermission::Deny => {
                let reason = outcome
                    .block_reason
                    .or(outcome.permission_reason)
                    .unwrap_or_else(|| "denied by a hook".to_string());
                let (display, result) = block_texts(&reason);
                Some(HookPermissionVerdict::Deny { display, result })
            }
            // An explicit `ask` is the default path stated out loud.
            HookPermission::Ask => None,
        }
    }

    fn permission_prompt(&self, call: &ToolCallRequest, cancel: &CancelToken) {
        let message = format!(
            "{} needs your permission to use {}",
            crate::APP_NAME,
            super::tools::display_name(&call.name)
        );
        let payload = crate::hooks::notification_payload(
            &self.live_context(),
            "permission_prompt",
            PERMISSION_PROMPT_TITLE,
            &message,
            Some((&call.name, &call.id)),
        );
        // On the lane, under the turn's token: the gate waits at once, the
        // `Interrupt` an Esc on this very prompt fires queues behind it — so
        // no tracker ever hears *idle* before *blocked* (docs/herdr.md) —
        // and that Esc still reaps a notifier that hangs.
        let pinned = self.pinned();
        self.handles
            .lane
            .submit_under(cancel.clone(), move |cancel| {
                let _ = pinned.dispatch_now(
                    HookEvent::Notification,
                    Some("permission_prompt"),
                    &payload,
                    cancel,
                );
            });
    }

    fn permission_result(
        &self,
        call: &ToolCallRequest,
        allowed: bool,
        source: PermissionSource,
        cancel: &CancelToken,
    ) {
        let payload = crate::hooks::permission_result_payload(
            &self.live_context(),
            &call.name,
            &call.arguments,
            &call.id,
            if allowed { "allow" } else { "deny" },
            source.label(),
        );
        let _ = self.dispatch(
            HookEvent::PermissionResult,
            Some(&call.name),
            &payload,
            cancel,
        );
    }

    fn subagent_start(
        &self,
        agent_id: &str,
        agent_type: &str,
        cancel: &CancelToken,
    ) -> Vec<String> {
        let tagged = self.tagged(agent_id, agent_type);
        let payload =
            crate::hooks::subagent_start_payload(&tagged.live_context(), agent_id, agent_type);
        tagged
            .dispatch(HookEvent::SubagentStart, Some(agent_type), &payload, cancel)
            .additional_context
    }

    fn stop(
        &self,
        stop_hook_active: bool,
        last_message: &str,
        cancel: &CancelToken,
    ) -> Option<String> {
        // The sink's tag picks the event — Claude Code's own rule (`Stop`
        // for the lead, `SubagentStop` when an agent id is present); the
        // subagent event matches on its agent type, the lead's on nothing.
        let context = self.live_context();
        let (event, query, payload) = match (&context.agent_id, &context.agent_type) {
            (Some(id), Some(kind)) => (
                HookEvent::SubagentStop,
                Some(kind.clone()),
                crate::hooks::subagent_stop_payload(
                    &context,
                    id,
                    kind,
                    stop_hook_active,
                    last_message,
                ),
            ),
            _ => (
                HookEvent::Stop,
                None,
                crate::hooks::stop_payload(&context, stop_hook_active, last_message),
            ),
        };
        let outcome = self.dispatch(event, query.as_deref(), &payload, cancel);
        // `continue: false` outranks a block (codex's aggregate rule): a
        // hook that says *stop everything* is not asking for another round.
        if outcome.stopped {
            return None;
        }
        outcome.block_reason
    }

    fn session_start(&self, cancel: &CancelToken) -> Vec<String> {
        // What the boundary already fired, in the background — waited for
        // first (Claude Code's rule: the first request waits for its
        // SessionStart), so its context is this turn's.
        self.handles.lane.wait_idle(&|| cancel.is_cancelled());
        let mut contexts: Vec<String> = match self.handles.held.lock() {
            Ok(mut held) => held.drain(..).collect(),
            Err(_) => Vec::new(),
        };
        // …then whatever is still queued: a source the boundary could not
        // fire (no real backend yet) runs here, the codex drain.
        contexts.extend(self.run_session_sources(cancel));
        contexts
    }

    fn user_prompt_submit(&self, prompt: &str, cancel: &CancelToken) -> PromptVerdict {
        // A loop-initiated turn's prompt is synthesized, not the user's —
        // the boundary marked it, and the mark clears with the skip.
        if self
            .handles
            .synthetic_turn
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return PromptVerdict::default();
        }
        let payload = crate::hooks::user_prompt_submit_payload(&self.live_context(), prompt);
        // Matcher-less, both references' rule: every configured group fires.
        let outcome = self.dispatch(HookEvent::UserPromptSubmit, None, &payload, cancel);
        let blocked = outcome.block_reason.clone().or_else(|| {
            outcome.stopped.then(|| {
                outcome
                    .stop_reason
                    .clone()
                    .unwrap_or_else(|| "stopped by a hook".to_string())
            })
        });
        PromptVerdict {
            blocked,
            contexts: outcome.additional_context,
        }
    }

    fn stop_failure(&self, error: &str, details: &str, cancel: &CancelToken) {
        // The session's turn, not a subagent's round: a tagged sink is a
        // subagent's (Claude Code fires its stop events the same way).
        if self.context.agent_id.is_some() {
            return;
        }
        let payload =
            crate::hooks::stop_failure_payload(&self.live_context(), error, details, details);
        let _ = self.dispatch(HookEvent::StopFailure, Some(error), &payload, cancel);
    }

    fn interrupt(&self) {
        let payload = crate::hooks::interrupt_payload(&self.live_context());
        self.run_bounded(HookEvent::Interrupt, None, &payload);
    }

    fn session_end(&self, reason: &str) {
        let payload = crate::hooks::session_end_payload(&self.live_context(), reason);
        self.run_bounded(HookEvent::SessionEnd, Some(reason), &payload);
    }

    fn for_subagent(&self, agent_id: &str, agent_type: &str) -> Option<Arc<dyn HookSink>> {
        Some(Arc::new(self.tagged(agent_id, agent_type)))
    }
}

/// The `hooks.json` path: `ALTER_ZERO_HOOKS_FILE`, else
/// `{config_home}/hooks.json`. Mirrors how every other config file resolves.
#[must_use]
pub fn hooks_file_path(config_home: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("ALTER_ZERO_HOOKS_FILE") {
        return Some(PathBuf::from(path));
    }
    config_home.map(|dir| dir.join("hooks.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hooks_for(json: &str) -> CommandHooks {
        let file = HooksFile::parse(json).expect("fixture parses");
        CommandHooks::new(
            Arc::new(file),
            HookContext {
                session_id: "s".into(),
                cwd: "/tmp".into(),
                model: "m".into(),
                permission_mode: Some("manual".into()),
                ..HookContext::default()
            },
            None,
            std::env::temp_dir(),
            HookHandles::default(),
        )
        .expect("the fixture has a runnable handler")
    }

    fn call(name: &str, arguments: &str) -> ToolCallRequest {
        ToolCallRequest {
            id: "call-1".into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    fn pre_hook(command: &str) -> String {
        format!(
            r#"{{"hooks":{{"PreToolUse":[{{"matcher":"bash",
                 "hooks":[{{"type":"command","command":{command},"timeout":20}}]}}]}}}}"#,
            command = serde_json::to_string(command).unwrap()
        )
    }

    #[test]
    fn a_file_with_nothing_runnable_declines_to_build_a_sink() {
        let file = HooksFile::parse(r#"{"hooks":{"PreToolUse":[]}}"#).unwrap();
        assert!(
            CommandHooks::new(
                Arc::new(file),
                HookContext::default(),
                None,
                std::env::temp_dir(),
                HookHandles::default(),
            )
            .is_none()
        );
    }

    #[test]
    fn the_no_op_sink_says_nothing_about_anything() {
        let cancel = CancelToken::new();
        let c = call("bash", "{}");
        assert_eq!(NoHooks.pre_tool_use(&c, &cancel), PreToolVerdict::default());
        assert!(NoHooks.permission_request(&c, &cancel).is_none());
        assert!(NoHooks.subagent_start("a", "explore", &cancel).is_empty());
    }

    #[test]
    fn a_hook_reads_the_payload_on_stdin_and_can_block_on_what_it_finds() {
        // The end-to-end contract in one command: read stdin, inspect
        // `tool_input.command`, refuse with exit 2 + a stderr reason.
        let hooks = hooks_for(&pre_hook(
            r#"read -r line; case "$line" in *"rm -rf"*) echo "no destructive deletes" >&2; exit 2;; esac"#,
        ));
        let verdict = hooks.pre_tool_use(
            &call("bash", r#"{"command":"rm -rf build/"}"#),
            &CancelToken::new(),
        );
        assert_eq!(
            verdict.blocked.as_deref(),
            Some("no destructive deletes"),
            "the hook's stderr is the reason"
        );

        let allowed = hooks.pre_tool_use(&call("bash", r#"{"command":"ls"}"#), &CancelToken::new());
        assert_eq!(allowed, PreToolVerdict::default(), "a silent hook allows");
    }

    #[test]
    fn a_hook_whose_matcher_does_not_fire_never_runs_at_all() {
        // `false` would block if it ran — the matcher is what saves the call.
        let hooks = hooks_for(&pre_hook("echo blocked >&2; exit 2"));
        let verdict = hooks.pre_tool_use(&call("read", "{}"), &CancelToken::new());
        assert_eq!(verdict, PreToolVerdict::default());
    }

    #[test]
    fn a_json_verdict_can_deny_rewrite_and_add_context_at_once() {
        let hooks = hooks_for(&pre_hook(
            r#"printf '%s' '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","additionalContext":"ran under review","updatedInput":{"command":"ls -la"}}}'"#,
        ));
        let verdict = hooks.pre_tool_use(&call("bash", r#"{"command":"ls"}"#), &CancelToken::new());
        assert!(verdict.pre_approved);
        assert_eq!(verdict.blocked, None);
        assert_eq!(verdict.context.as_deref(), Some("ran under review"));
        let updated = verdict.updated_input.expect("a rewrite");
        assert!(updated.contains("ls -la"), "{updated}");
        assert!(verdict.note.is_some(), "the user sees that a hook spoke");
    }

    #[test]
    fn a_hook_that_exceeds_its_timeout_is_killed_and_fails_open() {
        let json = r#"{"hooks":{"PreToolUse":[{"hooks":[
            {"type":"command","command":"sleep 30","timeout":1}]}]}}"#;
        let hooks = hooks_for(json);
        let started = Instant::now();
        let verdict = hooks.pre_tool_use(&call("bash", "{}"), &CancelToken::new());
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the timeout must actually reap it: {:?}",
            started.elapsed()
        );
        assert_eq!(
            verdict.blocked, None,
            "a broken hook must not wedge the agent"
        );
        let note = verdict.note.expect("the failure is surfaced");
        assert!(note.contains("timed out"), "{note}");
    }

    #[test]
    fn a_payload_bigger_than_the_pipe_buffer_never_wedges_the_cancel() {
        // The regression: the payload write used to run on the calling thread
        // *before* the deadline loop, so a payload past the pipe buffer
        // (64 KiB on Linux) fed to a hook that never reads stdin parked
        // `write_all` until the child exited — Esc dead, timeout never armed.
        // A `PostToolUse` payload embeds the tool's output, whose cap *is*
        // 64 KiB, so this is reachable from an ordinary `read`/`bash` call.
        let json = r#"{"hooks":{"PostToolUse":[{"hooks":[
            {"type":"command","command":"sleep 8","timeout":600}]}]}}"#;
        let hooks = hooks_for(json);
        let outcome = ToolOutcome::ok("x".repeat(HOOK_OUTPUT_MAX_BYTES));
        let cancel = CancelToken::new();
        let flag = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            flag.cancel();
        });
        let started = Instant::now();
        let verdict = hooks.post_tool_use(&call("bash", "{}"), &outcome, &cancel);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "Esc must reap a hook stuck on its stdin write: {:?}",
            started.elapsed()
        );
        assert!(verdict.is_quiet() || verdict.note.is_some());
    }

    #[test]
    fn a_payload_bigger_than_the_pipe_buffer_never_defeats_the_timeout() {
        // The Esc twin above proves the cancel path; this one proves the
        // *deadline* arms even while the payload write is still parked — the
        // original bug never started the clock, so a 2 s timeout ran for the
        // child's whole lifetime (an independent review measured 60 s on a
        // 3 s timeout).
        let json = r#"{"hooks":{"PostToolUse":[{"hooks":[
            {"type":"command","command":"sleep 8","timeout":2}]}]}}"#;
        let hooks = hooks_for(json);
        let outcome = ToolOutcome::ok("x".repeat(HOOK_OUTPUT_MAX_BYTES));
        let started = Instant::now();
        let verdict = hooks.post_tool_use(&call("bash", "{}"), &outcome, &CancelToken::new());
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_secs(2) && elapsed < Duration::from_secs(5),
            "the timeout must fire at ~2s, not the child's 8s lifetime: {elapsed:?}"
        );
        let note = verdict.note.expect("the timeout is surfaced");
        assert!(note.contains("timed out"), "{note}");
    }

    #[test]
    fn a_cancelled_turn_reaps_a_running_hook_promptly() {
        let json = r#"{"hooks":{"PreToolUse":[{"hooks":[
            {"type":"command","command":"sleep 30","timeout":600}]}]}}"#;
        let hooks = hooks_for(json);
        let cancel = CancelToken::new();
        let flag = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            flag.cancel();
        });
        let started = Instant::now();
        let verdict = hooks.pre_tool_use(&call("bash", "{}"), &cancel);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "Esc must not wait out the hook: {:?}",
            started.elapsed()
        );
        assert_eq!(verdict.blocked, None);
    }

    #[test]
    fn a_missing_hook_command_fails_open_with_a_warning() {
        let hooks = hooks_for(&pre_hook("/definitely/not/a/real/binary"));
        let verdict = hooks.pre_tool_use(&call("bash", "{}"), &CancelToken::new());
        assert_eq!(verdict.blocked, None);
        assert!(verdict.note.is_some(), "the user hears about it");
    }

    #[test]
    fn two_matching_hooks_both_run_and_the_first_block_wins() {
        let json = r#"{"hooks":{"PreToolUse":[
            {"hooks":[{"type":"command","command":"echo first >&2; exit 2"}]},
            {"hooks":[{"type":"command","command":"echo second >&2; exit 2"}]}]}}"#;
        let hooks = hooks_for(json);
        let verdict = hooks.pre_tool_use(&call("bash", "{}"), &CancelToken::new());
        assert_eq!(verdict.blocked.as_deref(), Some("first"));
    }

    #[test]
    fn post_tool_use_sees_the_output_and_turns_a_block_into_model_feedback() {
        let json = r#"{"hooks":{"PostToolUse":[{"matcher":"bash","hooks":[
            {"type":"command","command":"grep -q 'FAILED' && printf '%s' '{\"decision\":\"block\",\"reason\":\"tests are red\"}' || true"}]}]}}"#;
        let hooks = hooks_for(json);
        let outcome = ToolOutcome {
            output: "3 passed, 1 FAILED".into(),
            ok: true,
            ..ToolOutcome::ok(String::new())
        };
        let verdict = hooks.post_tool_use(&call("bash", "{}"), &outcome, &CancelToken::new());
        let context = verdict.context.expect("the model is told");
        assert!(context.contains("tests are red"), "{context}");
        assert!(verdict.note.is_some_and(|n| n.contains("tests are red")));
    }

    #[test]
    fn permission_request_can_answer_in_the_users_stead_both_ways() {
        let deny = hooks_for(
            r#"{"hooks":{"PermissionRequest":[{"hooks":[{"type":"command","command":
              "printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PermissionRequest\",\"decision\":{\"behavior\":\"deny\",\"message\":\"never in prod\"}}}'"}]}]}}"#,
        );
        match deny.permission_request(&call("bash", "{}"), &CancelToken::new()) {
            Some(HookPermissionVerdict::Deny { display, result }) => {
                assert!(display.contains("never in prod"), "{display}");
                assert!(result.contains("never in prod"), "{result}");
            }
            other => panic!("expected a deny, got {other:?}"),
        }

        let allow = hooks_for(
            r#"{"hooks":{"PermissionRequest":[{"hooks":[{"type":"command","command":
              "printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PermissionRequest\",\"decision\":{\"behavior\":\"allow\"}}}'"}]}]}}"#,
        );
        assert!(matches!(
            allow.permission_request(&call("bash", "{}"), &CancelToken::new()),
            Some(HookPermissionVerdict::Allow { .. })
        ));
    }

    #[test]
    fn a_broken_permission_request_sibling_rides_the_allow_note() {
        // Two handlers: one that cannot spawn (a warning) and one that
        // allows. The warning must not vanish — it rides the visible note so
        // the user hears their guard is broken.
        let hooks = hooks_for(
            r#"{"hooks":{"PermissionRequest":[{"hooks":[
              {"type":"command","command":"/definitely/not/a/real/binary"},
              {"type":"command","command":
              "printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PermissionRequest\",\"decision\":{\"behavior\":\"allow\"}}}'"}]}]}}"#,
        );
        match hooks.permission_request(&call("bash", "{}"), &CancelToken::new()) {
            Some(HookPermissionVerdict::Allow { note }) => {
                assert!(note.starts_with("Allowed by hook"), "{note}");
                assert!(
                    note.contains("/definitely/not/a/real/binary"),
                    "the broken sibling is named: {note}"
                );
            }
            other => panic!("expected an allow, got {other:?}"),
        }
    }

    #[test]
    fn a_subagent_start_hooks_plain_output_becomes_that_agents_context() {
        // SubagentStart is one of the events whose plain stdout *is* the
        // answer — a hook that just prints is briefing the agent.
        let hooks = hooks_for(
            r#"{"hooks":{"SubagentStart":[{"matcher":"explore","hooks":[{"type":"command",
              "command":"echo 'the repo root is /srv/app'"}]}]}}"#,
        );
        assert_eq!(
            hooks.subagent_start("a1", "explore", &CancelToken::new()),
            vec!["the repo root is /srv/app".to_string()]
        );
        // A different agent type is not this hook's business.
        assert!(
            hooks
                .subagent_start("a2", "reviewer", &CancelToken::new())
                .is_empty(),
            "the matcher gates on agent_type"
        );
    }

    #[test]
    fn a_lead_sinks_stop_fires_the_stop_event_and_a_block_is_the_feedback() {
        let probe = std::env::temp_dir().join("alter-zero-hook-stop-probe.json");
        let _ = std::fs::remove_file(&probe);
        let hooks = hooks_for(&format!(
            r#"{{"hooks":{{"Stop":[{{"hooks":[{{"type":"command",
              "command":"tee {} | grep -q '\"stop_hook_active\":false' && {{ echo 'tests are red' >&2; exit 2; }} || exit 0"}}]}}]}}}}"#,
            probe.display()
        ));
        // First firing: the flag is false, the hook blocks — the reason is
        // the continuation feedback.
        let feedback = hooks.stop(false, "the answer", &CancelToken::new());
        assert_eq!(feedback.as_deref(), Some("tests are red"));
        let written = std::fs::read_to_string(&probe).expect("the stop hook ran");
        let value: serde_json::Value = serde_json::from_str(&written).expect("valid JSON");
        assert_eq!(value["hook_event_name"], serde_json::json!("Stop"));
        assert_eq!(value["stop_hook_active"], serde_json::json!(false));
        assert_eq!(
            value["last_assistant_message"],
            serde_json::json!("the answer")
        );
        // Second firing: the flag is true, the hook lets go.
        assert_eq!(hooks.stop(true, "fixed it", &CancelToken::new()), None);
        let _ = std::fs::remove_file(&probe);
    }

    #[test]
    fn a_tagged_sinks_stop_fires_subagent_stop_and_matches_the_agent_type() {
        let probe = std::env::temp_dir().join("alter-zero-hook-substop-probe.json");
        let _ = std::fs::remove_file(&probe);
        let hooks = hooks_for(&format!(
            r#"{{"hooks":{{"SubagentStop":[{{"matcher":"explore","hooks":[{{"type":"command",
              "command":"cat > {}"}}]}}]}}}}"#,
            probe.display()
        ));
        let sink = hooks
            .for_subagent("a1", "explore")
            .expect("a real sink re-tags itself");
        assert_eq!(sink.stop(false, "found it", &CancelToken::new()), None);
        let written = std::fs::read_to_string(&probe).expect("the stop hook ran");
        let value: serde_json::Value = serde_json::from_str(&written).expect("valid JSON");
        assert_eq!(value["hook_event_name"], serde_json::json!("SubagentStop"));
        assert_eq!(value["agent_id"], serde_json::json!("a1"));
        assert_eq!(value["agent_type"], serde_json::json!("explore"));
        assert_eq!(value["stop_hook_active"], serde_json::json!(false));
        assert_eq!(
            value["last_assistant_message"],
            serde_json::json!("found it")
        );
        // The old `success` extension is gone — the event fires only on a
        // natural completion now.
        assert!(!value.as_object().unwrap().contains_key("success"));
        let _ = std::fs::remove_file(&probe);

        // A different agent type is not this hook's business.
        let other = hooks.for_subagent("a2", "reviewer").expect("re-tags");
        let _ = std::fs::remove_file(&probe);
        assert_eq!(other.stop(false, "done", &CancelToken::new()), None);
        assert!(!probe.exists(), "the matcher gates on agent_type");
    }

    #[test]
    fn a_stop_hooks_continue_false_outranks_its_own_block() {
        // codex's aggregate rule: a hook that says *stop everything* is not
        // asking for another round, even when it also spelled a block.
        let hooks = hooks_for(
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":
              "printf '%s' '{\"continue\":false,\"decision\":\"block\",\"reason\":\"keep going\"}'"}]}]}}"#,
        );
        assert_eq!(hooks.stop(false, "answer", &CancelToken::new()), None);
    }

    #[test]
    fn a_subagents_calls_carry_its_identity_into_the_payload() {
        // The hook writes what it was told to a file; we read it back.
        let probe = std::env::temp_dir().join("alter-zero-hook-agent-probe.json");
        let _ = std::fs::remove_file(&probe);
        let hooks = hooks_for(&pre_hook(&format!("cat > {}", probe.display())));
        let sink = hooks
            .for_subagent("agent-7", "explore")
            .expect("a real sink re-tags itself");
        let _ = sink.pre_tool_use(&call("bash", "{}"), &CancelToken::new());
        let written = std::fs::read_to_string(&probe).expect("the hook received a payload");
        let value: serde_json::Value = serde_json::from_str(&written).expect("valid JSON");
        assert_eq!(value["agent_id"], serde_json::json!("agent-7"));
        assert_eq!(value["agent_type"], serde_json::json!("explore"));
        let _ = std::fs::remove_file(&probe);
    }

    #[test]
    fn session_id_is_the_conversation_the_recorder_publishes() {
        // The bug: payloads carried the id minted at startup, while the
        // rollout file minted its own and `--resume` resolves by the file's
        // — so a resume command built from `session_id` (herdr's, any
        // session tracker's) named a session that did not exist, and the id
        // never followed a `/clear` or a `/resume` (docs/hooks.md). The
        // recorder's id is the conversation's: it wins over the stored one
        // the moment it is published, and moves when it moves.
        let probe = std::env::temp_dir().join("alter-zero-hook-conversation-probe.json");
        let _ = std::fs::remove_file(&probe);
        let conversation: ConversationCell = ConversationCell::default();
        let file = HooksFile::parse(&pre_hook(&format!("cat > {}", probe.display()))).unwrap();
        let hooks = CommandHooks::new(
            Arc::new(file),
            HookContext {
                session_id: "startup-id".into(),
                ..HookContext::default()
            },
            None,
            std::env::temp_dir(),
            HookHandles {
                conversation: Arc::clone(&conversation),
                ..HookHandles::default()
            },
        )
        .expect("runnable");
        let read = || -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(&probe).expect("hook ran")).unwrap()
        };

        let _ = hooks.pre_tool_use(&call("bash", "{}"), &CancelToken::new());
        assert_eq!(
            read()["session_id"],
            serde_json::json!("startup-id"),
            "nothing published yet: the stored id stands"
        );

        *conversation.write().expect("not poisoned") = Conversation {
            id: Some("conv-2".into()),
            transcript_path: None,
        };
        let _ = hooks.pre_tool_use(&call("bash", "{}"), &CancelToken::new());
        let value = read();
        assert_eq!(value["session_id"], serde_json::json!("conv-2"));
        assert_eq!(
            value["transcript_path"],
            serde_json::Value::Null,
            "an id before its file exists — a /clear's fresh conversation"
        );
        let _ = std::fs::remove_file(&probe);
    }

    #[test]
    fn payloads_report_the_live_permission_mode_and_transcript_path() {
        // The regression: the sink used to freeze `permission_mode: None`
        // ("disabled") at build time, so a Shift+Tab cycle never reached a
        // payload — and `transcript_path` stayed null forever because the
        // rollout file is created after the sink. Both now resolve at
        // dispatch time from live handles.
        let probe = std::env::temp_dir().join("alter-zero-hook-live-ctx-probe.json");
        let _ = std::fs::remove_file(&probe);
        let gate = crate::permission::PermissionGate::new();
        gate.set_mode(crate::permission::PermissionMode::Edit);
        let conversation: ConversationCell = ConversationCell::default();
        let file = HooksFile::parse(&pre_hook(&format!("cat > {}", probe.display()))).unwrap();
        let hooks = CommandHooks::new(
            Arc::new(file),
            HookContext {
                session_id: "s".into(),
                cwd: "/tmp".into(),
                model: "m".into(),
                permission_mode: None,
                ..HookContext::default()
            },
            None,
            std::env::temp_dir(),
            HookHandles {
                gate: Some(gate.clone()),
                conversation: Arc::clone(&conversation),
                ..HookHandles::default()
            },
        )
        .expect("runnable");

        let _ = hooks.pre_tool_use(&call("bash", "{}"), &CancelToken::new());
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&probe).expect("hook ran")).unwrap();
        assert_eq!(value["permission_mode"], serde_json::json!("edit"));
        assert_eq!(value["transcript_path"], serde_json::Value::Null);

        gate.set_mode(crate::permission::PermissionMode::Master);
        conversation.write().expect("not poisoned").transcript_path =
            Some("/r/sess.jsonl".to_string());
        let _ = hooks.pre_tool_use(&call("bash", "{}"), &CancelToken::new());
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&probe).expect("hook ran")).unwrap();
        assert_eq!(
            value["permission_mode"],
            serde_json::json!("master"),
            "a Shift+Tab cycle reaches the next payload"
        );
        assert_eq!(
            value["transcript_path"],
            serde_json::json!("/r/sess.jsonl"),
            "the recorder's published rollout path reaches the payload"
        );
        let _ = std::fs::remove_file(&probe);
    }

    #[test]
    fn session_start_drains_the_queued_sources_once_and_matches_on_them() {
        // codex's pending-source model: bootstrap/`/clear`/`/resume` queue a
        // source, the next turn's top drains them all, and the matcher gates
        // on the source string. A second drain finds nothing.
        let file = HooksFile::parse(
            r#"{"hooks":{"SessionStart":[{"matcher":"startup","hooks":[{"type":"command",
              "command":"echo 'the build id is ZX-4417'"}]}]}}"#,
        )
        .unwrap();
        let handles = HookHandles::default();
        handles
            .sources
            .lock()
            .unwrap()
            .extend(["startup".to_string(), "clear".to_string()]);
        let hooks = CommandHooks::new(
            Arc::new(file),
            HookContext::default(),
            None,
            std::env::temp_dir(),
            handles,
        )
        .expect("runnable");
        assert_eq!(
            hooks.session_start(&CancelToken::new()),
            vec!["the build id is ZX-4417".to_string()],
            "plain stdout is the context; the clear source matched nothing"
        );
        assert_eq!(
            hooks.session_start(&CancelToken::new()),
            Vec::<String>::new(),
            "the queue drained"
        );
    }

    #[test]
    fn user_prompt_submit_blocks_adds_context_and_skips_synthetic_turns() {
        let file = HooksFile::parse(
            r#"{"hooks":{"UserPromptSubmit":[{"matcher":"ignored-for-this-event","hooks":[{"type":"command",
              "command":"grep -q secret && { echo 'no secrets in prompts' >&2; exit 2; }; echo 'shipped today: v2'"}]}]}}"#,
        )
        .unwrap();
        let handles = HookHandles::default();
        let hooks = CommandHooks::new(
            Arc::new(file),
            HookContext::default(),
            None,
            std::env::temp_dir(),
            handles.clone(),
        )
        .expect("runnable");

        // Matcher-less event: the group fires despite its matcher text.
        let blocked = hooks.user_prompt_submit("here is my secret", &CancelToken::new());
        assert_eq!(blocked.blocked.as_deref(), Some("no secrets in prompts"));

        let clean = hooks.user_prompt_submit("hello", &CancelToken::new());
        assert_eq!(clean.blocked, None);
        assert_eq!(
            clean.contexts,
            vec!["shipped today: v2".to_string()],
            "plain stdout is context for this event"
        );

        // A loop-initiated turn is marked synthetic: the hook never fires and
        // the mark clears with the skip.
        handles
            .synthetic_turn
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            hooks.user_prompt_submit("here is my secret", &CancelToken::new()),
            PromptVerdict::default()
        );
        assert_eq!(
            hooks
                .user_prompt_submit("here is my secret", &CancelToken::new())
                .blocked
                .as_deref(),
            Some("no secrets in prompts"),
            "the mark cleared: the next real prompt is gated again"
        );
    }

    #[test]
    fn turn_start_folds_the_opening_hooks_into_notes_and_a_verdict() {
        // The spawn-top glue in one call: SessionStart's context first, then
        // UserPromptSubmit's, each already formatted as the wire text the
        // model reads — and the notes come back even when the prompt is
        // blocked (both references inject on block).
        #[derive(Debug)]
        struct Fake;
        impl HookSink for Fake {
            fn session_start(&self, _cancel: &CancelToken) -> Vec<String> {
                vec!["the repo root is /srv/app".to_string()]
            }
            fn user_prompt_submit(&self, prompt: &str, _cancel: &CancelToken) -> PromptVerdict {
                PromptVerdict {
                    blocked: prompt.contains("secret").then(|| "no secrets".to_string()),
                    contexts: vec!["shipped today: v2".to_string()],
                }
            }
        }
        let opening = turn_start(&Fake, "hello", &CancelToken::new());
        assert_eq!(opening.blocked, None);
        assert_eq!(opening.notes.len(), 2);
        assert_eq!(opening.notes[0].0, "SessionStart hook");
        assert!(
            opening.notes[0].1.starts_with("<system-reminder>\n")
                && opening.notes[0]
                    .1
                    .contains("SessionStart hook additional context: the repo root is /srv/app"),
            "{:?}",
            opening.notes[0].1
        );
        assert_eq!(opening.notes[1].0, "UserPromptSubmit hook");

        let blocked = turn_start(&Fake, "my secret", &CancelToken::new());
        assert_eq!(blocked.blocked.as_deref(), Some("no secrets"));
        assert_eq!(blocked.notes.len(), 2, "context injects even on a block");
    }

    #[test]
    fn session_end_runs_under_a_hard_budget_and_matches_on_the_reason() {
        // Quitting must never hang on a hook: the whole event is capped at
        // ~2 s whatever the handler's own timeout says. The matcher gates on
        // the reason, Claude Code's rule.
        let probe = std::env::temp_dir().join("alter-zero-hook-end-probe.json");
        let _ = std::fs::remove_file(&probe);
        let hooks = hooks_for(&format!(
            r#"{{"hooks":{{"SessionEnd":[
              {{"matcher":"clear","hooks":[{{"type":"command","command":"cat > {}"}}]}},
              {{"matcher":"prompt_input_exit","hooks":[{{"type":"command","command":"sleep 30","timeout":600}}]}}]}}}}"#,
            probe.display()
        ));
        let started = Instant::now();
        hooks.session_end("prompt_input_exit");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the budget reaps a slow shutdown hook: {:?}",
            started.elapsed()
        );
        assert!(!probe.exists(), "the clear-matched handler never fired");

        hooks.session_end("clear");
        let written = std::fs::read_to_string(&probe).expect("the clear hook ran");
        let value: serde_json::Value = serde_json::from_str(&written).expect("valid JSON");
        assert_eq!(value["hook_event_name"], serde_json::json!("SessionEnd"));
        assert_eq!(value["reason"], serde_json::json!("clear"));
        let _ = std::fs::remove_file(&probe);
    }

    /// A sink whose every listed event appends its payload, one line each,
    /// to `probe` — the observation events' tests read what was sent.
    fn recording_hooks(probe: &Path, events: &[(&str, Option<&str>)]) -> CommandHooks {
        let groups: Vec<String> = events
            .iter()
            .map(|(event, matcher)| {
                let matcher = matcher.map_or_else(String::new, |m| format!(r#""matcher":"{m}","#));
                format!(
                    r#""{event}":[{{{matcher}"hooks":[{{"type":"command","command":"cat >> {p}; echo >> {p}"}}]}}]"#,
                    p = probe.display()
                )
            })
            .collect();
        hooks_for(&format!(r#"{{"hooks":{{{}}}}}"#, groups.join(",")))
    }

    fn recorded(probe: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(probe)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("valid JSON"))
            .collect()
    }

    #[test]
    fn a_failed_call_runs_post_tool_use_failure_and_its_context_reaches_the_model() {
        let json = r#"{"hooks":{"PostToolUseFailure":[{"matcher":"askuserquestion","hooks":[
            {"type":"command","command":"printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PostToolUseFailure\",\"additionalContext\":\"ask again later\"}}'"}]}]}}"#;
        let hooks = hooks_for(json);
        let declined = ToolOutcome::error("User declined to answer questions");
        let verdict = hooks.post_tool_use_failure(
            &call("askuserquestion", r#"{"questions":[]}"#),
            &declined,
            &CancelToken::new(),
        );
        assert_eq!(verdict.context.as_deref(), Some("ask again later"));
        assert!(verdict.note.is_some(), "the user sees that a hook spoke");
        // The matcher is the tool name, like PostToolUse's.
        let other = hooks.post_tool_use_failure(
            &call("bash", "{}"),
            &ToolOutcome::error("boom"),
            &CancelToken::new(),
        );
        assert!(other.is_quiet());
    }

    #[test]
    fn a_prompt_and_its_answer_reach_their_hooks_naming_the_call() {
        let probe = std::env::temp_dir().join("alter-zero-hook-permission-events-probe.jsonl");
        let _ = std::fs::remove_file(&probe);
        let hooks = recording_hooks(
            &probe,
            &[
                ("Notification", Some("permission_prompt")),
                ("PermissionResult", Some("bash")),
            ],
        );
        let c = call("bash", r#"{"command":"make deploy"}"#);
        hooks.permission_prompt(&c, &CancelToken::new());
        hooks.permission_result(&c, false, PermissionSource::User, &CancelToken::new());
        let sent = recorded(&probe);
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert_eq!(
            sent[0]["hook_event_name"],
            serde_json::json!("Notification")
        );
        assert_eq!(
            sent[0]["notification_type"],
            serde_json::json!("permission_prompt")
        );
        assert_eq!(
            sent[0]["message"],
            serde_json::json!(format!(
                "{} needs your permission to use Bash",
                crate::APP_NAME
            ))
        );
        assert_eq!(sent[0]["title"], serde_json::json!("Permission needed"));
        assert_eq!(sent[0]["tool_use_id"], serde_json::json!("call-1"));
        assert_eq!(
            sent[1]["hook_event_name"],
            serde_json::json!("PermissionResult")
        );
        assert_eq!(
            sent[1]["decision"],
            serde_json::json!({"behavior": "deny", "source": "user"})
        );
        assert_eq!(
            sent[1]["tool_input"]["command"],
            serde_json::json!("make deploy")
        );
        let _ = std::fs::remove_file(&probe);
    }

    #[test]
    fn stop_failure_is_the_leads_and_matches_on_the_error_type() {
        let probe = std::env::temp_dir().join("alter-zero-hook-stop-failure-probe.jsonl");
        let _ = std::fs::remove_file(&probe);
        let hooks = recording_hooks(&probe, &[("StopFailure", Some("rate_limit"))]);
        hooks.stop_failure("server_error", "HTTP 500", &CancelToken::new());
        assert!(recorded(&probe).is_empty(), "the matcher is the error type");
        // A subagent's failed round is its own business: like Stop, the
        // event describes the session's turn (Claude Code's rule).
        hooks.tagged("agent-1", "explore").stop_failure(
            "rate_limit",
            "HTTP 429",
            &CancelToken::new(),
        );
        assert!(
            recorded(&probe).is_empty(),
            "a subagent fires no StopFailure"
        );
        hooks.stop_failure("rate_limit", "HTTP 429: slow down", &CancelToken::new());
        let sent = recorded(&probe);
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0]["error"], serde_json::json!("rate_limit"));
        assert_eq!(
            sent[0]["error_details"],
            serde_json::json!("HTTP 429: slow down")
        );
        let _ = std::fs::remove_file(&probe);
    }

    #[test]
    fn interrupt_runs_under_a_hard_budget() {
        // An interrupt must never wait on a hook: like SessionEnd, the whole
        // event is capped (codex clamps its Interrupt hooks to 3 s).
        let probe = std::env::temp_dir().join("alter-zero-hook-interrupt-probe.jsonl");
        let _ = std::fs::remove_file(&probe);
        let hooks = hooks_for(&format!(
            r#"{{"hooks":{{"Interrupt":[{{"hooks":[
              {{"type":"command","command":"cat >> {p}; echo >> {p}"}},
              {{"type":"command","command":"sleep 30","timeout":600}}]}}]}}}}"#,
            p = probe.display()
        ));
        let started = Instant::now();
        hooks.interrupt();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the budget reaps a slow interrupt hook: {:?}",
            started.elapsed()
        );
        let sent = recorded(&probe);
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0]["hook_event_name"], serde_json::json!("Interrupt"));
        let _ = std::fs::remove_file(&probe);
    }

    #[test]
    fn a_hook_child_is_told_where_the_running_binary_is() {
        // `ALTER_ZERO_BIN_PATH`, herdr's own `HERDR_BIN_PATH` idea: an
        // integration's command names `"$ALTER_ZERO_BIN_PATH" …` and never
        // goes stale when the binary moves or updates (docs/herdr.md).
        let probe = std::env::temp_dir().join("alter-zero-hook-bin-path-probe.txt");
        let _ = std::fs::remove_file(&probe);
        let hooks = hooks_for(&pre_hook(&format!(
            r#"printf '%s' "$ALTER_ZERO_BIN_PATH" > {}"#,
            probe.display()
        )));
        let _ = hooks.pre_tool_use(&call("bash", "{}"), &CancelToken::new());
        let told = std::fs::read_to_string(&probe).expect("the hook ran");
        assert_eq!(
            std::path::PathBuf::from(&told),
            running_binary(std::env::current_exe().unwrap()),
            "{told:?}"
        );
        let _ = std::fs::remove_file(&probe);
    }

    #[test]
    fn a_binary_replaced_under_the_running_process_is_still_named_by_its_path() {
        // Linux reports a running executable whose file was replaced (an
        // `alter-zero update`) as `{path} (deleted)`; the path itself is
        // where the new binary is.
        assert_eq!(
            running_binary(std::path::PathBuf::from("/usr/bin/alter-zero (deleted)")),
            std::path::PathBuf::from("/usr/bin/alter-zero")
        );
        assert_eq!(
            running_binary(std::path::PathBuf::from("/usr/bin/alter-zero")),
            std::path::PathBuf::from("/usr/bin/alter-zero")
        );
    }

    // ===== the boundary lane (docs/hooks.md) =====

    #[test]
    fn the_lane_runs_its_jobs_in_submission_order_off_the_caller() {
        let lane = HookLane::default();
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let started = Instant::now();
        for (n, sleep) in [(1, 150), (2, 0), (3, 50)] {
            let order = Arc::clone(&order);
            lane.submit(move |_cancel| {
                std::thread::sleep(Duration::from_millis(sleep));
                order.lock().unwrap().push(n);
            });
        }
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "submit never blocks the caller: {:?}",
            started.elapsed()
        );
        assert!(lane.wait_idle(&|| false));
        assert_eq!(
            *order.lock().unwrap(),
            vec![1, 2, 3],
            "FIFO, whatever each takes"
        );
    }

    #[test]
    fn waiting_on_the_lane_gives_up_when_the_waiter_is_cancelled() {
        let lane = HookLane::default();
        lane.submit(|_cancel| std::thread::sleep(Duration::from_millis(400)));
        let started = Instant::now();
        assert!(!lane.wait_idle(&|| started.elapsed() > Duration::from_millis(50)));
        assert!(started.elapsed() < Duration::from_millis(300));
        assert!(lane.wait_idle_for(Duration::from_secs(5)), "it does drain");
    }

    #[test]
    fn cancelling_the_lane_reaps_the_running_job_and_drops_the_queued_ones() {
        let lane = HookLane::default();
        let ran = Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let ran = Arc::clone(&ran);
            lane.submit(move |cancel| {
                let started = Instant::now();
                while !cancel.is_cancelled() && started.elapsed() < Duration::from_secs(10) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                ran.lock().unwrap().push("first, reaped");
            });
        }
        {
            let ran = Arc::clone(&ran);
            lane.submit(move |_| ran.lock().unwrap().push("queued, dropped"));
        }
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        lane.cancel_pending();
        {
            let ran = Arc::clone(&ran);
            lane.submit(move |_| ran.lock().unwrap().push("after the cancel"));
        }
        assert!(lane.wait_idle_for(Duration::from_secs(5)));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            *ran.lock().unwrap(),
            vec!["first, reaped", "after the cancel"]
        );
    }

    #[test]
    fn a_dispatch_never_overtakes_a_boundary_event_on_the_lane() {
        // The race the lane exists for: an Esc fires `Interrupt` off the UI
        // thread and the user's next prompt starts a turn at once — its
        // `UserPromptSubmit` must not land before the `Interrupt`, or a
        // tracker ends on *idle* while the agent works (docs/herdr.md).
        let probe = std::env::temp_dir().join("alter-zero-hook-lane-order-probe.jsonl");
        let _ = std::fs::remove_file(&probe);
        let hooks = hooks_for(&format!(
            r#"{{"hooks":{{
              "Interrupt":[{{"hooks":[{{"type":"command","command":"sleep 0.3; cat >> {p}; echo >> {p}"}}]}}],
              "UserPromptSubmit":[{{"hooks":[{{"type":"command","command":"cat >> {p}; echo >> {p}"}}]}}]}}}}"#,
            p = probe.display()
        ));
        let lane = hooks.handles.lane.clone();
        {
            let hooks = hooks.clone();
            lane.submit(move |_| hooks.interrupt());
        }
        let _ = hooks.user_prompt_submit("next", &CancelToken::new());
        let names: Vec<serde_json::Value> = recorded(&probe)
            .iter()
            .map(|v| v["hook_event_name"].clone())
            .collect();
        assert_eq!(
            names,
            vec![
                serde_json::json!("Interrupt"),
                serde_json::json!("UserPromptSubmit")
            ]
        );
        let _ = std::fs::remove_file(&probe);
    }

    #[test]
    fn an_interrupt_never_reaches_a_hook_ahead_of_the_prompt_it_closed() {
        // The prompt's `Notification` is still running — a reporter waiting
        // on a lock, a slow socket — when the user presses Esc on that very
        // prompt. Its `Interrupt` must not land first: a tracker would read
        // *idle*, then *blocked*, and show a prompt nobody can answer until
        // the next turn (docs/herdr.md).
        let probe = std::env::temp_dir().join("alter-zero-hook-prompt-order-probe.jsonl");
        let started = std::env::temp_dir().join("alter-zero-hook-prompt-order-started");
        let _ = std::fs::remove_file(&probe);
        let _ = std::fs::remove_file(&started);
        let hooks = hooks_for(&format!(
            r#"{{"hooks":{{
              "Notification":[{{"hooks":[{{"type":"command","command":"touch {s}; sleep 0.3; cat >> {p}; echo >> {p}"}}]}}],
              "Interrupt":[{{"hooks":[{{"type":"command","command":"cat >> {p}; echo >> {p}"}}]}}]}}}}"#,
            p = probe.display(),
            s = started.display()
        ));
        // The prompt is raised on the backend's thread…
        let backend = {
            let hooks = hooks.clone();
            std::thread::spawn(move || {
                hooks.permission_prompt(&call("bash", "{}"), &CancelToken::new());
            })
        };
        let waited = Instant::now();
        while !started.exists() && waited.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(started.exists(), "the prompt's hook never started");
        // …and answered with an Esc on the UI's.
        hooks.fire_interrupt();
        backend.join().unwrap();
        assert!(hooks.handles.lane.wait_idle_for(Duration::from_secs(5)));
        let names: Vec<serde_json::Value> = recorded(&probe)
            .iter()
            .map(|v| v["hook_event_name"].clone())
            .collect();
        assert_eq!(
            names,
            vec![
                serde_json::json!("Notification"),
                serde_json::json!("Interrupt")
            ]
        );
        let _ = std::fs::remove_file(&probe);
        let _ = std::fs::remove_file(&started);
    }

    #[test]
    fn the_turns_esc_still_reaps_a_prompt_notification_that_hangs() {
        // Ordered behind the prompt is not stuck behind it: the turn's own
        // token reaps the hook, so the `Interrupt` runs at once and a hung
        // notifier never delays the next turn's hooks.
        let probe = std::env::temp_dir().join("alter-zero-hook-prompt-reap-probe.jsonl");
        let started = std::env::temp_dir().join("alter-zero-hook-prompt-reap-started");
        let _ = std::fs::remove_file(&probe);
        let _ = std::fs::remove_file(&started);
        let hooks = hooks_for(&format!(
            r#"{{"hooks":{{
              "Notification":[{{"hooks":[{{"type":"command","command":"touch {s}; sleep 30; cat >> {p}; echo >> {p}","timeout":600}}]}}],
              "Interrupt":[{{"hooks":[{{"type":"command","command":"cat >> {p}; echo >> {p}"}}]}}]}}}}"#,
            p = probe.display(),
            s = started.display()
        ));
        let turn = CancelToken::new();
        let backend = {
            let hooks = hooks.clone();
            let turn = turn.clone();
            std::thread::spawn(move || hooks.permission_prompt(&call("bash", "{}"), &turn))
        };
        let waited = Instant::now();
        while !started.exists() && waited.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(started.exists(), "the prompt's hook never started");
        let esc = Instant::now();
        turn.cancel();
        hooks.fire_interrupt();
        assert!(hooks.handles.lane.wait_idle_for(Duration::from_secs(5)));
        assert!(
            esc.elapsed() < Duration::from_secs(3),
            "the Esc waited on the hung hook: {:?}",
            esc.elapsed()
        );
        backend.join().unwrap();
        let names: Vec<serde_json::Value> = recorded(&probe)
            .iter()
            .map(|v| v["hook_event_name"].clone())
            .collect();
        assert_eq!(names, vec![serde_json::json!("Interrupt")]);
        let _ = std::fs::remove_file(&probe);
        let _ = std::fs::remove_file(&started);
    }

    #[test]
    fn an_interrupt_waits_for_the_interrupted_turns_hooks_to_be_reaped() {
        // A hook of the turn is mid-flight when Esc lands — a `PreToolUse`
        // reporter, say. The cancel reaps it within a poll, but until then
        // it can still write; the `Interrupt` must not reach anyone before
        // it is gone, or a tracker ends on the stale *working* it sent.
        let probe = std::env::temp_dir().join("alter-zero-hook-reaped-order-probe.txt");
        let started = std::env::temp_dir().join("alter-zero-hook-reaped-order-started");
        let _ = std::fs::remove_file(&probe);
        let _ = std::fs::remove_file(&started);
        let hooks = hooks_for(&format!(
            r#"{{"hooks":{{
              "PreToolUse":[{{"hooks":[{{"type":"command","command":"cat > /dev/null; touch {s}; while :; do echo tick >> {p}; done","timeout":600}}]}}],
              "Interrupt":[{{"hooks":[{{"type":"command","command":"cat > /dev/null; echo interrupt >> {p}"}}]}}]}}}}"#,
            p = probe.display(),
            s = started.display()
        ));
        let turn = CancelToken::new();
        let backend = {
            let hooks = hooks.clone();
            let turn = turn.clone();
            std::thread::spawn(move || {
                let _ = hooks.pre_tool_use(&call("bash", "{}"), &turn);
            })
        };
        let waited = Instant::now();
        while !started.exists() && waited.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(started.exists(), "the turn's hook never started");
        turn.cancel();
        hooks.fire_interrupt();
        assert!(hooks.handles.lane.wait_idle_for(Duration::from_secs(5)));
        backend.join().unwrap();
        let lines: Vec<String> = std::fs::read_to_string(&probe)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        let at = lines
            .iter()
            .position(|line| line == "interrupt")
            .expect("the interrupt hook ran");
        assert!(
            lines[at + 1..].iter().all(|line| line != "tick"),
            "{} ticks landed after the interrupt",
            lines[at + 1..].len()
        );
        let _ = std::fs::remove_file(&probe);
        let _ = std::fs::remove_file(&started);
    }

    #[test]
    fn a_quit_ends_the_session_after_the_cancelled_turns_hooks() {
        // Quitting mid-turn cancels the turn, then fires `SessionEnd`: a
        // hook of that turn still being reaped must not land after it — a
        // tracker would hear *working* from a session that released its
        // pane (docs/herdr.md).
        let probe = std::env::temp_dir().join("alter-zero-hook-quit-order-probe.txt");
        let started = std::env::temp_dir().join("alter-zero-hook-quit-order-started");
        let _ = std::fs::remove_file(&probe);
        let _ = std::fs::remove_file(&started);
        let hooks = hooks_for(&format!(
            r#"{{"hooks":{{
              "PostToolUse":[{{"hooks":[{{"type":"command","command":"cat > /dev/null; touch {s}; while :; do echo tick >> {p}; done","timeout":600}}]}}],
              "SessionEnd":[{{"hooks":[{{"type":"command","command":"cat > /dev/null; echo end >> {p}"}}]}}]}}}}"#,
            p = probe.display(),
            s = started.display()
        ));
        let turn = CancelToken::new();
        let backend = {
            let hooks = hooks.clone();
            let turn = turn.clone();
            std::thread::spawn(move || {
                let _ = hooks.post_tool_use(&call("bash", "{}"), &ToolOutcome::ok("hi"), &turn);
            })
        };
        let waited = Instant::now();
        while !started.exists() && waited.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(started.exists(), "the turn's hook never started");
        turn.cancel();
        assert!(hooks.end_session_at_exit("prompt_input_exit"));
        backend.join().unwrap();
        let lines: Vec<String> = std::fs::read_to_string(&probe)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        let at = lines
            .iter()
            .position(|line| line == "end")
            .expect("the SessionEnd hook ran");
        assert!(
            lines[at + 1..].iter().all(|line| line != "tick"),
            "{} ticks landed after the session ended",
            lines[at + 1..].len()
        );
        let _ = std::fs::remove_file(&probe);
        let _ = std::fs::remove_file(&started);
    }

    #[test]
    fn a_job_runs_under_the_token_it_was_given_and_a_quit_cancels_it() {
        // The prompt's notification runs on the lane under the TURN's token
        // — and a quit, which reaps the lane's running job before its
        // `SessionEnd`, must reach that token too.
        let lane = HookLane::default();
        let turn = CancelToken::new();
        let reaped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let turn = turn.clone();
            let reaped = Arc::clone(&reaped);
            lane.submit_under(turn.clone(), move |cancel| {
                assert_eq!(cancel, &turn, "the job runs under the token it was given");
                let started = Instant::now();
                while !cancel.is_cancelled() && started.elapsed() < Duration::from_secs(10) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                reaped.store(cancel.is_cancelled(), std::sync::atomic::Ordering::SeqCst);
            });
        }
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        lane.cancel_pending();
        assert!(lane.wait_idle_for(Duration::from_secs(5)));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert!(reaped.load(std::sync::atomic::Ordering::SeqCst));
        assert!(turn.is_cancelled());
    }

    #[test]
    fn session_start_fired_at_the_boundary_hands_its_context_to_the_next_turn() {
        // Claude Code's timing: SessionStart runs in the background at the
        // boundary it describes, and the first request waits for it — so a
        // tracker hears of the session at launch, and the hook's context
        // still reaches the turn (docs/hooks.md).
        let probe = std::env::temp_dir().join("alter-zero-hook-eager-start-probe.jsonl");
        let _ = std::fs::remove_file(&probe);
        let hooks = hooks_for(&format!(
            r#"{{"hooks":{{"SessionStart":[{{"hooks":[
              {{"type":"command","command":"cat >> {p}; echo >> {p}; echo 'the build id is ZX-4417'"}}]}}]}}}}"#,
            p = probe.display()
        ));
        hooks
            .handles
            .sources
            .lock()
            .unwrap()
            .push("startup".to_string());
        hooks.fire_session_start();
        assert!(hooks.handles.lane.wait_idle_for(Duration::from_secs(5)));
        assert_eq!(recorded(&probe).len(), 1, "fired at the boundary");
        assert_eq!(recorded(&probe)[0]["source"], serde_json::json!("startup"));

        let contexts = hooks.session_start(&CancelToken::new());
        assert_eq!(contexts, vec!["the build id is ZX-4417".to_string()]);
        assert_eq!(recorded(&probe).len(), 1, "the turn never fires it again");
        assert!(
            hooks.session_start(&CancelToken::new()).is_empty(),
            "the context is the next turn's alone"
        );
        let _ = std::fs::remove_file(&probe);
    }

    #[test]
    fn a_boundary_event_names_the_conversation_it_was_fired_for() {
        // `/resume` fires SessionEnd for the conversation it leaves and
        // adopts the next in the same breath; the event, queued behind
        // whatever the lane is running, must still name the one that ended.
        let probe = std::env::temp_dir().join("alter-zero-hook-pinned-probe.jsonl");
        let _ = std::fs::remove_file(&probe);
        let hooks = recording_hooks(&probe, &[("SessionEnd", None)]);
        *hooks.handles.conversation.write().unwrap() = Conversation {
            id: Some("leaving".into()),
            transcript_path: Some("/r/leaving.jsonl".into()),
        };
        hooks
            .handles
            .lane
            .submit(|_| std::thread::sleep(Duration::from_millis(200)));
        hooks.fire_session_end("resume");
        *hooks.handles.conversation.write().unwrap() = Conversation {
            id: Some("arriving".into()),
            transcript_path: Some("/r/arriving.jsonl".into()),
        };
        assert!(hooks.handles.lane.wait_idle_for(Duration::from_secs(5)));
        let sent = recorded(&probe);
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0]["session_id"], serde_json::json!("leaving"));
        assert_eq!(
            sent[0]["transcript_path"],
            serde_json::json!("/r/leaving.jsonl")
        );
        assert_eq!(sent[0]["reason"], serde_json::json!("resume"));
        let _ = std::fs::remove_file(&probe);
    }

    #[test]
    fn quitting_runs_session_end_even_behind_a_slow_session_start() {
        let probe = std::env::temp_dir().join("alter-zero-hook-exit-probe.jsonl");
        let _ = std::fs::remove_file(&probe);
        let hooks = hooks_for(&format!(
            r#"{{"hooks":{{
              "SessionStart":[{{"hooks":[{{"type":"command","command":"sleep 30","timeout":600}}]}}],
              "SessionEnd":[{{"hooks":[{{"type":"command","command":"cat >> {p}; echo >> {p}"}}]}}]}}}}"#,
            p = probe.display()
        ));
        hooks
            .handles
            .sources
            .lock()
            .unwrap()
            .push("startup".to_string());
        hooks.fire_session_start();
        std::thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        assert!(hooks.end_session_at_exit("prompt_input_exit"));
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "the slow SessionStart was reaped: {:?}",
            started.elapsed()
        );
        let sent = recorded(&probe);
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0]["reason"], serde_json::json!("prompt_input_exit"));
        let _ = std::fs::remove_file(&probe);
    }

    #[test]
    fn contexts_held_for_a_conversation_that_ended_are_forgotten() {
        let hooks = hooks_for(
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo old context"}]}]}}"#,
        );
        hooks
            .handles
            .sources
            .lock()
            .unwrap()
            .push("startup".to_string());
        hooks.fire_session_start();
        hooks.handles.forget_held_contexts();
        assert!(hooks.handles.lane.wait_idle_for(Duration::from_secs(5)));
        assert!(
            hooks.session_start(&CancelToken::new()).is_empty(),
            "a /clear's fresh conversation never reads the old one's context"
        );
    }

    #[test]
    fn a_backend_error_is_classified_into_claude_codes_stop_failure_types() {
        use crate::llm::LlmError;
        let api = |status| LlmError::Api {
            status,
            body: String::new(),
        };
        assert_eq!(stop_failure_kind(&api(429)), "rate_limit");
        assert_eq!(stop_failure_kind(&api(529)), "overloaded");
        assert_eq!(stop_failure_kind(&api(401)), "authentication_failed");
        assert_eq!(stop_failure_kind(&api(403)), "authentication_failed");
        assert_eq!(stop_failure_kind(&api(402)), "billing_error");
        assert_eq!(stop_failure_kind(&api(404)), "model_not_found");
        assert_eq!(stop_failure_kind(&api(400)), "invalid_request");
        assert_eq!(stop_failure_kind(&api(413)), "invalid_request");
        assert_eq!(stop_failure_kind(&api(500)), "server_error");
        assert_eq!(stop_failure_kind(&api(503)), "server_error");
        assert_eq!(
            stop_failure_kind(&LlmError::Http("reset".to_string())),
            "server_error"
        );
        assert_eq!(
            stop_failure_kind(&LlmError::Decode("bad".to_string())),
            "unknown"
        );
    }

    #[test]
    fn the_compact_wrapper_maps_the_turn_onto_the_compact_events() {
        // PreCompact rides the wrapper's prompt hook (context = extra
        // summarization instructions; it can never block — neither reference
        // honours a block for it) and PostCompact rides its stop (which
        // never continues a summarization).
        let probe = std::env::temp_dir().join("alter-zero-hook-compact-probe.jsonl");
        let _ = std::fs::remove_file(&probe);
        let file = HooksFile::parse(&format!(
            r#"{{"hooks":{{
              "PreCompact":[{{"matcher":"auto","hooks":[{{"type":"command",
                "command":"tee -a {p} >/dev/null; echo >> {p}; echo 'keep the API notes'"}}]}}],
              "PostCompact":[{{"hooks":[{{"type":"command","command":"tee -a {p} >/dev/null; echo >> {p}"}}]}}]}}}}"#,
            p = probe.display()
        ))
        .unwrap();
        let inner = CommandHooks::new(
            Arc::new(file),
            HookContext::default(),
            None,
            std::env::temp_dir(),
            HookHandles::default(),
        )
        .expect("runnable");
        let wrapper = CompactHooks::new(inner, true);

        let verdict =
            wrapper.user_prompt_submit("summarize this conversation", &CancelToken::new());
        assert_eq!(verdict.blocked, None, "PreCompact can never block");
        assert_eq!(verdict.contexts, vec!["keep the API notes".to_string()]);
        assert_eq!(wrapper.prompt_hook_label(), "PreCompact");

        assert_eq!(
            wrapper.stop(false, "the handoff summary", &CancelToken::new()),
            None,
            "a summarization is never continued by a hook"
        );
        let lines: Vec<serde_json::Value> = std::fs::read_to_string(&probe)
            .expect("both hooks ran")
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid JSON"))
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["hook_event_name"], serde_json::json!("PreCompact"));
        assert_eq!(lines[0]["trigger"], serde_json::json!("auto"));
        assert_eq!(
            lines[1]["hook_event_name"],
            serde_json::json!("PostCompact")
        );
        assert_eq!(
            lines[1]["compact_summary"],
            serde_json::json!("the handoff summary")
        );
        let _ = std::fs::remove_file(&probe);
    }

    #[test]
    fn the_project_dir_is_exported_under_both_names() {
        let probe = std::env::temp_dir().join("alter-zero-hook-env-probe.txt");
        let _ = std::fs::remove_file(&probe);
        let hooks = hooks_for(&pre_hook(&format!(
            "printf '%s|%s' \"$ALTER_ZERO_PROJECT_DIR\" \"$CLAUDE_PROJECT_DIR\" > {}",
            probe.display()
        )));
        let _ = hooks.pre_tool_use(&call("bash", "{}"), &CancelToken::new());
        let written = std::fs::read_to_string(&probe).expect("the hook ran");
        let (ours, theirs) = written.split_once('|').expect("both vars");
        assert!(!ours.is_empty(), "ALTER_ZERO_PROJECT_DIR is set");
        assert_eq!(ours, theirs, "the Claude Code alias matches");
        let _ = std::fs::remove_file(&probe);
    }
}
