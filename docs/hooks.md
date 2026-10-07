# Lifecycle hooks

Claude Code and codex both let a user wedge their own command into the agent's
lifecycle: run a linter after every `edit`, refuse a `bash` command that
matches a pattern, feed the model extra context when a session opens. This is
that feature, ported to this codebase.

```
● Bash(rm -rf build/)
  ⎿  Blocked by hook: no destructive deletes outside ./tmp
```

The **`/hooks`** command opens a read-only inline browser over this
configuration — which events have hooks, under which matchers, running what —
see `docs/hooks-menu.md`.

## The contract

codex ships a `codex-rs/hooks` crate whose engine type is literally named
`ClaudeHooksEngine`: it implements **Claude Code's** hook wire contract rather
than a novel one. We target that same contract, so a hook script written for
either tool works here unchanged.

Config is a `hooks.json` whose shape is event → matcher groups → handlers:

```json
{
  "description": "optional, ignored",
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "bash|write",
        "hooks": [
          { "type": "command", "command": "./guard.sh", "timeout": 60 }
        ]
      }
    ]
  }
}
```

A handler receives its event payload as **JSON on stdin** and answers with
**JSON on stdout**. The two halves are spelled differently, and both
references agree on the asymmetry, so we copy it exactly:

- **stdin (the payload) is `snake_case`** — `session_id`, `transcript_path`,
  `cwd`, `permission_mode`, `hook_event_name`, `tool_name`, `tool_input`,
  `tool_use_id`, `agent_id`, `agent_type`, `scratchpad_dir`. `session_id` is
  the **conversation's** id — Claude Code's meaning of the field, and the id
  `--resume` takes, since it names the rollout file (`docs/resume.md`).
  It follows the conversation: a fresh one gets a new id on `/clear`, a
  `/resume` hands over the resumed file's, and `transcript_path` is `null`
  until the conversation's first message creates its file. It used to be an
  id minted at startup that the rollout never used, so a hook could not
  resume the session it was told about (`docs/herdr.md`). At launch the two
  are still the same id, and the session's temp tree keeps that first id
  for the whole run (`docs/scratchpad.md`), which is why the payload names
  that tree on its own: `scratchpad_dir`.
- **stdout (the verdict) is `camelCase`** — `continue`, `stopReason`,
  `suppressOutput`, `systemMessage`, `decision`, `reason`,
  `hookSpecificOutput.{hookEventName,permissionDecision,
  permissionDecisionReason,updatedInput,additionalContext}`.

Sixteen events are modelled. What each may return:

