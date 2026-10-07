//! The herdr integration's pure half (`docs/herdr.md`): what a hook event
//! means for the herdr pane Alter Zero runs in.
//!
//! [herdr](https://herdr.dev) is a terminal multiplexer for coding agents
//! whose sidebar shows each agent as `working`, `blocked` or `idle`. It reads
//! the agents it knows off their screens; Alter Zero it learns from reports —
//! herdr's documented vendor route — sent by a reporter the **lifecycle
//! hooks** run at each event (`alter-zero integration hook herdr`, the
//! commands [`hooks_file_json`] installs). Each event is a separate process,
//! so what the reporter needs to remember between them is a [`State`] in a
//! small file the boundary locks around [`step`]; everything here is the
//! decision, no I/O anywhere — the socket, the lock and the file are
//! `tui::integration_cli`'s.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::json;

/// The reporter's `source` and `agent` in every request — herdr's rule for a
/// vendor integration is to name itself, never an agent herdr supports, and
/// never to start the source with `herdr:`.
pub const SOURCE: &str = "alter-zero";
pub const AGENT: &str = "alter-zero";

/// Set in every terminal the agent's own tools run (`pty::spawn`), so an
/// Alter Zero started **from** one — sharing the pane's `HERDR_*` variables
/// — knows the pane is the outer session's and reports nothing (codex's
/// `CODEX_THREAD_ID` guard, `docs/herdr.md`).
pub const NESTED_VAR: &str = "ALTER_ZERO_AGENT";

/// The question tool: its call puts a question on screen, so it is a prompt
/// the user must answer, like a permission prompt.
const ASK_TOOL: &str = "askuserquestion";

/// The command every installed hook runs: `sh` checks it is inside herdr
/// before the reporter is even started, so a session outside herdr pays one
/// `sh` per event; `ALTER_ZERO_BIN_PATH` is the running binary, set by the
/// engine for every hook child, so the file never goes stale.
pub const HOOK_COMMAND: &str =
    r#"[ "${HERDR_ENV:-}" = 1 ] || exit 0; exec "$ALTER_ZERO_BIN_PATH" integration hook herdr"#;

/// The installed hooks' timeout — herdr's own for its integrations' hooks.
pub const HOOK_TIMEOUT_SECS: u64 = 10;

/// The events the integration listens to, with the matcher each needs —
/// [`step`]'s whole table (`docs/herdr.md`).
pub const HOOK_EVENTS: &[(&str, Option<&str>)] = &[
    ("SessionStart", None),
    ("UserPromptSubmit", None),
    ("PreToolUse", None),
    ("PostToolUse", Some(ASK_TOOL)),
    ("PostToolUseFailure", Some(ASK_TOOL)),
    ("Notification", Some("permission_prompt")),
    ("PermissionResult", None),
    ("Stop", None),
    ("StopFailure", None),
    ("Interrupt", None),
    ("PreCompact", None),
    ("PostCompact", None),
    ("SessionEnd", None),
];

/// herdr's limits on a resume command: at most this many arguments, this
/// many bytes in all.
const RESUME_MAX_ARGS: usize = 64;
const RESUME_MAX_BYTES: usize = 8192;

/// The herdr pane this process runs in, from the variables herdr sets for
/// every process in a pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    /// `HERDR_PANE_ID` — `w1:p2`.
    pub id: String,
    /// `HERDR_SOCKET_PATH` — the server's API socket.
    pub socket: PathBuf,
}

