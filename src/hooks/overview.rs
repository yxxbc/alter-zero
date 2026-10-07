//! The pure display tree behind the read-only `/hooks` menu
//! (`docs/hooks-menu.md`).
//!
//! [`HooksOverview::from_file`] digests a parsed [`HooksFile`] into what the
//! menu browses: every modelled event in registry order, each event's matcher
//! groups merged by matcher string, and every handler — the skipped kinds
//! included, because a *browser* reports what is configured, not what runs.
//! The per-event summary/description strings and the matcher flag describe
//! **this runner's** semantics (`llm/hooks.rs`), not the reference's.

use super::config::{HandlerKind, HooksFile};
use super::event::HookEvent;

/// The matcher label shown for a group that matches everything (an absent or
/// empty `matcher`) — Claude Code's `(all)`.
pub const MATCHER_ALL_LABEL: &str = "(all)";

/// The list label shown for a handler with nothing to display — a non-command
/// kind whose content field (`prompt`, `url`, …) this config model doesn't
/// carry, or a command handler missing its command.
pub const HOOK_NO_CONTENT_LABEL: &str = "(not set)";

/// The digested `hooks.json` the menu browses. Built once at open
/// ([`HooksOverview::from_file`]) from the same parsed file the runner holds,
/// so the browser and the dispatcher can never disagree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HooksOverview {
    /// Every modelled event in [`HookEvent::ALL`] order — always all sixteen,
    /// hooks or none, so the menu lists what *could* fire.
    pub events: Vec<EventOverview>,
}

/// One event's configured hooks, grouped by matcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventOverview {
    pub event: HookEvent,
    /// The event's matcher groups in config order (first appearance), groups
    /// sharing a matcher string merged — the reference's grouping.
    pub matchers: Vec<MatcherOverview>,
}

/// One matcher row: the groups under an event that share a matcher string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatcherOverview {
    /// The raw matcher string; empty = match-everything (labelled
    /// [`MATCHER_ALL_LABEL`]).
    pub matcher: String,
    pub hooks: Vec<HookOverview>,
}

/// One configured handler, resolved to display strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookOverview {
    /// The handler's `"type"` — `command`, or a kind we parse but never run
    /// (`prompt`, `agent`, …).
    pub kind: String,
    /// The real command text (empty when the handler has none) — the detail
    /// box always shows this, never the status message.
    pub content: String,
    /// `statusMessage`, when set — stands in for the command in list rows
    /// (Claude Code's `getHookDisplayText`).
    pub status_message: Option<String>,
    /// Where the hook came from when it is **not** the user's own layer: the
    /// display path of the `hooks.d/` integration file that installed it
    /// (`docs/herdr.md`) — such a hook runs whatever the per-directory row
    /// says. `None` is `hooks.json` (and the trusted project file merged
    /// into it), which the row governs.
    pub origin: Option<String>,
}

impl HooksOverview {
    /// Digest a parsed file. Events the file names that we don't model never
    /// show (the reference iterates its own registry the same way).
    #[must_use]
    pub fn from_file(file: &HooksFile) -> Self {
        let mut overview = Self {
            events: HookEvent::ALL
                .iter()
                .map(|&event| EventOverview {
                    event,
                    matchers: Vec::new(),
                })
                .collect(),
        };
        overview.add_layer(file, None);
        overview
    }

    /// This overview with an integration file's hooks listed after the
    /// user's, each naming `origin` — the `hooks.d/` layer, which runs in
    /// every directory (`docs/herdr.md`).
    #[must_use]
    pub fn with_integration(mut self, file: &HooksFile, origin: &str) -> Self {
        self.add_layer(file, Some(origin));
        self
    }

    /// Append `file`'s groups per event, merged into the rows by matcher
    /// string (first appearance wins the position).
    fn add_layer(&mut self, file: &HooksFile, origin: Option<&str>) {
        for overview in &mut self.events {
            let groups = file
                .hooks
                .get(overview.event.name())
                .map_or(&[][..], Vec::as_slice);
            for group in groups {
                let key = group.matcher.clone().unwrap_or_default();
                let row = match overview.matchers.iter_mut().position(|m| m.matcher == key) {
                    Some(index) => &mut overview.matchers[index],
                    None => {
                        overview.matchers.push(MatcherOverview {
                            matcher: key,
                            hooks: Vec::new(),
                        });
                        overview.matchers.last_mut().expect("just pushed")
                    }
                };
                row.hooks.extend(group.hooks.iter().map(|handler| {
                    HookOverview {
                        kind: match &handler.kind {
                            HandlerKind::Command => "command".to_string(),
                            HandlerKind::Other(name) => name.clone(),
                        },
                        content: handler
                            .command
                            .as_deref()
                            .map(str::trim)
                            .unwrap_or_default()
                            .to_string(),
                        status_message: handler
                            .status_message
                            .clone()
                            .filter(|m| !m.trim().is_empty()),
                        origin: origin.map(str::to_string),
                    }
                }));
            }
        }
    }