| event                           | the hook may return                                   |
| ------------------------------- | ----------------------------------------------------- |
| `PreToolUse`                    | allow / deny / ask · block + reason · `additionalContext` · `updatedInput` |
| `PermissionRequest`             | `allow` / `deny` + message                             |
| `PostToolUse`                   | block + reason · `additionalContext`                   |
| `PostToolUseFailure`            | block + reason · `additionalContext` (PostToolUse's answers, for a call that failed) |
| `UserPromptSubmit`              | block + reason · `additionalContext`                   |
| `Stop` / `SubagentStop`         | block + reason (make the agent keep going)             |
| `SessionStart` / `SubagentStart`| `additionalContext`                                    |
| `SessionEnd`                    | nothing — output ignored (both references)             |
| `PreCompact`                    | `additionalContext` / stdout → extra compact instructions — **no block** (see below) |
| `PostCompact`                   | nothing — envelope only                                |
| `Notification`                  | nothing — an observation (`notification_type` is the matcher) |
| `PermissionResult`              | nothing — an observation                               |
| `StopFailure`                   | nothing — output ignored (Claude Code's rule)          |
| `Interrupt`                     | nothing — output ignored                               |

The last four, and `PostToolUseFailure`, are what a **status tracker**
needs and the first eleven never said: when a turn ends some way other than
naturally, when a human is being asked, and when they answered. They were
added for the herdr integration (`docs/herdr.md`), each from a reference
that already defines it — `PostToolUseFailure`, `Notification` and
`StopFailure` from Claude Code, `Interrupt` from codex, `PermissionResult`
from Kimi Code — so a hook written for one of those tools reads the same
payload here.

**Exit codes** (Claude Code's semantics, verbatim): `0` — success, stdout is
read as a verdict when it starts with `{`, else treated as plain text. `2` —
**block**, and the reason is **stderr**, not stdout. Any other non-zero — a
non-blocking error, surfaced to the user and otherwise ignored. Stdout that
starts with `{` but fails to parse is a non-blocking error too, never a
silent allow.

**Merging.** When several handlers match one event: **any** block wins, the
**first** reason is kept, the contexts **concatenate** in handler order, and
for `PreToolUse` a `deny` outranks an `ask`, which outranks an `allow`.

Two payload fields are **extensions**, since the contract has no slot for
facts this app has: `tool_response` is `{"output": …, "success": …}` (our tools
return text, not a structured result, and a stable shape beats
sometimes-a-string), and `SubagentStop` carries `success`. That second one
exists because the obvious shortcut is wrong: `stop_hook_active` means *this
run was started by a `Stop` hook's block* — the loop guard — so reporting a
failed agent through it would hand a hook author the wrong fact under a
documented name. It is always `false` here, because nothing yet continues an
agent from a hook. codex extends these payloads the same way, with `turn_id`.

The new events add three more, for the same reason. A `permission_prompt`
`Notification` carries the call's `tool_name` and `tool_use_id` beside
Claude Code's `title`/`message`, so a tracker can pair the prompt with the
`PermissionResult` that closes it — two agents can be asking at once
(`docs/permissions.md`). `PermissionResult`'s `decision` is
`{"behavior": "allow" | "deny", "source": "user" | "hook" | "classifier"}`,
naming who decided. And `PostToolUseFailure`'s `is_interrupt` is always
`false`: an interrupted call is reported by the `Interrupt` event instead.

## Matchers

Both references share one implementation, and so do we
(`hooks::matcher::matches`):

- absent, empty, or `*` — matches everything.
- every character in `[A-Za-z0-9_|]` — the **fast path**: split on `|`, trim,
  and compare for **exact equality**. No regex is compiled. Note this means
  `bash` deliberately does *not* match a hypothetical `bashoutput`.
- anything else — compiled as a regex.

The original design proposed warn-and-skipping real regexes to avoid taking a
`regex` dependency. That turned out to be a cost that barely exists: `regex`
is a thin wrapper over the `regex-automata` engine that `fancy-regex` (a
direct dependency, for the status-line tally's split pattern —
docs/tokenizer.md) already pulls into every build. Since `^Bash$` is a common
matcher in real Claude Code configs, silently skipping it would be a footgun
with no offsetting saving, and we compile it instead. A matcher that fails to
compile is warned about and skipped.

**Tool names answer to both spellings.** The match query for the tool events
is this app's own tool name — `bash`, `read`, `write`, `edit`, `agent` —
lowercase, as the model sees it, and each also answers to its Claude Code
spelling as a **second exact name** (`Bash`, `Read`, `Write`, `Edit`, and
`Task` for `agent` — the reference's own legacy-alias precedent,
`permissionRuleParser.ts`). So `"matcher": "Bash"` — the single most common
hook config in the wild — and `^Bash$` both select `bash` here, while
`bashoutput` still matches neither: an alias is a second exact name, never a
prefix or case-folding rule. The bash companions (`docs/bash-tools.md`) have
no reference spelling to borrow, so each one's second name is the one its cell
shows — `bashsend` answers to `BashSend`, likewise `BashWait`, `BashKill`,
`BashList`, and the legacy `bash_session` to `BashSession` — and `Bash` never
selects them: it is an exact name. Agent types (`SubagentStart`/`SubagentStop`)
and the other non-tool queries are never aliased.

## Where each event attaches

| event                | site                                                            |
| -------------------- | --------------------------------------------------------------- |
| `PreToolUse`         | `llm/agent.rs` — before `approve`; **`agent` launches included** |
| `PermissionRequest`  | `llm/approval.rs` — after the standing rules                     |
| `Notification`       | `llm/approval.rs` — as the prompt opens (`permission_prompt`), on the lane |
| `PermissionResult`   | `llm/approval.rs` — once a request is decided: by a hook, the classifier or the user |
| `PostToolUse`        | `llm/agent.rs` — on `execute`'s return, **successes only**       |
| `PostToolUseFailure` | `llm/agent.rs` — on `execute`'s return, **failures only**        |
| `SubagentStart`      | `llm/backend.rs` — inside `spawn_subagent_run`                   |
| `SubagentStop`       | `llm/agent.rs` — `run_agent`'s Complete arm, via the tagged sink |
| `SessionStart`       | `tui` — on the lane at launch, `/clear`, `/resume`, a `/login`; the spawn top waits for it |
| `SessionEnd`         | `tui` — `/clear`, `/resume` and `shutdown`, on the lane, under a 2 s event budget |
| `UserPromptSubmit`   | `llm/backend.rs` — the spawn top, after `SessionStart`           |
| `Stop`               | `llm/agent.rs` — `run_agent`'s Complete arm                      |
| `StopFailure`        | `llm/agent.rs` — a failed round, before its `Error` event       |
| `Interrupt`          | `tui` — an Esc that stops an agent turn, on the lane             |
| `PreCompact`         | the compact spawn's top, via the `CompactHooks` wrapper          |
| `PostCompact`        | the compact spawn's completion, via the same wrapper             |

Every one of the sixteen fires, and none runs on the tokio loop: the
backend's events run on its own threads, and the boundary's on **the lane**
— both halves of the story below.

`PermissionRequest` sits exactly where the auto-mode classifier does — after
the standing allowlist, before the user — because it answers the same
question: *may this run without asking?* Two consequences worth stating: a
call a standing rule already covers never reaches it (nothing is being asked),
and neither does any call when the permission gate is off entirely
(`ALTER_ZERO_PERMISSIONS=0`), since there is no permission decision to make.
`PreToolUse` still fires in both cases — it is about the call, not about
asking.

### One execution context — the premise that dissolved

The original phasing assumed the six "loop-path" events had to run on the
tokio current-thread loop, where a blocking subprocess freezes the spinner
and the keyboard — and that attaching them therefore needed a pending-turn
state machine the codebase doesn't have. Verifying the references killed
that premise:

- **Claude Code fires its stop hooks inside the query loop**, not at the
  REPL layer — a `Stop` block *is* the next iteration of that loop
  (`query.ts:1282-1305`), which is exactly `run_agent` here.
- **Neither reference blocks the first paint on `SessionStart`.** Claude
  Code kicks it off without awaiting before render and blocks only the
  *first API call* (`main.tsx:3761-3765`); codex queues a pending source and
  drains it at the top of the next turn (`turn.rs:239-241`) — the
  checkpoint probe's lesson, honoured by both references before we ever hit
  it. This engine began with codex's drain and has since moved to Claude
  Code's timing (below): a session tracker must hear of a session when it
  opens, not at its first prompt (`docs/herdr.md`).

So every event the agent fires lands on a **backend thread**, where
blocking is already normal and correct — the permission gate parks there on
a `Condvar`, `llm::exec` polls its children every 20 ms — and the
loop-thread problem never existed. The few the **boundary** fires (the
session's start and end, an Esc) run on an ordered lane of their own,
described after the list:

- **`Stop`/`SubagentStop`** fire in `run_agent` at `RoundOutcome::Complete`,
  before `StreamDone`. A block appends the reply-so-far as an assistant
  message and the feedback (`Stop hook feedback:\n{reason}`, Claude Code's
  exact wording) as the next user message, flips `stop_hook_active`, and the
  **same turn runs another round** — so the status line keeps working, the
  queue and token tally are untouched, and `dispatch_after_turn`'s
  turn-end checkpoint lands *after* every continuation: a formatter hook's
  writes are inside the turn's snapshot and an Esc-Esc backtrack restores
  them. By construction it never fires for a `/compact` turn (that backend
  carries the wrapper below), a `!` shell (no agent loop), a backend error
  (that is `StopFailure`'s, below), or an interrupt (Claude Code returns
  before stop hooks on an abort — Esc always breaks a continuation chain,
  and fires `Interrupt` instead). The engine never refuses a re-block:
  `stop_hook_active` is the hook's own guard, the reference's exact posture.
  The tagged sink routes a subagent's completion to `SubagentStop`
  (matcher = agent type) with the same block-to-continue semantics on the
  agent's own loop; a killed or errored agent fires nothing.
- **`SessionStart`** fires **at the boundary it describes**, in the
  background on the lane: right after the first frame paints (`startup`,
  or `resume` for a `--continue`/`--resume` boot), on `/clear` (`clear`),
  on a `/resume` load (`resume`) — and, for a session that ran the offline
  demo until a `/login`, at that switch (matcher = source). Its context
  waits in `HookHandles::held`: the next spawn waits for the lane before
  round 1 and takes it, so it still lands as user messages after the
  prompt and is recorded (see `HookNote` below). A context the next
  conversation must not read — a `/clear` or `/resume` that came first — is
  dropped with the conversation it was for.
- **`UserPromptSubmit`** fires right after `SessionStart`, before the first
  request. A **block** streams `StreamEvent::PromptBlocked` instead of
  anything else: the loop rolls the just-recorded submission back out of
  history (the recorder's shrink-rewrite erases it from the rollout — the
  reference's "erased from context", so nothing a secrets-filter censored
  can reach a later turn), returns the text to the composer, purge-repaints
  the echo away, and commits a red reason-only notice. `additionalContext`
  injects even on a block — both references' rule. A loop-initiated turn (a
  background completion's follow-up) is marked synthetic and skips the
  event: its prompt is not the user's. The backend consults its retained
  request prefix only past this check, so a block costs no prompt-cache
  prefix (`docs/prompt-caching.md`).
- **`PreCompact`/`PostCompact`** ride the summarization spawn through the
  `CompactHooks` wrapper: its "prompt" hook is PreCompact (context/stdout →
  extra summarization instructions; **it cannot block — in either
  reference**: Claude Code's own in-app docs claim exit 2 blocks compaction,
  but the `blocked` field is never read at any PreCompact call site, and
  codex has no exit-2 arm for it), and its completion is PostCompact. The
  wrapper never drains the session-source queue and never continues a
  summarization; its notes stay out of the conversation record (they are
  ephemeral, like the compact chunks themselves).
- **`SessionEnd`** fires on `/clear` (`clear`), on a `/resume` load
  (`resume` — naming the conversation being left, before the recorder
  adopts the next) and at quit (`prompt_input_exit`), under a **2 s
  whole-event budget** — Claude Code caps this event at 1.5 s and codex
  clamps it to 3 s, because quitting must never hang on a hook. Output is
  ignored, as in both references. A quit drops whatever the lane still
  holds and reaps its running job first, so a slow `SessionStart` cannot
  spend the budget.
- **`Interrupt`** (codex's event) fires when an Esc stops an agent turn —
  whether it kept what streamed or undid the submission — on the lane,
  under the same 2 s budget, output ignored. A `!` shell turn fires nothing
  (it never told the hooks it started). Without it a status tracker saw a
  turn start and never end: Esc fired nothing at all — the bug codex fixed
  by adding this event (openai/codex#22858).
- **`StopFailure`** (Claude Code's) fires when a round fails — the
  provider's error once the retries are spent, or the tool-call ceiling —
  with the error's **type** as the matcher: `rate_limit` (429),
  `overloaded` (529), `authentication_failed` (401, 403), `billing_error`
  (402), `model_not_found` (404), `invalid_request` (any other 4xx),
  `server_error` (5xx, a failed connection) or `unknown` (anything else, the
  ceiling included), beside `error_details` and `last_assistant_message`.
  It runs before the `Error` event is sent, so its hooks finish before the
  user can start the next turn; it is the lead's only (a subagent's failed
  round is its own business, like `Stop`), and never an interrupt's.
- **`PostToolUseFailure`** (Claude Code's) is `PostToolUse` for a call that
  ran and failed — a command's non-zero exit, a refused edit, a question the
  user declined or let time out — with the same answers, `error` holding
  the failure text. A background launch fires neither.
- **`Notification` and `PermissionResult`** say a human is being asked, and
  has answered. `PermissionRequest` cannot: it fires before auto mode's
  classifier and may be decided without anyone seeing a prompt. So the
  prompt opening fires `Notification` (`permission_prompt`, Claude Code's
  type, titled `Permission needed` over `Alter Zero needs your permission to
  use Bash`), and every decided request fires `PermissionResult` (Kimi
  Code's event) naming who decided it — the user, a `PermissionRequest`
  hook, or the classifier. A standing rule decides nothing, so it raises
  neither; a request reaped by Esc gets no result (the `Interrupt` that
  follows is the record). The `Notification` is queued on the lane under
  the **turn's** token rather than run inline: the gate waits at once, the
  `Interrupt` of an Esc on that very prompt can never reach a hook before
  it — a tracker would read *idle*, then *blocked*, and show a prompt
  nobody can answer — and that Esc still reaps a notifier that hangs.
- **The boundary's events need a real backend**, which keeps the pairs
  honest: the offline demo fires no hooks, so a dummy-only session fires no
  `SessionStart`, and so no `SessionEnd` or `Interrupt` either — a
  session-logging hook never sees an end without its start. A `/login`
  mid-session makes the session real: its queued `startup` fires at the
  switch, and the end fires at quit.

### The lane

The boundary fires its events from the UI thread, which must never wait on
a hook — and they race the backend's: an Esc's `Interrupt` and the
`UserPromptSubmit` of the turn the user starts right after it are fired a
keystroke apart from two threads, and a tracker that hears them in the
wrong order ends on *idle* while the agent works. `llm::hooks::HookLane` is
the answer: the boundary's events — `SessionStart`, `SessionEnd`,
`Interrupt`, and the prompt's `Notification` — are jobs on one FIFO lane,
each run off the caller's thread in submission order, and **every hook the
backend dispatches first waits for the lane to drain** (polling its turn's
token, so an Esc still frees it). Each job's payload is resolved when it is
**fired**, not when it runs (`CommandHooks::pinned`): a `/resume` fires
`SessionEnd` and adopts the next conversation in the same breath, and the
event must still name the conversation that ended.

The order has to hold the other way too: an event that **ends** a turn must
not be overtaken by that turn's own hooks. An Esc cancels the turn, and the
runner reaps its running hook within a 20 ms poll — but until then the hook
can still write, and a status reporter mid-flight would land its *working*
after the `Interrupt`'s *idle*. So every backend dispatch registers its
turn's token while it runs (`llm::hooks::InFlight`), and the `Interrupt` and
`SessionEnd` jobs first wait (`REAP_WAIT`, 1 s at most) until no running
dispatch belongs to a cancelled turn. A quit cancels a running turn before
it fires `SessionEnd`, so the same wait covers it; a background agent's
hooks, whose turn nothing ended, are never waited for. The prompt's
`Notification` gets the guarantee from the lane itself: it is a job, so the
`Interrupt` queues behind it.

**A blocking hook must poll the `CancelToken`.** The reason the existing
`Condvar` park is safe is that `PermissionGate::wait` takes a cancellation
predicate and `run_bash` re-checks cancellation every 20 ms. The hook runner
polls on the same 20 ms cadence — the payload write included, on its own
thread, since a pipe-buffer-filling payload fed to a handler that never
reads stdin used to park an inline write past both Esc and the timeout —
and kills the process group on cancel, so Esc stays prompt.

## How a verdict reaches the screen

The single most useful finding of the audit was that this codebase already had
somewhere for every hook verdict to land, under another name. **No new
`StreamEvent` variant was added**, and none was needed:

- **A block** is `Approval::Reject { display, result }` — the permission
  gate's own two-text rejection. It already renders as a red cell, already
  keeps the longer model-facing text on `ToolCall::context_output`, already
  round-trips through the rollout, and already shows correctly in Ctrl+D. A
  hook block is spelled `⎿ Blocked by hook: {reason}` and the model reads the
  stop-and-wait instruction.
- **A `PreToolUse` allow** is `Approval::AllowNoted { note }` — the auto-mode
  classifier's existing "this ran without you" provenance row.
- **`additionalContext`** rides `StreamEvent::ToolNote` for the dim `⎿` row
  the user sees, and is appended to the **model-facing** result through the
  existing `ToolAnswered { display, result }` split, so the cell stays clean
  while `context_output` carries what the model actually read. Both are
  recorded, both survive a `/resume`.

Two additions were needed for the conversation-level events, because nothing
existing could express them:

- **`StreamEvent::HookNote { label, text }`** carries hook-injected
  conversation text — a Stop block's feedback, a SessionStart/
  UserPromptSubmit hook's additional context. The loop finalises the
  assistant run before it (invariant 4's flush-before-you-interleave) and
  records a cell-less **`HistoryItem::HookNote`**: invisible inline (Claude
  Code hides these from its normal view too — `isMeta`), expanded in the
  Ctrl+O transcript under its dim `● {label}` heading, replayed **verbatim**
  by `context_messages` as the user-role message the model actually read,
  and round-tripped through the rollout so a `/resume` keeps it. Context
  wire text uses Claude Code's exact shape — the
  `<system-reminder>`-wrapped `{event} hook additional context: …`.
- **`StreamEvent::PromptBlocked { reason }`** is a `UserPromptSubmit`
  block's terminal event — sent instead of `StreamDone`, nothing follows
  it; the loop's arm owns the rollback described above.

The one change that *was* needed: `ToolAnswered` and `ToolRejected` gained a
`truncated` flag. They had none because their only users — the ask tool and a
permission refusal — never produce capped output. Routing an **executed**
call's resolution through them does, so without it a hook-amended `bash`
whose output hit `TOOL_OUTPUT_MAX_BYTES` silently lost the `…` marker its
expanded cell appends. `false` at every site where nothing ran.

**Injected context appends at the frontier, never at the front.** The obvious
design — reuse the `user_instructions` slot — is wrong twice over. That slot
is owned by the project-doc feature and is rewritten wholesale at turn start;
and `context_messages_with` places it *first*, where implicit-caching
providers key the prompt prefix, so front-injection would invalidate the
prompt cache every single turn. Hook context therefore lands on the tool call
that produced it: cache-stable, `/resume`-safe, and — being an append —
needing no `history_generation` bump.

## A worked config

`~/.alter-zero/hooks.json`, doing the two things people ask for most — guard a
command, and lint after a file change:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "bash",
        "hooks": [
          {
            "type": "command",
            "command": "jq -re '.tool_input.command | test(\"rm -rf\") | not' >/dev/null || { echo 'no recursive deletes' >&2; exit 2; }",
            "timeout": 10
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "write|edit",
        "hooks": [
          {
            "type": "command",
            "command": "path=$(jq -r '.tool_input.path'); case \"$path\" in *.rs) cargo fmt -- \"$path\" >/dev/null 2>&1;; esac",
            "timeout": 60
          }
        ]
      }
    ]
  }
}
```

The first refuses through the shell-native route — exit `2`, reason on stderr.
The second answers with JSON when it wants to say something and stays silent
otherwise; a hook that prints nothing and exits `0` has approved by saying
nothing, which is the common case and costs one process.

To hand the model a fact instead of a refusal, print the verdict object:

```bash
printf '%s' '{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"the build id is ZX-4417"}}'
```

## Configuration and trust

**Two layers: the user's file, plus the project's behind a trust gate.**
`~/.alter-zero/hooks.json` (`ALTER_ZERO_HOOKS_FILE` overrides that user
path; `ALTER_ZERO_HOOKS` gates the feature off entirely, the
`ALTER_ZERO_PERMISSIONS` pattern) — and, since `docs/project-config.md`, the
project's own `{root}/.alter-zero/hooks.json`, discovered at the
nearest-`.git` root and **union-merged** after the user file
(`HooksFile::merged` — one more entry from the loader, no precedence rules to
invent: `select`'s existing dedup-by-command keeps the user's copy of a
duplicate, any block wins, contexts concatenate). The reason it was
user-level-only for so long is the reason the project layer is
**default-deny**: a project-local hooks file without a trust gate means
cloning a repository earns arbitrary code execution on the first tool call.
The gate is codex's content-hash answer with the interaction design solved
by `/trust` — the project file is read and fingerprinted at bootstrap but
contributes **nothing** to the merge until the user approves it in the
`/trust` review menu, and an edited file is untrusted again. See
`docs/project-config.md` for the whole gate (the `trust.json` store, the
review menu, live activation, `ALTER_ZERO_PROJECT_CONFIG`).

A malformed `hooks.json` is **not** silently treated as "no hooks" — that is
how a user comes to believe a guard is running when it is not. The parse
returns an error, the boundary raises it as a red startup toast, and the
session continues with hooks off.

The child is spawned through the same **tty-detach tier walk** every other
shell child uses (`docs/tty-detach.md`), so a hook that tries to prompt on
`/dev/tty` fails fast instead of hijacking the TUI. It inherits the session's
cwd and gets `ALTER_ZERO_PROJECT_DIR` — plus `CLAUDE_PROJECT_DIR`, so scripts
written for Claude Code find their project root unchanged — and
`ALTER_ZERO_BIN_PATH`, the running binary, the way herdr names its own in
`HERDR_BIN_PATH`: a hook that calls back into Alter Zero
(`"$ALTER_ZERO_BIN_PATH" integration hook herdr`) needs no absolute path
written into a file that would go stale when the binary moves or updates.

The `/settings` menu carries a **Hooks** row. It is a plain on/off toggle,
which is all a cycling menu with no free-text field can ever offer, and it
reports `false (unavailable)` when no config file resolved. It is **off by
default** and **per working directory** (`docs/per-directory-state.md`): a
hooks file written for one project should not fire in every project, so a
directory opts in — one cycle records `"hooks": true` in its `settings.json`
entry. `ALTER_ZERO_HOOKS` seeds the row for a run (`=1` on, a falsy value
off — the `ALTER_ZERO_TOOLS` pattern) and is never saved.

**A third layer runs everywhere: `{config_home}/hooks.d/`.** The row is
right for `hooks.json`, which holds a user's guards and formatters, and
wrong for an **integration** — a hook that connects Alter Zero to the
terminal it runs in belongs to every directory, and one that waits for a
per-directory opt-in reports nothing from the next project the user opens.
So each `hooks.d/*.json` (a whole `hooks.json` apiece, written by an
installer such as `alter-zero integration install herdr`, `docs/herdr.md`)
is union-merged after the user's file and the trusted project file, in
file-name order, and runs **whatever the row says**. Only
`ALTER_ZERO_HOOKS=0` turns it off with the rest. It needs no trust gate:
it lives in the config home, beside the `hooks.json` that is trusted for
the same reason. A file that will not parse is a red startup toast naming
it, and the others still load. `/hooks` labels its entries `Integration`
and names their file on the detail page (`docs/hooks-menu.md`).

## Module layout

Following the pure-core / boundary split the rest of the crate uses:

| where               | what                                                                | tested by  |
| ------------------- | -------------------------------------------------------------------- | ---------- |
| `src/hooks/` (pure) | config parse · matcher · payload builders · verdict parse · merge      | unit, TDD  |
| `src/subprocess.rs` | `spawn_shell_with` — the tier walk with caller-chosen stdio            | unit + real-`sh` |
| `src/llm/hooks.rs`  | the runner and the `HookSink` glue: payload → spawn → parse → merge    | real-`sh` unit tests + `smoke.sh` |
| `src/tui/config.rs` | locating and loading `hooks.json`                                      | existing   |

The pure half takes a hook's captured stdout **as a string**, so every verdict
and merge rule is unit-testable from fixtures with no process anywhere — the
same split `session`/`history` use for their JSONL formats. Only the spawn is
boundary code.

### The seam: one trait, not sixteen closures

`run_agent` already takes nine parameters under an
`#[allow(clippy::too_many_arguments)]`. Adding a closure per hook event is how
this feature rots into sixteen hand-wired call sites and an unreviewable
signature. Instead there is one trait object with defaulted no-op methods:

```rust
pub trait HookSink: Send + Sync + Debug {
    fn pre_tool_use(&self, …) -> PreToolVerdict { PreToolVerdict::default() }
    fn post_tool_use(&self, …) -> PostToolVerdict { PostToolVerdict::default() }
    fn post_tool_use_failure(&self, …) -> PostToolVerdict { PostToolVerdict::default() }
    fn permission_request(&self, …) -> Option<HookPermissionVerdict> { None }
    fn permission_prompt(&self, _call: &ToolCallRequest, _cancel: &CancelToken) {}
    fn permission_result(&self, …, _source: PermissionSource, _cancel: &CancelToken) {}
    fn subagent_start(&self, …) -> Vec<String> { Vec::new() }
    fn stop(&self, _active: bool, _last: &str, _cancel: &CancelToken) -> Option<String> { None }
    fn stop_failure(&self, _error: &str, _details: &str, _cancel: &CancelToken) {}
    fn session_start(&self, _cancel: &CancelToken) -> Vec<String> { Vec::new() }
    fn user_prompt_submit(&self, _prompt: &str, _cancel: &CancelToken) -> PromptVerdict { … }
    fn session_end(&self, _reason: &str) {}
    fn interrupt(&self) {}
    fn for_subagent(&self, …) -> Option<Arc<dyn HookSink>> { None }
    fn prompt_hook_label(&self) -> &'static str { "UserPromptSubmit" }
}
```

Every blocking method takes the turn's `CancelToken` — the trait says one
thing about blocking, and the two exceptions (`session_end` and `interrupt`,
which run past cancellation under their own 2 s budget) say so at the
method. `stop` routes itself: the lead sink fires `Stop`, a
`for_subagent`-tagged one fires `SubagentStop` — Claude Code picks the event
the same way, by the presence of an agent id. The boundary does not fire
through the trait: it owns the concrete `CommandHooks`, whose `fire_*`
methods put the boundary's events on the lane.

Every method defaults, so `NoHooks` costs nothing and every existing test
compiles untouched. **Adding an event is one defaulted method plus one call
site** — not a wider signature and not a new subsystem; the five the herdr
integration needed were exactly that, each. That property is the whole
point of the design; if a later change breaks it, the change is wrong.

## Deliberately not doing

- **`mcp_tool`, `prompt` and `agent` handler types.** codex parses them; only
  `command` is implemented there either. Ours parse and are skipped with a
  warning rather than failing the file.
- **Async (fire-and-forget) handlers.** Every v1 hook is synchronous. The
  `"async": true` flag parses and is ignored, with a warning. The
  bounded-concurrency pool codex uses is a phase-two concern.
- **Output spilling.** codex writes an oversized `additionalContext` to disk
  with a preview. We cap and truncate (`HOOK_OUTPUT_MAX_BYTES`) instead until
  someone needs more.
- **Editing hooks from the UI.** `/hooks` browses them read-only
  (`docs/hooks-menu.md`); the files are the configuration.

## Offline coverage

`scripts/smoke.sh` unsets every `*_API_KEY` before launching, so the boundary
suite always runs on the dummy backend, and `src/tui/` gets no unit tests by
policy. A hook runner would therefore have zero automated coverage unless a
dummy scenario drives it — so one exists. A prompt mentioning **`hook`** plays
a scripted turn with both halves of the contract:

```
● Bash(rm -rf build/)
  ⎿  Blocked by hook: no destructive deletes outside ./tmp

● Bash(ls -la src)
  ⎿  total 8
     drwxr-xr-x  app
     drwxr-xr-x  ui
  ⎿  Context added by hook
```

Its refusal text comes from `llm::hooks::block_texts` — the function the live
runner calls — so the offline cell is byte-for-byte the one a real `hooks.json`
produces, the rule every other scripted demo follows with the real executor's
formatters. **`smoke.sh` Phase 72** drives it and asserts the blocked call
produced no output of its own, the allowed one kept its output under the
provenance row, the model-facing paragraph never reached scrollback, the
Ctrl+O transcript kept the refusal, and the demo's closing Stop-hook
feedback note stayed cell-less inline while the transcript recorded it.
**Phase 73** drives the `prompt-block` scenario (cue: `hook` + `block my
prompt`) through the real loop arm: the blocked submission leaves scrollback
(exactly the composer's restored copy of the text remains) under the red
reason-only notice.

Three layers cover the rest:

- the **pure core** is unit-tested from fixtures with no process anywhere;
- the **runner** has real-`sh` tests beside `llm::exec`'s — a hook reading the
  payload off stdin and blocking on what it finds, a timeout being reaped, a
  cancelled turn reaping a running hook, a missing command failing open —
  and the lane's: jobs in submission order, a dispatch never overtaking an
  `Interrupt`, an `Interrupt` never landing ahead of the prompt it closed
  (while the turn's Esc still reaps that prompt's hung notifier), a quit
  that reaps a slow `SessionStart` before its `SessionEnd`, and a payload
  naming the conversation it was fired for;
- the **wiring** has `run_agent` and `approve_call` tests over fake sinks —
  `PostToolUseFailure` for a failed call only, `StopFailure` classified and
  sent before the error, `Notification` then `PermissionResult` around a
  prompt and neither for a standing rule — and three
  `#[ignore]`d **live** tests in `tests/live_openrouter.rs` that put a real
  model through a real `hooks.json`: a `PreToolUse` block (the command never
  runs), a `PostToolUse` `additionalContext` (the model reads the injected
  fact and repeats it), and an `updatedInput` rewrite (the hook's command is
  what executes).

The boundary's own events need a real backend, so their smoke coverage
runs against a stub provider: **Phase 132** installs the herdr integration
and reads the reporter's requests off a stub herdr socket — `SessionStart`
at launch, a turn's `UserPromptSubmit` and `Stop`, a prompt's
`Notification`, an Esc's `Interrupt` and the quit's `SessionEnd`, in that
order, each carrying the conversation's resumable id (`docs/herdr.md`).

## Deliberate deviations from the references

Verified against both sources, and chosen — not accidental:

- **Exit 2 beats a JSON verdict here and in codex; Claude Code parses the
  JSON first** (`hooks.ts:2533` returns before the exit-2 arm at `:2648`),
  so a script that prints a valid verdict *and* exits 2 reads differently
  there. Scripts that do both are ambiguous everywhere; we follow codex.
- **`{`-prefixed stdout that fails to parse is a loud warning** here and in
  codex; Claude Code swallows it silently (`hooks.ts:447-450`) and treats
  the output as plain text. Loud beats silent for a guard.
- **Exit 2 with empty stderr still blocks** (with a stand-in reason) — the
  Claude Code behaviour (`'No stderr output'`); codex fails open there.
- **Handlers run sequentially**, not in parallel (both references
  parallelize one event's handlers). Deterministic order is what keeps the
  merge's "first reason wins" meaningful — Claude Code's parallel version
  has a lost-deny-reason race (`hooks.ts:2862-2867`) we decline to copy.
  The cost: N pathological handlers stack N timeouts.
- **`tool_response` is a stable `{"output": …, "success": …}` object.** The
  references send the raw tool result (a bare string for codex's bash, an
  arbitrary object for Claude Code), which is sometimes-a-string; ours is
  one shape a script can always index.
- **`stop_hook_active` is advisory, engine-side too** — verified as Claude
  Code's exact posture (no code refuses a re-block; `maxTurns` is its only
  backstop). Esc is ours: an interrupt never fires Stop, so it always
  breaks a continuation chain.
- **A handler carrying Claude Code's `if` pre-filter is skipped with a
  warning** rather than run *wider than the author scoped it* — we don't
  evaluate permission-rule expressions. `shell: "powershell"` is skipped
  the same way; `shell: "bash"` runs under `sh`.
- **Oversized output is capped in memory at 64 KiB** as it is read, with a
  truncation marker — codex spills to a temp file with a preview instead
  (~2.5k-token default); nobody has needed the spill here yet.

## Known gaps

- **`"async": true` runs synchronously**, with a warning. So does a
  `statusMessage`, which is parsed and not displayed.
- **The rest of Claude Code's twenty-seven events are unmodelled**, and
  `Notification` fires only its `permission_prompt` type — a question from
  `askuserquestion` is a tool call, which `PreToolUse` already announces. A
  config naming an unmodelled event loads fine and never fires.
- **No event follows a prompt another hook refused.** When one
  `UserPromptSubmit` handler blocks the prompt, its siblings have already
  run — a tracker one of them told *working* stays there until the next
  turn. Neither reference fires anything after a refused prompt either.
- **A PermissionRequest hook that abstains loses its warnings** — the
  fail-open posture has no channel to the user from that position; a
  deciding verdict carries them on its note.
- **The project layer is read once, at bootstrap** — like the user file. A
  `.alter-zero/hooks.json` that appears (or changes) mid-session is a
  restart away; `/trust`'s approval activates exactly the reviewed startup
  snapshot (`docs/project-config.md`).