impl Pane {
    /// The pane `var` names — `None` outside herdr (`HERDR_ENV` is not
    /// exactly `1`, or the pane or socket is missing), and `None` inside an
    /// Alter Zero agent's own terminal ([`NESTED_VAR`]), where the pane
    /// belongs to the session that started this one.
    #[must_use]
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let set = |name: &str| var(name).filter(|value| !value.is_empty());
        if var("HERDR_ENV").as_deref() != Some("1") || set(NESTED_VAR).is_some() {
            return None;
        }
        Some(Self {
            id: set("HERDR_PANE_ID")?,
            socket: PathBuf::from(set("HERDR_SOCKET_PATH")?),
        })
    }

    /// The name of this pane's state file: a stable hash (FNV-1a) of the
    /// socket and the pane id — two herdr servers can both have a `w1:p1` —
    /// in hex, so it is always a plain file name.
    #[must_use]
    pub fn state_file_name(&self) -> String {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let socket = self.socket.to_string_lossy();
        for byte in socket
            .as_bytes()
            .iter()
            .chain(&[0])
            .chain(self.id.as_bytes())
        {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        format!("{hash:016x}.json")
    }
}

/// One hook payload, as far as the reporter reads it — every field optional,
/// so a payload from a newer build (or a hand-run test) never fails it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Event {
    #[serde(default, rename = "hook_event_name")]
    pub name: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    /// Present when a subagent acted (`docs/hooks.md`).
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub notification_type: Option<String>,
    /// `SessionEnd`'s.
    #[serde(default)]
    pub reason: Option<String>,
}

impl Event {
    /// Read a payload; `None` when it is not one.
    #[must_use]
    pub fn parse(json: &str) -> Option<Self> {
        serde_json::from_str(json).ok()
    }
}

/// What the reporter remembers between events — one file per pane.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// Whether the lead's turn is running.
    #[serde(default)]
    pub turn: bool,
    /// The prompts on screen waiting for the user, by tool call id.
    #[serde(default)]
    pub waiting: BTreeSet<String>,
    /// The last `seq` sent ([`next_seq`]).
    #[serde(default)]
    pub seq: u64,
}

impl State {
    /// Read a state file; anything unreadable is a fresh state — the next
    /// `SessionStart` rewrites it anyway.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        serde_json::from_str(text).unwrap_or_default()
    }

    /// The file's text.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// What herdr is told: `blocked` while any prompt waits on the user,
    /// else `working` while the turn runs, else `idle`.
    #[must_use]
    pub fn agent_state(&self) -> AgentState {
        if !self.waiting.is_empty() {
            AgentState::Blocked
        } else if self.turn {
            AgentState::Working
        } else {
            AgentState::Idle
        }
    }
}

/// herdr's semantic agent state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Working,
    Blocked,
}

impl AgentState {
    /// The wire spelling.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
        }
    }
}

/// What to tell herdr about one event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// `pane.report_agent`: the state, and the conversation's id when it can
    /// be resumed (it has a rollout file).
    State {
        state: AgentState,
        session: Option<String>,
    },
    /// `pane.release_agent`: the session really ended.
    Release,
}

/// Fold one event into `state` and say what herdr should hear — `None` for
/// an event the integration does not read. The whole table is
/// `docs/herdr.md`'s *Events to states*.
pub fn step(state: &mut State, event: &Event) -> Option<Report> {
    let lead = event.agent_id.is_none();
    let asks = event.tool_name.as_deref() == Some(ASK_TOOL);
    let call = event.tool_use_id.clone();
    match event.name.as_str() {
        // A new conversation: nothing running, nothing asked. The `seq`
        // survives — herdr compares it across restarts.
        "SessionStart" => {
            *state = State {
                seq: state.seq,
                ..State::default()
            };
        }
        "UserPromptSubmit" | "PreCompact" => state.turn = true,
        "PreToolUse" => {
            // A subagent works on its own clock — often long after the
            // lead's turn ended — so only the lead's calls mean the turn
            // runs. Its questions are on screen all the same.
            if lead {
                state.turn = true;
            }
            if asks && let Some(call) = call {
                state.waiting.insert(call);
            }
        }
        "PostToolUse" | "PostToolUseFailure" => {
            if asks && let Some(call) = call {
                state.waiting.remove(&call);
            }
        }
        "Notification" => {
            if event.notification_type.as_deref() != Some("permission_prompt") {
                return None;
            }
            if let Some(call) = call {
                state.waiting.insert(call);
            }
        }
        "PermissionResult" => {
            if let Some(call) = call {
                state.waiting.remove(&call);
            }
        }
        "Stop" | "StopFailure" => {
            if lead {
                state.turn = false;
            }
        }
        // Esc tears the turn down, and every prompt it raised with it.
        "Interrupt" => {
            state.turn = false;
            state.waiting.clear();
        }
        "PostCompact" => state.turn = false,
        // A `/clear` or `/resume` replaces the conversation in the same
        // process: the `SessionStart` that follows reports it. Anything
        // else is the session ending.
        "SessionEnd" => {
            return match event.reason.as_deref() {
                Some("clear" | "resume") => None,
                _ => Some(Report::Release),
            };
        }
        _ => return None,
    }
    Some(Report::State {
        state: state.agent_state(),
        // Only a conversation with a file behind it can be resumed: a fresh
        // one has none until its first message is recorded.
        session: event
            .transcript_path
            .as_ref()
            .filter(|path| !path.is_empty())
            .and(event.session_id.clone())
            .filter(|id| !id.is_empty()),
    })
}