    /// Every configured handler across the modelled events — the `{N} hooks
    /// configured` count.
    #[must_use]
    pub fn total(&self) -> usize {
        self.events.iter().map(EventOverview::count).sum()
    }

    /// The handlers of the user's own layer alone — the ones the
    /// per-directory **Hooks** row governs, and so the ones the "off here"
    /// note is about.
    #[must_use]
    pub fn user_total(&self) -> usize {
        self.events
            .iter()
            .flat_map(|event| &event.matchers)
            .flat_map(|matcher| &matcher.hooks)
            .filter(|hook| hook.origin.is_none())
            .count()
    }
}

impl EventOverview {
    /// The handlers configured under this event, across its matchers — the
    /// `({count})` suffix on the event row.
    #[must_use]
    pub fn count(&self) -> usize {
        self.matchers.iter().map(|m| m.hooks.len()).sum()
    }

    /// The handlers level 3 lists: one matcher row's hooks (`Some(index)` —
    /// out of range is empty), or — for a matcher-less event — every handler
    /// of the event flattened in matcher-row order (`None`).
    #[must_use]
    pub fn hooks_at(&self, matcher: Option<usize>) -> Vec<&HookOverview> {
        match matcher {
            Some(i) => self
                .matchers
                .get(i)
                .map_or_else(Vec::new, |m| m.hooks.iter().collect()),
            None => self.matchers.iter().flat_map(|m| m.hooks.iter()).collect(),
        }
    }
}

impl MatcherOverview {
    /// The row label: the raw matcher, or [`MATCHER_ALL_LABEL`] for empty.
    #[must_use]
    pub fn label(&self) -> &str {
        if self.matcher.is_empty() {
            MATCHER_ALL_LABEL
        } else {
            &self.matcher
        }
    }
}

impl HookOverview {
    /// The list-row text: the status message when set, else the command, else
    /// [`HOOK_NO_CONTENT_LABEL`].
    #[must_use]
    pub fn display(&self) -> &str {
        if let Some(message) = &self.status_message {
            return message;
        }
        if self.content.is_empty() {
            HOOK_NO_CONTENT_LABEL
        } else {
            &self.content
        }
    }
}

/// The event's one-line summary — the description column of the events list.
#[must_use]
pub const fn event_summary(event: HookEvent) -> &'static str {
    match event {
        HookEvent::PreToolUse => "Before tool execution",
        HookEvent::PostToolUse => "After tool execution",
        HookEvent::PostToolUseFailure => "After a tool call fails",
        HookEvent::PermissionRequest => "When a tool call needs permission",
        HookEvent::PermissionResult => "When a permission request is decided",
        HookEvent::Notification => "When the user's attention is needed",
        HookEvent::UserPromptSubmit => "When the user submits a prompt",
        HookEvent::SessionStart => "When a session starts",
        HookEvent::SessionEnd => "When the session ends",
        HookEvent::Stop => "When the model finishes its reply",
        HookEvent::StopFailure => "When a turn ends on an error",
        HookEvent::Interrupt => "When the user interrupts a turn",
        HookEvent::SubagentStart => "When a subagent is launched",
        HookEvent::SubagentStop => "When a subagent finishes",
        HookEvent::PreCompact => "Before context compaction",
        HookEvent::PostCompact => "After context compaction",
    }
}

