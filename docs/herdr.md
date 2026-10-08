# herdr — reporting the agent's state to the multiplexer it runs in

**Run Alter Zero in a [herdr](https://herdr.dev) pane and the pane's sidebar
row says what the agent is doing: `working`, `blocked` on a question only you
can answer, or `idle` waiting for your next message. After a herdr server
restart the pane comes back running the same conversation.** All of it is
done through the lifecycle hooks (`docs/hooks.md`): one command installs a
hooks file, and the hooks engine runs a small reporter at each lifecycle
event, which tells herdr's socket what changed.

```
alter-zero integration install herdr     # write ~/.alter-zero/hooks.d/herdr.json
alter-zero integration status            # installed? inside herdr? which pane?
alter-zero integration uninstall herdr   # remove it
```

herdr is a terminal multiplexer for coding agents: one real PTY pane per
agent, workspaces and tabs, and a sidebar that rolls every agent up to
`blocked`, `working`, `done` or `idle`, with a notification when one finishes
or needs a decision. It knows the agents it ships support for by reading
their screens (a *manifest* of each one's prompts and status lines) or from
hooks their vendors install. It does not know Alter Zero, and it should not
have to: herdr documents a vendor route, *Add Herdr support to your agent*,
where the agent reports its own state over herdr's local socket. This is that
route, built on hooks.

## How herdr is told

Every process in a herdr pane inherits four variables:

| variable | meaning |
| --- | --- |
| `HERDR_ENV` | `1` inside herdr |
| `HERDR_PANE_ID` | the pane the agent runs in, e.g. `w1:p2` |
| `HERDR_SOCKET_PATH` | herdr's API socket |
| `HERDR_BIN_PATH` | the herdr binary that owns the pane |

The hooks engine passes the whole environment to every hook child
(`docs/hooks.md`), so the reporter finds them. Outside herdr — `HERDR_ENV`
unset — it does nothing, and the installed command line checks that in `sh`
before the reporter is even started, so a session outside herdr pays one
`sh` per event and nothing else.

Two requests, newline-delimited JSON over the Unix socket, each answered by
one line the reporter reads and ignores (the read is what keeps herdr from
seeing a half-closed connection; failures are swallowed — herdr must never
slow the agent down):

```json
{"id":"alter-zero:1759770000000000000","method":"pane.report_agent","params":{
  "pane_id":"w1:p2","source":"alter-zero","agent":"alter-zero","state":"working",
  "seq":1759770000000000000,
  "agent_session_id":"19a8f3c2d4e5f60-1a2b",
  "resume_argv":["alter-zero","--resume","19a8f3c2d4e5f60-1a2b"]}}
{"id":"alter-zero:1759770000000000001","method":"pane.release_agent","params":{
  "pane_id":"w1:p2","source":"alter-zero","agent":"alter-zero","seq":1759770000000000001}}
```

- `source` and `agent` are both `alter-zero`: herdr's rule is that a vendor
  integration names itself (never an agent herdr already supports) and never
  starts its source with `herdr:`.
- `state` is `working`, `blocked` or `idle`. herdr derives `done` itself (an
  `idle` pane whose tab you have not looked at since), and its notifications
  come from these transitions.
- `seq` is a nanosecond timestamp the reporter keeps **strictly increasing**
  (`max(now, last + 1)`, the last one stored beside the state below). herdr
  drops a report whose `seq` is not higher than the last it accepted, from
  this source, across restarts — which is what makes a late report harmless.
- `resume_argv` rides **every** report: `alter-zero --resume <id>` (beside
  `agent_session_id`) once the conversation has a rollout file — the
  payload's `transcript_path` is not null — and a bare `alter-zero` before
  that. A fresh session has no file until its first message is recorded
  (`docs/resume.md`, deferred create), and a resume command naming an id
  with no file behind it would fail on restore; but leaving the command
  off is worse than a fresh start, because herdr keeps the last one it was
  given — after a `/clear`, the conversation the user just left. The turn
  writes its rollout before its first hook runs, so even its first
  `working` carries the `--resume`. `resume_argv` is what herdr acts on: it
  runs it in the pane's directory after a server restart, and its rules are
  that the first word is a plain command on `PATH` (the binary's own file
  name, not its path), no argument holds an apostrophe or a control
  character, at most 64 arguments and 8 KiB — a report breaking them is
  refused whole, so the reporter falls back to the fresh start for an id
  herdr would refuse, and drops the command only when the binary's own
  name is one. `agent_session_id` names the conversation for herdr's
  records (herdr reads it only for the agents it ships support for; for a
  vendor source it is informational). `--resume` restores the
  conversation's own model (`docs/session-model.md`) and the directory's
  permission mode is per project already, so `alter-zero --resume <id>` is
  the whole command.
- `pane.release_agent` is sent once, when the session really ends — a quit
  (`SessionEnd` with `prompt_input_exit` or `other`). A `/clear` or a
  `/resume` replaces the conversation in the same process, which herdr asks
  to be *reported*, not released: the `SessionStart` that follows does that.

## Events to states

The reporter keeps two facts across events, because each event is a
separate process: whether the **lead's turn** is running, and the set of
**prompts waiting on the user** (by agent and tool call id). What it
reports is derived from them every time:

```
blocked   if any prompt is waiting
working   else if the lead's turn is running
idle      otherwise
```

| event | matcher | what it changes |
| --- | --- | --- |
| `SessionStart` | — | a new conversation: turn idle, no prompts waiting |
| `UserPromptSubmit` | — | the turn is running |
| `PreToolUse` | — | the lead's call → the turn is running (a background follow-up turn, which has no prompt of its own, starts here); an `askuserquestion` call (lead or subagent) → its question is waiting |
| `PostToolUse`, `PostToolUseFailure` | `askuserquestion` | the question was answered, declined or timed out |
| `PostToolUse` | `agentkill` | the lead stopped an agent: its prompts left the screen with it |
| `Notification` | `permission_prompt` | a permission prompt is on screen → waiting |
| `PermissionResult` | — | that prompt was answered → no longer waiting |
| `Stop`, `StopFailure` | — | the lead's turn ended (naturally / on an error), and anything of the lead's still waiting with it |
| `Interrupt` | — | the user interrupted the turn: it ended, and every prompt it raised went with it |
| `PreCompact`, `PostCompact` | — | a `/compact` summarization started / landed |
| `SessionEnd` | — | `prompt_input_exit` / `other` → release the pane; `clear` / `resume` → nothing (the next `SessionStart` reports) |

Three rules hold the table together:

- **Subagents never move the turn.** A payload carrying `agent_id` is a
  subagent's (`docs/hooks.md`), and a background agent keeps working long
  after the lead's turn ends — so its calls do not report `working`, or the
  pane would say *working* while the composer is waiting on you. Its
  **prompts** still count: a permission prompt a background agent raised is
  on your screen and needs you, whatever the lead is doing.
- **Prompts are a set, not a flag.** Parallel agents ask before any one is
  answered (`docs/permissions.md`): two prompts can be waiting at once, and
  answering the first must leave the pane `blocked` while the second is on
  screen. Keyed by agent **and** call id — a call id is unique only within
  one conversation, and Ollama numbers every round's calls from `call_0`,
  so the lead's prompt and a subagent's can share one — a prompt is added
  and removed exactly once, and the order two agents' hooks reach the
  reporter in does not matter. A prompt can also leave without an answer:
  the lead's turn ending takes whatever of the lead's was left, and the
  lead's `agentkill` takes the stopped agent's, which leaves the screen
  with it (`docs/permissions.md`).
- **The turn's end clears the turn.** `Stop`, `StopFailure` and `Interrupt`
  are the three ways a lead turn ends, and the reporter needs all three — the
  last two are exactly what codex's integration was missing before codex
  added `Interrupt` (openai/codex#22858: Esc fired nothing, so external
  status integrations stayed at *Running*). A `/clear` or a quit that cuts
  a turn short fires its `Interrupt` too, before the `SessionEnd`.
- **Only what happened is reported.** The integration's hooks run after
  the user's and the project's, and not for an event those vetoed
  (`docs/hooks.md` *Integrations*): a prompt a `UserPromptSubmit` hook
  refused never makes the pane *working*, a question a `PreToolUse` hook
  refused never makes it *blocked*, and a `Stop` a hook turned into another
  round never makes it *idle* while that round runs.

The state lives in one small JSON file per pane —
`~/.alter-zero/herdr/{hash of socket path + pane id}.json` (the config
home's, so `ALTER_ZERO_CONFIG_DIR` moves it) — locked for the whole
read-compute-send-write (`File::lock`): the lead's hooks and each
subagent's run on their own threads, so two reporters can run at once. A
`SessionStart` resets it, so a crashed session's stale state never leaks
into the next one, and the release deletes it.

## What the hooks engine needed

The engine was a faithful port of Claude Code's contract, and still was not
enough to drive herdr. Auditing it against herdr's own integrations — codex
(`SessionStart`, `UserPromptSubmit`, `Stop`, `Interrupt`), and the two that
report full lifecycle state through hooks alone, Kimi Code
(`PermissionRequest`/`PermissionResult`, `PostToolUseFailure`, `Interrupt`)
and MastraCode — found seven things, each fixed in the engine rather than
worked around in the reporter (`docs/hooks.md` has the details):

1. **`session_id` was not the conversation's.** Payloads carried an id
   minted at startup, while the rollout file minted a second one, and
   `--resume` resolves by the file's. A resume command built from the
   payload could never work, and the id never followed a `/clear` or a
   `/resume`. The recorder now holds the conversation's id from the start —
   the startup id for a fresh session's file, a fresh one on `/clear`, the
   adopted file's on resume — and publishes it to every payload, Claude
   Code's meaning of the field. `scratchpad_dir` joins the base payload, so
   a hook can still find the session's temp tree after the id moves.
2. **Nothing fired when a turn ended any way but naturally.** Esc fired
   nothing and neither did a backend error, so a status reporter stayed at
   `working`. `Interrupt` (codex, Kimi, MastraCode) and `StopFailure`
   (Claude Code, Kimi) close those two ends.
3. **Nothing said a human was being asked, or had answered.**
   `PermissionRequest` fires *before* auto mode's classifier and may be
   decided without anyone seeing a prompt, so it cannot mean *blocked*.
   `Notification` with `notification_type: "permission_prompt"` (Claude
   Code's name) fires the moment a prompt is actually on screen, and
   `PermissionResult` (Kimi, MastraCode) after every `PermissionRequest`
   once it is decided — by the user, a hook or the classifier.
   `PostToolUseFailure` (Claude Code, Kimi) covers a question that was
   declined or timed out, which is a failed `askuserquestion` call.
4. **`SessionStart` waited for the first prompt.** It was drained at the top
   of the next turn, so a freshly opened Alter Zero was invisible to herdr
   until you typed. It now fires **in the background** at the boundary it
   describes — launch, `/clear`, `/resume`, and the `/login` that first
   connects a model to a session — Claude Code's behaviour (*runs
   in the background at launch/resume, allowing immediate user input*), and
   the next turn waits for it before its first request, so its
   `additionalContext` still reaches that turn.
5. **Boundary events ran on the UI thread, or not at all.** `SessionEnd`
   blocked the loop for up to 2 s, and an `Interrupt` run there would freeze
   Esc. Events the boundary fires now go through one ordered **lane** off the
   UI thread, and every later hook dispatch waits for the lane first — so an
   `Interrupt` can never land after the `UserPromptSubmit` of the turn the
   user started right after it. The other direction holds too: an Esc's
   `Interrupt` (and a quit's `SessionEnd`) waits until the interrupted
   turn's own hooks have been reaped, so a reporter mid-flight cannot land
   a late *working* after the *idle*. A permission prompt's `Notification`
   rides the lane itself, under the turn's own token: an Esc on that prompt
   fires an `Interrupt` that queues behind it, so herdr can never hear
   *idle* and then a late *blocked* — while the Esc still reaps a reporter
   that hangs. A quit cancels every subagent before its `SessionEnd`, so a
   background agent's tool call can never report after the release and
   put the pane's agent back for a session that is gone.
6. **Hooks were off in every directory you had not opted into.** The
   `/settings` **Hooks** row is per directory and off by default
   (`docs/per-directory-state.md`) — right for `~/.alter-zero/hooks.json`,
   which holds the user's own guards and formatters, and wrong for an
   integration with the terminal, which belongs to every directory. So
   integrations live in their own always-on layer, `~/.alter-zero/hooks.d/`.
7. **An integration heard what another hook refused.** Handlers run in
   order and all of them run, so the reporter was told of a prompt a
   `UserPromptSubmit` hook had refused (the pane stuck *working* with no
   turn running), of a question a `PreToolUse` hook had refused (stuck
   *blocked*), and of a `Stop` a hook had turned into another round (*idle*
   while that round ran). The integration layer now observes: it runs
   after everything else and not for an event that was stopped.

Codex, by comparison, sees `blocked` by reading the screen (herdr's codex
manifest), runs `SessionStart` lazily, and leaves `working` standing after a
backend error. Alter Zero has no manifest in herdr, so everything herdr
knows comes from these reports — which is why the events above had to exist.

## `hooks.d/`

`{config_home}/hooks.d/*.json` — each file a whole `hooks.json`, union-merged
after the user's file and the trusted project file, in file-name order —
**runs in every directory, whatever the Hooks row says**. The row keeps
governing exactly what it always did (`hooks.json` and the project layer),
and `ALTER_ZERO_HOOKS=0` still turns every hook off for the run, these
included. A file there is written by an installer the user ran, so it
carries the trust of the config home it lives in, the same as `hooks.json`;
one that will not parse is a red startup toast naming it, and the others
still load.

`alter-zero integration install herdr` writes `hooks.d/herdr.json` whole
(through a temp file and a rename) and owns it: reinstalling overwrites it,
and `uninstall` deletes it. It never edits `hooks.json`. Each event's group
runs the same command:

```sh
[ "${HERDR_ENV:-}" = 1 ] || exit 0; exec "$ALTER_ZERO_BIN_PATH" integration hook herdr
```

`ALTER_ZERO_BIN_PATH` is set by the engine for every hook child — the
running binary, the way herdr's `HERDR_BIN_PATH` names its own — so the file
never goes stale when the binary moves or updates, and needs no absolute path
written into it. The reporter reads the event from the payload's
`hook_event_name`, so one command serves every event; the timeout is 10
seconds, herdr's own for its integrations.

`/hooks` lists both layers: the integration's entries labelled
`Integration`, their detail page naming `hooks.d/herdr.json`, and — where
the row is off in this directory — a note that the `hooks.json` entries are
not running this session, so an integration that runs is never read as one
that is off.

## The reporter

`alter-zero integration hook herdr` is the hook command. It reads one
payload from stdin (capped), and:

1. leaves at once — exit `0`, nothing printed — unless `HERDR_ENV=1`, the
   pane and socket variables are set, and `ALTER_ZERO_AGENT` is not (below);
2. locks the pane's state file and steps the state machine with the event
   (`herdr::step`, pure, the whole table above);
3. sends the request it produced, with 500 ms socket timeouts, and reads
   the one-line answer;
4. writes the state back — or, after a release, deletes it.

It never prints to stdout — a `SessionStart` or `UserPromptSubmit` hook's
plain stdout becomes model context (`docs/hooks.md`) — and always exits
`0`, an event or a name it does not know included: exit `2` from a `Stop`
hook would keep the turn going, and a failure to reach herdr is not
something the agent should hear about on every tool call.

**A session started from the agent's own terminal stays quiet.** Every
`bash` call runs in a pseudo-terminal that inherits the pane's environment
(`docs/interactive-shell.md`), so an Alter Zero the agent launches there —
a test of this very binary, say — would find `HERDR_PANE_ID` and report
over the session that owns the pane: herdr keys a report by pane and
source, and cannot tell two reporters with one source apart. So the tool
terminals carry `ALTER_ZERO_AGENT=1`, and the reporter treats it as *not
in a pane* — codex's `CODEX_THREAD_ID` guard, for the same reason.
`alter-zero integration status` says which of the three a terminal is: a
pane, a pane's agent terminal, or neither.

## Pictures in a pane

Separate from the hooks, and needing no install: a herdr pane draws Alter
Zero's pictures (a pasted screenshot, an image `read`) as kitty graphics.
herdr's emulator is libghostty, which speaks the kitty graphics protocol, but
a pane's environment names none of it — `TERM=xterm-256color`,
`TERM_PROGRAM=herdr`, and the outer terminal's `KITTY_WINDOW_ID` stripped — so
detection by environment alone fell to half-blocks there. Where the
environment names nothing, Alter Zero asks the terminal inside its startup
cursor query, and herdr answers `OK` and calls itself `libghostty`
(`docs/images.md` *Asking the terminal*). herdr's `kitty_graphics` setting has
been on by default since 0.9.0; with it off, herdr says nothing and the
pictures stay half-blocks. herdr then draws the picture on its own outer
terminal, so what an outer terminal without kitty graphics shows is herdr's
call; `terminal.kitty_graphics = false` in herdr's config, or
`ALTER_ZERO_IMAGE_PROTOCOL=halfblocks` in the pane, keeps half-blocks there.

## Known gaps

- **The offline demo fires no hooks** (`docs/hooks.md`), so neither does this:
  a session with no provider configured is invisible to herdr until
  `/login`.
- **A herdr that accepts and never answers slows the hooks.** A socket
  with nothing listening fails at once, and herdr accepts on a thread of
  its own, so the connect never waits; but a server whose request handling
  has wedged costs each report its 500 ms read timeout — herdr's own
  integrations allow the same 0.5 s — and the hooks run one after another,
  so each later hook waits that long too. Nothing the user sees waits on
  it — the prompt is on screen before its `Notification` runs — and the
  hook's 10 s timeout bounds the worst case.

## Testing

- The state machine, the request lines and the installed file are pure and
  unit-tested (`src/herdr.rs`): every row of the table, the prompt set under
  reordering, a call id the lead and two agents share, the lead's turn end
  and `agentkill` taking prompts with them, the subagent rule, `seq`
  monotonicity, the resume argv's validation and its fresh-start fallback,
  the release.
- The engine's new events have real-`sh` runner tests and `run_agent` /
  `approve_call` wiring tests beside the existing ones (`docs/hooks.md`).
- `tests/herdr_reporter.rs` runs the real binary's `integration hook herdr`
  against a stub Unix socket: payloads in, the exact requests out, the
  state file across invocations (a shared call id included).
- `scripts/smoke.sh` Phase 132 drives the whole loop: a stub
  chat-completions provider, a stub herdr socket, `integration install` into
  the phase's config home, and a session launched with herdr's variables —
  `idle` at launch, `working` then `idle` around a turn with the resume
  command attached (its id the one the quit hint prints), `blocked` while a
  permission prompt is open, `idle` after Esc, the release on quit, and the
  resume command herdr was given bringing the conversation back — then a
  second session beside a user hooks file: a refused prompt never
  reported, a Stop hook's extra round still *working*, and a `/clear` and a
  quit mid-turn each ending the turn (`Interrupt`) before the session.