/// The next `seq`: never below `now_ns` (herdr compares it across restarts,
/// so a timestamp), and strictly above the last one sent, so a clock that
/// stepped back can never make a newer report look stale.
pub fn next_seq(state: &mut State, now_ns: u64) -> u64 {
    let seq = now_ns.max(state.seq.saturating_add(1));
    state.seq = seq;
    seq
}

/// The command that resumes `session` — `alter-zero --resume <id>` — or
/// `None` when herdr would refuse it, which would refuse the state report
/// it rides with too: the first word a plain command name (`[A-Za-z0-9_.-]`,
/// not starting with `-`), no argument holding an apostrophe or a control
/// character, at most 64 arguments and 8 KiB.
#[must_use]
pub fn resume_argv(program: &str, session: &str) -> Option<Vec<String>> {
    let plain = !program.is_empty()
        && !program.starts_with('-')
        && program
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    let argv = vec![
        program.to_string(),
        "--resume".to_string(),
        session.to_string(),
    ];
    let clean = argv
        .iter()
        .all(|arg| !arg.is_empty() && !arg.contains('\'') && !arg.chars().any(char::is_control));
    let bytes: usize = argv.iter().map(String::len).sum();
    (plain && clean && argv.len() <= RESUME_MAX_ARGS && bytes <= RESUME_MAX_BYTES).then_some(argv)
}

/// The request line for `report` — one JSON object and its newline, the
/// whole of what herdr reads per connection. `program` is the binary's file
/// name, for the resume command.
#[must_use]
pub fn request_line(pane: &Pane, report: &Report, seq: u64, program: &str) -> String {
    let id = format!("{SOURCE}:{seq}");
    let request = match report {
        Report::State { state, session } => {
            let mut params = json!({
                "pane_id": pane.id,
                "source": SOURCE,
                "agent": AGENT,
                "state": state.label(),
                "seq": seq,
            });
            if let Some(session) = session
                && let Some(argv) = resume_argv(program, session)
            {
                params["agent_session_id"] = json!(session);
                params["resume_argv"] = json!(argv);
            }
            json!({ "id": id, "method": "pane.report_agent", "params": params })
        }
        Report::Release => json!({
            "id": id,
            "method": "pane.release_agent",
            "params": { "pane_id": pane.id, "source": SOURCE, "agent": AGENT, "seq": seq },
        }),
    };
    format!("{request}\n")
}