/// The event's multi-line description (stdin payload + exit-code semantics,
/// as **this** runner implements them — `docs/hooks.md`), shown under the
/// matcher/hook titles. Lines are `\n`-separated.
#[must_use]
pub const fn event_description(event: HookEvent) -> &'static str {
    match event {
        HookEvent::PreToolUse => {
            "Input to command is the tool call as snake_case JSON (tool_name, tool_input).\n\
             Exit code 0 - stdout JSON may allow, deny, ask, or rewrite the call\n\
             Exit code 2 - block the tool call; stderr is the reason the model reads\n\
             Other exit codes - non-blocking error; the call still runs"
        }
        HookEvent::PostToolUse => {
            "Input to command is JSON with tool_name, tool_input, and tool_response; \
             runs only after a call that succeeded.\n\
             Exit code 0 - stdout JSON may block or feed the model extra context\n\
             Exit code 2 - block; stderr is shown to the model\n\
             Other exit codes - non-blocking error"
        }
        HookEvent::PostToolUseFailure => {
            "Input to command is JSON with tool_name, tool_input, and error; runs \
             only after a call that failed.\n\
             Exit code 0 - stdout JSON may block or feed the model extra context\n\
             Exit code 2 - block; stderr is shown to the model\n\
             Other exit codes - non-blocking error"
        }
        HookEvent::PermissionResult => {
            "Runs once a permission request is decided; decision.behavior is allow \
             or deny, decision.source is user, hook, or classifier.\n\
             Observation only - output and exit codes are ignored"
        }
        HookEvent::Notification => {
            "Runs when the user's attention is needed; the matcher selects the \
             notification_type (permission_prompt: a prompt is on screen now).\n\
             Observation only - output and exit codes are ignored"
        }
        HookEvent::PermissionRequest => {
            "Runs where the permission prompt or auto-mode classifier would ask.\n\
             Exit code 0 - stdout JSON may allow or deny in the user's stead \
             (\"ask\" falls back to the prompt)\n\
             Exit code 2 - deny with stderr as the reason\n\
             Other exit codes - non-blocking error; the normal prompt asks"
        }
        HookEvent::UserPromptSubmit => {
            "Input to command is JSON with the submitted prompt text.\n\
             Exit code 0 - stdout JSON may block the prompt or add context\n\
             Exit code 2 - block the prompt; stderr is the reason shown\n\
             Other exit codes - non-blocking error; the prompt goes through"
        }
        HookEvent::SessionStart => {
            "Runs in the background as a session opens - at startup, /resume, or \
             /clear - and the next turn waits for it; the matcher selects the source.\n\
             Exit code 0 - stdout becomes context for the model\n\
             Blocking is ignored\n\
             Other exit codes - non-blocking error"
        }
        HookEvent::SessionEnd => {
            "Runs as the conversation closes (/clear, /resume, quit); the matcher \
             selects the reason.\n\
             Fire-and-forget under a 2s budget - output and exit codes are ignored"
        }
        HookEvent::Stop => {
            "Runs when the model stops answering; an interrupt never fires it.\n\
             Exit code 0 - a stdout JSON block makes the model keep going \
             (the reason becomes the next user message)\n\
             Exit code 2 - continue the turn with stderr as the feedback\n\
             Other exit codes - non-blocking error; the turn ends"
        }
        HookEvent::StopFailure => {
            "Runs when a turn ends on an error instead of a reply - the provider's, \
             or the tool-call limit; the matcher selects the error type (rate_limit, \
             server_error, ...).\n\
             Observation only - output and exit codes are ignored"
        }
        HookEvent::Interrupt => {
            "Runs when the user interrupts a running turn (Esc); Stop does not \
             fire for it.\n\
             Observation only, under a 2s budget - output and exit codes are ignored"
        }
        HookEvent::SubagentStart => {
            "Runs when an Agent-tool subagent launches; the matcher selects the \
             agent type.\n\
             Exit code 0 - stdout becomes context for the subagent\n\
             Blocking is ignored\n\
             Other exit codes - non-blocking error"
        }
        HookEvent::SubagentStop => {
            "Runs when a subagent finishes; the matcher selects the agent type.\n\
             Exit code 0 - a stdout JSON block makes the subagent keep going\n\
             Exit code 2 - continue its loop with stderr as the feedback\n\
             Other exit codes - non-blocking error; the subagent finishes"
        }
        HookEvent::PreCompact => {
            "Runs before a /compact summarization; the matcher selects the trigger \
             (manual, auto).\n\
             Exit code 0 - stdout becomes extra compaction instructions\n\
             Blocking is not honoured\n\
             Other exit codes - non-blocking error"
        }
        HookEvent::PostCompact => {
            "Runs after a compaction lands; the matcher selects the trigger \
             (manual, auto).\n\
             Fire-and-forget - output and exit codes are ignored"
        }
    }
}

