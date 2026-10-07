//! The sixteen lifecycle events a hook can attach to, and their wire names.
//!
//! The names are Claude Code's `HOOK_EVENTS` spellings and are also the keys
//! of a `hooks.json` `"hooks"` object, so they are `PascalCase` strings rather
//! than anything derived — a rename here silently stops matching real config
//! files, which is why they are written out.

/// One point in the agent's lifecycle a hook may run at (`docs/hooks.md`).
///
/// Claude Code's spellings throughout, plus the two a session tracker needs
/// that it lacks — codex's `Interrupt` and Kimi Code's `PermissionResult` —
/// under the names those tools ship them as (`docs/herdr.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HookEvent {
    /// Before a tool call runs — may allow, deny, ask, or rewrite its input.
    PreToolUse,
    /// After a tool call ran — may block, or feed the model extra context.
    PostToolUse,
    /// After a tool call ran and **failed** — the same answers as
    /// `PostToolUse`, which fires only for a success.
    PostToolUseFailure,
    /// Instead of asking the user to approve a call — may allow or deny.
    PermissionRequest,
    /// A `PermissionRequest` was decided — by the user, a hook or the
    /// classifier. Observation only.
    PermissionResult,
    /// The app wants the user's attention — `permission_prompt`: a prompt is
    /// on screen now. Observation only.
    Notification,
    /// The user submitted a prompt — may block it, or add context.
    UserPromptSubmit,
    /// A session opened.
    SessionStart,
    /// A session is closing.
    SessionEnd,
    /// The model stopped answering — may make it keep going.
    Stop,
    /// The turn ended on a backend error instead. Observation only.
    StopFailure,
    /// The user interrupted the running turn. Observation only.
    Interrupt,
    /// A subagent was launched.
    SubagentStart,
    /// A subagent finished — may make it keep going.
    SubagentStop,
    /// Before a `/compact` summarization turn.
    PreCompact,
    /// After one landed.
    PostCompact,
}

impl HookEvent {
    /// Every event, in the order [`crate::hooks::HooksFile`] lists them.
    pub const ALL: &'static [Self] = &[
        Self::PreToolUse,
        Self::PostToolUse,
        Self::PostToolUseFailure,
        Self::PermissionRequest,
        Self::PermissionResult,
        Self::Notification,
        Self::UserPromptSubmit,
        Self::SessionStart,
        Self::SessionEnd,
        Self::Stop,
        Self::StopFailure,
        Self::Interrupt,
        Self::SubagentStart,
        Self::SubagentStop,
        Self::PreCompact,
        Self::PostCompact,
    ];

    /// The wire name: the `hooks.json` key, and the payload's
    /// `hook_event_name`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::PermissionRequest => "PermissionRequest",
            Self::PermissionResult => "PermissionResult",
            Self::Notification => "Notification",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::Stop => "Stop",
            Self::StopFailure => "StopFailure",
            Self::Interrupt => "Interrupt",
            Self::SubagentStart => "SubagentStart",
            Self::SubagentStop => "SubagentStop",
            Self::PreCompact => "PreCompact",
            Self::PostCompact => "PostCompact",
        }
    }

    /// The event a `hooks.json` key names, or `None` for one we don't model
    /// (Claude Code has thirty-odd; a config mentioning `FileChanged` must
    /// load fine and simply never fire it).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|e| e.name() == name)
    }

    /// Whether this event's match query is a **tool name** — the events whose
    /// matchers also honour the Claude Code spellings of our tools
    /// ([`crate::hooks::matcher::claude_code_alias`]). The rest match on other
    /// facts (an agent type, a source, a trigger) that have no alias to
    /// borrow.
    #[must_use]
    pub const fn matches_tool_names(self) -> bool {
        matches!(
            self,
            Self::PreToolUse
                | Self::PostToolUse
                | Self::PostToolUseFailure
                | Self::PermissionRequest
                | Self::PermissionResult
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_event_round_trips_through_its_wire_name() {
        for &event in HookEvent::ALL {
            assert_eq!(HookEvent::from_name(event.name()), Some(event));
        }
    }

    #[test]
    fn an_event_we_do_not_model_is_not_an_error_just_unknown() {
        assert_eq!(HookEvent::from_name("FileChanged"), None);
        assert_eq!(HookEvent::from_name("pretooluse"), None);
    }

    #[test]
    fn all_lists_every_variant_exactly_once() {
        let mut names: Vec<&str> = HookEvent::ALL.iter().map(|e| e.name()).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "ALL has a duplicate");
        assert_eq!(total, 16);
    }

    #[test]
    fn the_status_events_a_session_tracker_needs_are_modelled_by_their_reference_names() {
        // herdr's full-lifecycle integrations (docs/herdr.md) key on these
        // spellings: codex's and Kimi's `Interrupt`, Kimi's
        // `PermissionResult`, Claude Code's `StopFailure`,
        // `PostToolUseFailure` and `Notification`. A rename here silently
        // stops a shared config from ever firing.
        for name in [
            "Interrupt",
            "StopFailure",
            "PostToolUseFailure",
            "PermissionResult",
            "Notification",
        ] {
            assert!(HookEvent::from_name(name).is_some(), "{name} is unmodelled");
        }
    }

    #[test]
    fn a_tool_events_query_is_a_tool_name_and_answers_to_its_aliases() {
        for event in [
            HookEvent::PreToolUse,
            HookEvent::PostToolUse,
            HookEvent::PostToolUseFailure,
            HookEvent::PermissionRequest,
            HookEvent::PermissionResult,
        ] {
            assert!(event.matches_tool_names(), "{}", event.name());
        }
        for event in [
            HookEvent::Notification,
            HookEvent::StopFailure,
            HookEvent::Interrupt,
            HookEvent::Stop,
        ] {
            assert!(!event.matches_tool_names(), "{}", event.name());
        }
    }
}