/// The hooks file `alter-zero integration install herdr` writes to
/// `hooks.d/herdr.json` — one group per [`HOOK_EVENTS`] entry, each running
/// [`HOOK_COMMAND`].
#[must_use]
pub fn hooks_file_json() -> String {
    let mut hooks = serde_json::Map::new();
    for (event, matcher) in HOOK_EVENTS {
        let mut group = serde_json::Map::new();
        if let Some(matcher) = matcher {
            group.insert("matcher".into(), json!(matcher));
        }
        group.insert(
            "hooks".into(),
            json!([{ "type": "command", "command": HOOK_COMMAND, "timeout": HOOK_TIMEOUT_SECS }]),
        );
        hooks.insert((*event).to_string(), json!([group]));
    }
    let file = json!({
        "description": "Reports Alter Zero's state to the herdr pane it runs in \
                        (docs/herdr.md). Written by `alter-zero integration install herdr`, \
                        which overwrites it; `alter-zero integration uninstall herdr` removes it.",
        "hooks": hooks,
    });
    let mut text = serde_json::to_string_pretty(&file).unwrap_or_default();
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    const INSIDE: &[(&str, &str)] = &[
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w1:p2"),
        ("HERDR_SOCKET_PATH", "/run/herdr/default.sock"),
    ];

    fn pane() -> Pane {
        Pane::from_env(env(INSIDE)).expect("inside herdr")
    }

    fn event(name: &str) -> Event {
        Event {
            name: name.to_string(),
            session_id: Some("19a8f3c2-1a2b".to_string()),
            ..Event::default()
        }
    }

    fn tool(name: &str, tool: &str, id: &str) -> Event {
        Event {
            tool_name: Some(tool.to_string()),
            tool_use_id: Some(id.to_string()),
            ..event(name)
        }
    }

    fn prompt(id: &str) -> Event {
        Event {
            notification_type: Some("permission_prompt".to_string()),
            ..tool("Notification", "bash", id)
        }
    }

    fn sub(mut event: Event) -> Event {
        event.agent_id = Some("agent-1".to_string());
        event
    }

    /// The states herdr hears for `events`, in order (a `None` step is
    /// skipped).
    fn heard(state: &mut State, events: &[Event]) -> Vec<&'static str> {
        events
            .iter()
            .filter_map(|event| match step(state, event)? {
                Report::State { state, .. } => Some(state.label()),
                Report::Release => Some("release"),
            })
            .collect()
    }

    // ----- the pane -----

    #[test]
    fn the_pane_is_read_from_herdrs_variables() {
        assert_eq!(
            pane(),
            Pane {
                id: "w1:p2".to_string(),
                socket: PathBuf::from("/run/herdr/default.sock"),
            }
        );
    }

    #[test]
    fn outside_herdr_there_is_no_pane() {
        for env_value in ["0", "true", "", " 1"] {
            let mut vars = INSIDE.to_vec();
            vars[0] = ("HERDR_ENV", env_value);
            assert_eq!(Pane::from_env(env(&vars)), None, "HERDR_ENV={env_value:?}");
        }
        assert_eq!(Pane::from_env(env(&INSIDE[1..])), None, "HERDR_ENV unset");
        for missing in ["HERDR_PANE_ID", "HERDR_SOCKET_PATH"] {
            let vars: Vec<(&str, &str)> = INSIDE
                .iter()
                .copied()
                .filter(|(k, _)| *k != missing)
                .collect();
            assert_eq!(Pane::from_env(env(&vars)), None, "{missing} unset");
            let blank: Vec<(&str, &str)> = INSIDE
                .iter()
                .map(|&(k, v)| if k == missing { (k, "") } else { (k, v) })
                .collect();
            assert_eq!(Pane::from_env(env(&blank)), None, "{missing} blank");
        }
    }

    #[test]
    fn an_alter_zero_started_from_the_agents_own_terminal_leaves_the_pane_alone() {
        let mut vars = INSIDE.to_vec();
        vars.push((NESTED_VAR, "1"));
        assert_eq!(Pane::from_env(env(&vars)), None);
    }

    #[test]
    fn the_state_file_is_a_plain_name_per_server_and_pane() {
        let name = pane().state_file_name();
        assert_eq!(name, pane().state_file_name(), "stable");
        assert!(name.ends_with(".json"));
        assert!(
            name.trim_end_matches(".json")
                .chars()
                .all(|c| c.is_ascii_hexdigit()),
            "{name}"
        );
        let other_pane = Pane {
            id: "w1:p3".to_string(),
            ..pane()
        };
        let other_server = Pane {
            socket: PathBuf::from("/run/herdr/test.sock"),
            ..pane()
        };
        assert_ne!(name, other_pane.state_file_name());
        assert_ne!(name, other_server.state_file_name());
    }

    // ----- events to states -----

    #[test]
    fn a_turn_is_working_from_its_prompt_to_its_stop() {
        let mut state = State::default();
        assert_eq!(
            heard(
                &mut state,
                &[
                    event("SessionStart"),
                    event("UserPromptSubmit"),
                    tool("PreToolUse", "read", "c1"),
                    event("Stop"),
                ]
            ),
            vec!["idle", "working", "working", "idle"]
        );
    }

    #[test]
    fn a_permission_prompt_blocks_until_it_is_answered() {
        let mut state = State::default();
        assert_eq!(
            heard(
                &mut state,
                &[
                    event("UserPromptSubmit"),
                    tool("PreToolUse", "bash", "c1"),
                    prompt("c1"),
                    tool("PermissionResult", "bash", "c1"),
                    event("Stop"),
                ]
            ),
            vec!["working", "working", "blocked", "working", "idle"]
        );
    }

    #[test]
    fn two_prompts_on_screen_block_until_both_are_answered_in_any_order() {
        // Parallel agents ask before any one is answered
        // (docs/permissions.md): answering the first leaves the second on
        // screen, and the order their hooks reach the reporter is theirs.
        for (first, second) in [("c1", "c2"), ("c2", "c1")] {
            let mut state = State::default();
            assert_eq!(
                heard(
                    &mut state,
                    &[
                        event("UserPromptSubmit"),
                        prompt("c1"),
                        sub(prompt("c2")),
                        tool("PermissionResult", "bash", first),
                        tool("PermissionResult", "bash", second),
                    ]
                ),
                vec!["working", "blocked", "blocked", "blocked", "working"]
            );
        }
    }

    #[test]
    fn a_question_blocks_until_it_is_answered_declined_or_timed_out() {
        for done in ["PostToolUse", "PostToolUseFailure"] {
            let mut state = State::default();
            assert_eq!(
                heard(
                    &mut state,
                    &[
                        event("UserPromptSubmit"),
                        tool("PreToolUse", ASK_TOOL, "q1"),
                        tool(done, ASK_TOOL, "q1"),
                        event("Stop"),
                    ]
                ),
                vec!["working", "blocked", "working", "idle"],
                "{done}"
            );
        }
    }

    #[test]
    fn a_background_agents_work_never_reads_as_the_turn_but_its_prompts_block() {
        // The lead's turn ended; a background agent keeps working — the
        // composer waits on the user, so the pane is idle, until the agent
        // puts a prompt on screen, which needs the user whatever the lead
        // is doing.
        let mut state = State::default();
        assert_eq!(
            heard(
                &mut state,
                &[
                    event("UserPromptSubmit"),
                    event("Stop"),
                    sub(tool("PreToolUse", "bash", "s1")),
                    sub(prompt("s2")),
                    sub(tool("PermissionResult", "bash", "s2")),
                    sub(tool("PostToolUse", "bash", "s2")),
                ]
            ),
            vec!["working", "idle", "idle", "blocked", "idle", "idle"]
        );
    }

    #[test]
    fn a_subagents_failed_round_is_not_the_turn_ending() {
        let mut state = State::default();
        assert_eq!(
            heard(
                &mut state,
                &[
                    event("UserPromptSubmit"),
                    sub(event("StopFailure")),
                    event("StopFailure"),
                ]
            ),
            vec!["working", "working", "idle"]
        );
    }

    #[test]
    fn an_interrupt_ends_the_turn_and_every_prompt_it_raised() {
        let mut state = State::default();
        assert_eq!(
            heard(
                &mut state,
                &[
                    event("UserPromptSubmit"),
                    prompt("c1"),
                    tool("PreToolUse", ASK_TOOL, "q1"),
                    event("Interrupt"),
                ]
            ),
            vec!["working", "blocked", "blocked", "idle"]
        );
        assert!(state.waiting.is_empty());
    }

    #[test]
    fn a_compaction_is_a_turn_of_its_own() {
        let mut state = State::default();
        assert_eq!(
            heard(&mut state, &[event("PreCompact"), event("PostCompact")]),
            vec!["working", "idle"]
        );
    }

    #[test]
    fn a_new_conversation_forgets_a_stale_turn_but_not_the_seq() {
        let mut state = State {
            turn: true,
            waiting: ["c9".to_string()].into(),
            seq: 41,
        };
        assert_eq!(heard(&mut state, &[event("SessionStart")]), vec!["idle"]);
        assert_eq!(
            state,
            State {
                seq: 41,
                ..State::default()
            }
        );
    }

    #[test]
    fn only_a_quit_releases_the_pane() {
        let end = |reason: &str| Event {
            reason: Some(reason.to_string()),
            ..event("SessionEnd")
        };
        let mut state = State::default();
        for reason in ["prompt_input_exit", "other", "logout"] {
            assert_eq!(
                step(&mut state, &end(reason)),
                Some(Report::Release),
                "{reason}"
            );
        }
        // The same process carries on with another conversation: herdr asks
        // for that to be reported, not released.
        for reason in ["clear", "resume"] {
            assert_eq!(step(&mut state, &end(reason)), None, "{reason}");
        }
    }

    #[test]
    fn events_the_integration_does_not_read_say_nothing() {
        let mut state = State::default();
        for name in [
            "SubagentStop",
            "SubagentStart",
            "PermissionRequest",
            "FileChanged",
            "",
        ] {
            assert_eq!(step(&mut state, &event(name)), None, "{name:?}");
        }
        let idle_note = Event {
            notification_type: Some("idle_prompt".to_string()),
            ..event("Notification")
        };
        assert_eq!(step(&mut state, &idle_note), None);
    }

    #[test]
    fn the_conversation_rides_only_once_it_has_a_file_to_resume() {
        let mut state = State::default();
        let fresh = event("UserPromptSubmit");
        assert_eq!(
            step(&mut state, &fresh),
            Some(Report::State {
                state: AgentState::Working,
                session: None,
            }),
            "no rollout yet: nothing to resume"
        );
        let recorded = Event {
            transcript_path: Some("/r/rollout-x.jsonl".to_string()),
            ..event("Stop")
        };
        assert_eq!(
            step(&mut state, &recorded),
            Some(Report::State {
                state: AgentState::Idle,
                session: Some("19a8f3c2-1a2b".to_string()),
            })
        );
    }

    // ----- seq -----

    #[test]
    fn seq_is_a_timestamp_that_only_ever_climbs() {
        let mut state = State::default();
        assert_eq!(next_seq(&mut state, 1_000), 1_000);
        assert_eq!(
            next_seq(&mut state, 1_000),
            1_001,
            "same instant: still higher"
        );
        assert_eq!(next_seq(&mut state, 900), 1_002, "the clock stepped back");
        assert_eq!(next_seq(&mut state, 5_000), 5_000);
        assert_eq!(state.seq, 5_000);
    }

    // ----- the requests -----

    #[test]
    fn the_resume_command_obeys_herdrs_rules_or_is_left_off() {
        assert_eq!(
            resume_argv("alter-zero", "19a8f3c2-1a2b"),
            Some(vec![
                "alter-zero".to_string(),
                "--resume".to_string(),
                "19a8f3c2-1a2b".to_string()
            ])
        );
        for program in [
            "",
            "-alter-zero",
            "/usr/bin/alter-zero",
            "alter zero",
            "alter'zero",
        ] {
            assert_eq!(resume_argv(program, "id"), None, "{program:?}");
        }
        for session in ["", "it's", "a\nb", &"x".repeat(9000)] {
            assert_eq!(resume_argv("alter-zero", session), None, "{session:?}");
        }
    }

    #[test]
    fn a_state_report_is_one_line_naming_the_pane_and_carrying_the_resume() {
        let line = request_line(
            &pane(),
            &Report::State {
                state: AgentState::Blocked,
                session: Some("19a8f3c2-1a2b".to_string()),
            },
            1_759_770_000_000_000_001,
            "alter-zero",
        );
        assert!(
            line.ends_with('\n') && line.matches('\n').count() == 1,
            "{line:?}"
        );
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            value,
            json!({
                "id": "alter-zero:1759770000000000001",
                "method": "pane.report_agent",
                "params": {
                    "pane_id": "w1:p2",
                    "source": "alter-zero",
                    "agent": "alter-zero",
                    "state": "blocked",
                    "seq": 1_759_770_000_000_000_001_u64,
                    "agent_session_id": "19a8f3c2-1a2b",
                    "resume_argv": ["alter-zero", "--resume", "19a8f3c2-1a2b"],
                }
            })
        );
    }

    #[test]
    fn a_resume_herdr_would_refuse_is_left_off_rather_than_sink_the_report() {
        let line = request_line(
            &pane(),
            &Report::State {
                state: AgentState::Idle,
                session: Some("19a8f3c2-1a2b".to_string()),
            },
            7,
            "alter zero",
        );
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["params"]["state"], json!("idle"));
        assert!(value["params"].get("resume_argv").is_none());
        assert!(value["params"].get("agent_session_id").is_none());
    }

    #[test]
    fn a_release_names_the_pane_and_the_reporter() {
        let line = request_line(&pane(), &Report::Release, 9, "alter-zero");
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            value,
            json!({
                "id": "alter-zero:9",
                "method": "pane.release_agent",
                "params": {
                    "pane_id": "w1:p2",
                    "source": "alter-zero",
                    "agent": "alter-zero",
                    "seq": 9,
                }
            })
        );
    }

    // ----- the installed file -----

    #[test]
    fn the_installed_file_is_a_hooks_file_running_the_reporter_on_every_event() {
        use crate::hooks::{HookEvent, HooksFile};
        let file = HooksFile::parse(&hooks_file_json()).expect("a valid hooks.json");
        for (name, matcher) in HOOK_EVENTS {
            let event = HookEvent::from_name(name)
                .unwrap_or_else(|| panic!("{name} is not an event the engine fires"));
            let query = matcher.or(match event {
                HookEvent::PreToolUse | HookEvent::PermissionResult => Some("bash"),
                HookEvent::SessionStart => Some("startup"),
                HookEvent::SessionEnd => Some("prompt_input_exit"),
                HookEvent::PreCompact | HookEvent::PostCompact => Some("manual"),
                HookEvent::StopFailure => Some("rate_limit"),
                _ => None,
            });
            let selection = file.select(event, query);
            assert!(
                selection.warnings.is_empty(),
                "{name}: {:?}",
                selection.warnings
            );
            assert_eq!(selection.handlers.len(), 1, "{name}");
            assert_eq!(selection.handlers[0].command, HOOK_COMMAND, "{name}");
            assert_eq!(
                selection.handlers[0].timeout_secs, HOOK_TIMEOUT_SECS,
                "{name}"
            );
        }
        // The question's end alone — not every call's.
        assert!(
            file.select(HookEvent::PostToolUse, Some("bash"))
                .handlers
                .is_empty()
        );
        assert!(
            file.select(HookEvent::Notification, Some("idle_prompt"))
                .handlers
                .is_empty()
        );
    }

    #[test]
    fn the_installed_command_never_starts_the_reporter_outside_herdr() {
        assert!(HOOK_COMMAND.starts_with(r#"[ "${HERDR_ENV:-}" = 1 ] || exit 0;"#));
        assert!(HOOK_COMMAND.contains(r#""$ALTER_ZERO_BIN_PATH" integration hook herdr"#));
    }
}