/// Whether the dispatcher passes a match query for this event — the events
/// whose menu path includes the matcher level, and whose detail page shows a
/// `Matcher:` row. Mirrors the `llm/hooks.rs` dispatch sites: the tool events
/// match the tool name, the subagent events the agent type, `SessionStart`
/// the source, `SessionEnd` the reason, the compact events the trigger,
/// `Notification` its type, `StopFailure` the error type — while `Stop`,
/// `UserPromptSubmit` and `Interrupt` run their groups matcher-or-not.
#[must_use]
pub const fn event_has_matchers(event: HookEvent) -> bool {
    !matches!(
        event,
        HookEvent::Stop | HookEvent::UserPromptSubmit | HookEvent::Interrupt
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two events, three groups (two sharing a matcher), four handlers — one
    /// of them a kind we never run, one carrying a status message.
    const SAMPLE: &str = r#"{
      "hooks": {
        "PreToolUse": [
          { "matcher": "bash",
            "hooks": [
              { "type": "command", "command": "./guard.sh",
                "statusMessage": "checking…" },
              { "type": "prompt" }
            ] },
          { "hooks": [ { "type": "command", "command": "./always.sh" } ] },
          { "matcher": "bash",
            "hooks": [ { "type": "command", "command": "./second.sh" } ] }
        ],
        "Stop": [
          { "hooks": [ { "type": "command", "command": "./done.sh" } ] }
        ],
        "FileChanged": [
          { "hooks": [ { "type": "command", "command": "./never.sh" } ] }
        ]
      }
    }"#;

    fn overview() -> HooksOverview {
        HooksOverview::from_file(&HooksFile::parse(SAMPLE).expect("fixture parses"))
    }

    fn event(overview: &HooksOverview, event: HookEvent) -> &EventOverview {
        overview
            .events
            .iter()
            .find(|e| e.event == event)
            .expect("every modelled event is listed")
    }

    #[test]
    fn every_modelled_event_is_listed_in_registry_order_hooks_or_none() {
        let overview = overview();
        let listed: Vec<HookEvent> = overview.events.iter().map(|e| e.event).collect();
        assert_eq!(listed, HookEvent::ALL.to_vec());
    }

    #[test]
    fn the_total_counts_every_handler_of_every_kind_under_modelled_events() {
        // 4 under PreToolUse (the skipped `prompt` kind included — it is
        // configured, which is what a browser reports) + 1 under Stop; the
        // FileChanged handler is under an event we don't model and never
        // shows, like the runner never fires it.
        assert_eq!(overview().total(), 5);
    }

    #[test]
    fn groups_sharing_a_matcher_merge_into_one_row_in_first_appearance_order() {
        let overview = overview();
        let pre = event(&overview, HookEvent::PreToolUse);
        let labels: Vec<&str> = pre.matchers.iter().map(MatcherOverview::label).collect();
        assert_eq!(labels, vec!["bash", MATCHER_ALL_LABEL]);
        assert_eq!(
            pre.matchers[0]
                .hooks
                .iter()
                .map(|h| h.content.as_str())
                .collect::<Vec<_>>(),
            vec!["./guard.sh", "", "./second.sh"],
            "the second bash group's handler joins the first's row; the \
             prompt kind has no command to show"
        );
        assert_eq!(pre.count(), 4);
    }

    #[test]
    fn hooks_at_indexes_one_matcher_row_or_flattens_the_whole_event() {
        let overview = overview();
        let pre = event(&overview, HookEvent::PreToolUse);
        assert_eq!(pre.hooks_at(Some(0)).len(), 3);
        assert_eq!(pre.hooks_at(Some(1)).len(), 1);
        assert_eq!(pre.hooks_at(Some(9)).len(), 0, "out of range is empty");
        let flat: Vec<&str> = pre
            .hooks_at(None)
            .iter()
            .map(|h| h.content.as_str())
            .collect();
        assert_eq!(flat, vec!["./guard.sh", "", "./second.sh", "./always.sh"]);
    }

    #[test]
    fn a_handler_resolves_to_kind_content_and_status_message() {
        let overview = overview();
        let pre = event(&overview, HookEvent::PreToolUse);
        let guard = &pre.matchers[0].hooks[0];
        assert_eq!(guard.kind, "command");
        assert_eq!(guard.content, "./guard.sh");
        assert_eq!(guard.status_message.as_deref(), Some("checking…"));
        let skipped = &pre.matchers[0].hooks[1];
        assert_eq!(skipped.kind, "prompt");
        assert_eq!(skipped.content, "");
        assert_eq!(skipped.status_message, None);
    }

    #[test]
    fn the_list_row_shows_the_status_message_else_the_command_else_a_placeholder() {
        let overview = overview();
        let pre = event(&overview, HookEvent::PreToolUse);
        assert_eq!(
            pre.matchers[0].hooks[0].display(),
            "checking…",
            "Claude Code's getHookDisplayText: the status message stands in"
        );
        assert_eq!(pre.matchers[0].hooks[2].display(), "./second.sh");
        assert_eq!(pre.matchers[0].hooks[1].display(), HOOK_NO_CONTENT_LABEL);
    }

    #[test]
    fn an_integration_layer_lists_beside_the_users_hooks_naming_its_file() {
        // `hooks.d/` (docs/herdr.md): an integration's hooks run whatever the
        // per-directory row says, so the browser lists them with the file
        // they came from — and only the user's own count toward the "off
        // here" note.
        let integration = HooksFile::parse(
            r#"{"hooks":{
              "PreToolUse":[{"matcher":"bash","hooks":[{"type":"command","command":"report working"}]}],
              "Interrupt":[{"hooks":[{"type":"command","command":"report idle"}]}]}}"#,
        )
        .unwrap();
        let overview =
            overview().with_integration(&integration, "~/.alter-zero/hooks.d/herdr.json");
        assert_eq!(
            overview.total(),
            7,
            "5 of the user's and 2 of the integration's"
        );
        assert_eq!(overview.user_total(), 5);

        let pre = event(&overview, HookEvent::PreToolUse);
        let bash: Vec<(&str, Option<&str>)> = pre.matchers[0]
            .hooks
            .iter()
            .map(|h| (h.content.as_str(), h.origin.as_deref()))
            .collect();
        assert_eq!(
            bash,
            vec![
                ("./guard.sh", None),
                ("", None),
                ("./second.sh", None),
                ("report working", Some("~/.alter-zero/hooks.d/herdr.json")),
            ],
            "the integration's group joins the user's row for the same matcher, after it"
        );
        let interrupt = event(&overview, HookEvent::Interrupt);
        assert_eq!(interrupt.count(), 1);
        assert_eq!(
            interrupt.matchers[0].hooks[0].origin.as_deref(),
            Some("~/.alter-zero/hooks.d/herdr.json")
        );
    }

    #[test]
    fn an_empty_file_still_lists_every_event_at_zero() {
        let overview = HooksOverview::from_file(&HooksFile::default());
        assert_eq!(overview.events.len(), HookEvent::ALL.len());
        assert_eq!(overview.total(), 0);
        assert!(overview.events.iter().all(|e| e.matchers.is_empty()));
    }

    #[test]
    fn every_event_carries_a_summary_and_a_multi_line_description() {
        for &event in HookEvent::ALL {
            assert!(
                !event_summary(event).is_empty(),
                "{} has no summary",
                event.name()
            );
            assert!(
                event_description(event).contains('\n'),
                "{}'s description is not multi-line",
                event.name()
            );
        }
    }

    #[test]
    fn the_session_and_failure_events_say_when_they_run_now() {
        // `SessionStart` runs as the session opens, in the background — not
        // at the first turn, which only waits for it (docs/hooks.md).
        let start = event_description(HookEvent::SessionStart);
        assert!(!start.contains("first turn after"), "{start}");
        assert!(start.contains("as a session opens"), "{start}");
        // A `/resume` ends the conversation it replaces, as `/clear` does.
        assert!(
            event_description(HookEvent::SessionEnd).contains("/resume"),
            "{}",
            event_description(HookEvent::SessionEnd)
        );
        // The tool-call limit ends a turn on an error too.
        let failure = event_description(HookEvent::StopFailure);
        assert!(failure.contains("tool-call limit"), "{failure}");
        assert!(!event_summary(HookEvent::StopFailure).contains("backend"));
    }

    #[test]
    fn the_matcher_flag_mirrors_the_dispatchers_query() {
        // The three events whose dispatch passes no query run their groups
        // matcher-or-not (`llm/hooks.rs`; codex ignores the matcher on the
        // same three), so the menu never shows them a matcher level.
        for &event in HookEvent::ALL {
            let expected = !matches!(
                event,
                HookEvent::Stop | HookEvent::UserPromptSubmit | HookEvent::Interrupt
            );
            assert_eq!(
                event_has_matchers(event),
                expected,
                "{} matcher flag",
                event.name()
            );
        }
    }
}
