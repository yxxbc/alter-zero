# herdr — reporting the session's state to the pane it runs in

**Inside a [herdr](https://herdr.dev) pane, Alter Zero tells herdr what it is
doing: `working` while a turn runs, `blocked` while a question or a
permission prompt waits on you, `idle` when it is ready for the next
message — plus the command that reopens the conversation, and a release
when you quit.** herdr is a terminal multiplexer built for coding agents:
each agent runs in a real pane, and a sidebar rolls every pane up to
blocked / working / done / idle, with a toast and a sound when one finishes
or needs you. Without this, herdr has no idea what Alter Zero is: it
identifies agents by their process name against a closed, compiled-in list,
so an Alter Zero pane read as a plain shell — no state, no "finished"
notification, no `herdr agent wait`, nothing restored after a herdr restart.

Nothing to configure. The reports start on their own when the environment
says this process runs in a herdr pane, and do nothing anywhere else.

```
 herdr sidebar                      Alter Zero, in pane w1:p1
 ● alter-zero   working     ⟵       ⣤⣀⣀ Working… (12s · ↓ 1.2k tokens · esc to interrupt)
 ● alter-zero   blocked     ⟵       Bash command  cargo publish   ❯ 1. Yes  2. …  3. No
 ● alter-zero   done        ⟵       Worked for 41s
```

The pure half is `src/herdr.rs` (when a process reports, what it reports,
the request lines, and the one socket send); the boundary is
`src/tui/herdr.rs` (the loop-bottom tick, the failure hold, the release at
quit) and `workers::spawn_herdr_reporter`, the thread that does the I/O.

## Why self-reporting

herdr offers an agent three ways to be understood, and only one is open to
an agent herdr doesn't ship support for:

1. **Screen detection.** herdr reads the bottom of each pane against a TOML
   manifest per agent. It cannot learn a new agent this way: the agent list,
   the process-name matching and which manifests load are compiled into
   herdr (a local manifest only *patches* an agent herdr already knows), and
   the project does not take unsolicited pull requests.
2. **Hooks.** Claude Code's own herdr integration is a `SessionStart` hook,
   but copied as it is it would do nothing here: it reports only the
   session's id, which herdr ignores from any source but its own
   integrations, and Claude's *state* comes from herdr reading Claude's
   screen against a manifest Alter Zero has none of. A script of our own,
   calling `herdr pane report-agent` from Alter Zero's `hooks.json`, covers
   the happy path — `UserPromptSubmit` → working, `PermissionRequest` and a
   `PreToolUse` on `AskUserQuestion` → blocked, `Stop` → idle, `SessionEnd`
   → release — and misses the rest:
   - `Stop` fires only when a round completes, never on an Esc interrupt, a
     backend error, a `/compact` turn or a `!` command, so after any of
     those the pane reads `working` until some later turn ends;
   - nothing fires when a permission prompt is *answered*: `PostToolUse`
     comes only after the call succeeds, so an approved two-minute build
     reads `blocked` for two minutes, and a refused call stays `blocked`
     until the model's next call;
   - nothing fires at launch (`SessionStart` waits for the first turn), for
     a `!` command or for a summarization, and a hook process cannot know a
     background subagent is still running when the lead's `Stop` arrives;
   - the payload's `session_id` is not the id `--resume` takes (the rollout
     mints its own when its file is created), so the resume command would
     have to be parsed out of `transcript_path`;
   - hooks are off until a directory turns them on (`docs/hooks.md`), so
     every user installs a script and opts in per project, and each
     `PreToolUse`/`PostToolUse` spawns a process the turn waits on.
3. **Self-reporting** — herdr's documented path for a third-party agent
   ("Add Herdr support to your agent", herdr.dev/docs/add-herdr-support):
   the agent reports its own state, its resume command and its release
   over herdr's socket. For an agent label herdr does not recognise, these
   reports are the **only** source of state, never overridden by screen
   detection and never expiring, so they have to be right — which is why
   they are derived, not scripted.

## What is reported, and when

The state is **derived from the session**, not pushed from the places that
change it: `herdr::activity` reads `App` at the bottom of every loop
iteration (`Session::after_iteration`), `herdr::Reporter::observe` turns it
into a report, and a report goes out only when it **differs** from the last
one sent — the `tips.json` cursor's rule, so an ordinary iteration costs a
few field reads and a compare. Every transition the session makes, from any
of the places it can make it (a user turn, a queued follow-up, a `!`
command, `/compact` and auto-compaction, a background notice's follow-up
turn, an interrupt, `/clear`, a subagent settling), lands in the one
derivation, so none of them can be forgotten.

| herdr state | when | `message` |
|---|---|---|
| `blocked` | a permission prompt or an `AskUserQuestion` modal is open (`App::modal_open` — even with no turn running, since a background subagent can ask) | `Bash command: cargo publish`, `Edit file: main.rs`, `Question: Which database?` |
| `working` | a turn is in flight (`App::turn_active`: a user or queued turn, a `!` command, a summarization, a background follow-up) | — |
| `blocked` | the last turn ended on a **backend error** (retries exhausted), until the next turn starts or the conversation is replaced | the error, one line |
| `working` | a subagent is still running, or has queued follow-ups to run | — |
| `idle` | otherwise: ready for input | — |

Top to bottom is the priority. Four of those rows deserve a reason:

- **A failed turn is `blocked`, not `idle`.** `idle` after `working` is
  herdr's "finished" toast and sound; a turn that died on a rate limit or a
  rejected key did not finish, and the user has to act. This is the one fact
  the derivation cannot read off `App` — the red notice it leaves in history
  is the same role an interrupt's is — so the `StreamEvent::Error` arm hands
  it over (`Session::herdr_turn_failed` → `Reporter::fail`), stamped with the
  history generation. The hold lasts until a turn starts again — any turn, a
  queued one included — or until the generation moves: `/clear`, `/resume`
  and an Esc-Esc backtrack all replace or rewind the conversation, taking the
  failed turn off the screen, and the block goes with it, with no call site
  of its own to forget. An **interrupt** is not a failure: the user pressed
  Esc and is looking at the result.
- **A running subagent is `working`.** The lead is ready for input, but the
  work it was asked to do is not done: the subagent's result will come back
  as a follow-up turn. Reporting `idle` there would fire "finished", and
  `herdr agent wait` would return, while work is still in flight.
- **A background shell is not `working`.** A shell is a process, not the
  agent's work, and the ones worth backgrounding are the ones that never
  end — a dev server would pin the pane to `working` forever. A command
  whose exit wakes the model starts a follow-up turn, which reports itself.
- **A `/login` sign-in or an MCP OAuth wait is not `blocked`.** You opened
  those pages yourself and are looking at them; nothing the agent is doing
  waits on you.

`done` is not a state an agent reports: herdr shows `idle` as `done` until
you have looked at the pane.

### The resume command

Every report carries `resume_argv`, the command herdr types into the pane's
shell, in its directory, after a herdr **server restart** — so the pane
comes back as the conversation it was:

- `["alter-zero", "--resume", "{session id}"]` once the conversation has a
  rollout file (`docs/resume.md`), which restores its model too
  (`docs/session-model.md`);
- `["alter-zero"]` before the first message, and after `/clear` until the
  new conversation's first one — the pane comes back as a fresh session,
  which is what it was.

The program name is the one you ran (`cli::bin_name`, the exit hint's), and
`alter-zero` when that name is not a bare command herdr accepts (herdr types
the command into a shell, so it takes `[A-Za-z0-9_.-]+` and nothing else —
`herdr::plain_command`). A session id herdr would refuse is left out rather
than sent to be rejected.

### Release on quit

Quitting (`/quit`, Ctrl+C, a closed stdin) sends `pane.release_agent` as the
last thing `Session::shutdown` does, which clears Alter Zero's name, state
and resume command from the pane at once. The send runs on a thread of its
own and the quit waits at most `herdr::RELEASE_WAIT` for it — a wedged herdr
cannot hold the terminal hostage. A crash sends nothing; herdr notices the
pane is back at its shell prompt within a second or two and clears it
itself.

## The wire

herdr speaks newline-delimited JSON over a Unix socket: one request line per
connection, one response line back. `herdr::send` connects, writes the line,
waits for the first byte of the reply (or the close, or the timeout) and
hangs up — the write and the wait each bounded by `SEND_TIMEOUT` (500 ms),
every failure ignored, because a report herdr never received costs nothing
and a report that blocks the agent would cost a lot. The connect has no
timeout of its own (a local one fails at once on a dead socket and waits
only while herdr's accept backlog is full), which is why nothing calls
`send` on the event loop: reports go through the writer thread, and the
quit's release through a thread the quit waits on for at most
`RELEASE_WAIT`.

```json
{"id":"alter-zero:1759800000000000","method":"pane.report_agent","params":{"pane_id":"w1:p1","source":"alter-zero","agent":"alter-zero","state":"blocked","message":"Bash command: cargo publish","seq":1759800000000000,"resume_argv":["alter-zero","--resume","18a9f2c33d41e5b6-1a2b"]}}
{"id":"alter-zero:1759800000000001","method":"pane.release_agent","params":{"pane_id":"w1:p1","source":"alter-zero","agent":"alter-zero","seq":1759800000000001}}
```

- **`source` and `agent` are both `alter-zero`.** The agent is the name the
  sidebar shows; the source names the integration, and must not start with
  `herdr:`, which herdr reserves for its own.
- **`seq` increases on every request, across runs.** herdr keeps the last
  sequence it accepted per source per pane for as long as its server runs —
  past this process's exit — and silently ignores anything not above it.
  `herdr::next_seq` takes the larger of one more than the last and the wall
  clock in microseconds, so a relaunch in the same pane starts above
  whatever the previous run sent, and a clock that steps back cannot stall
  the reports.
- **Only the latest state is sent.** The worker drains its queue before each
  send and sends the newest line; the sequence numbers make dropping the
  older ones safe, and the release — sent from the quit path itself — always
  carries the highest number, so it wins even if a report is still in
  flight beside it.
- **Only a socket this user owns is written to** (`herdr::socket_acceptable`,
  an `lstat` with no symlink followed): a stale or hostile
  `HERDR_SOCKET_PATH` naming a regular file or someone else's socket gets
  nothing. herdr creates its socket readable by its owner only, so a
  same-user check is herdr's own trust model, the one PrimeIntellect's Prime
  Agent integration applies too.

The direct socket rather than `"$HERDR_BIN_PATH" pane report-agent …`:
the CLI is the portable choice herdr recommends, but it is a process spawn
per transition and two round trips per report (it pings for protocol
compatibility first), where the socket is one write. herdr's own Pi
extension and the third-party reporters it links to do the same.

## When it runs

`herdr::Pane::from_env` decides once, at startup, from what herdr exports
into every pane:

| variable | requirement |
|---|---|
| `HERDR_ENV` | exactly `1` |
| `HERDR_SOCKET_PATH` | non-empty |
| `HERDR_PANE_ID` | non-empty — `w1:p1` |
| `ALTER_ZERO_HERDR` | not `0`/`false`/`no`/`off` — the off switch |

Anything else and there is no reporter: no thread, no socket, no cost.
`ALTER_ZERO_HERDR=0` is the only off switch, and there is deliberately no
`/settings` row — outside herdr there is nothing to turn off, and inside it
herdr's own settings decide what a state change notifies.

### The shells it runs are not herdr's pane

Every command the model runs in a pseudo-terminal gets `ALTER_ZERO_HERDR=0`
(`pty::spawn::TERMINAL_ENV`, beside `TERM`): that terminal is Alter Zero's,
not herdr's, whatever the inherited `HERDR_*` variables say. Without it, an
Alter Zero started *inside* one of those shells — the model running
`cargo run` on this very repository — would report as the pane's agent and
release the pane on its way out, leaving the real session invisible to herdr
until its next state change.

The `HERDR_*` variables themselves are left alone, on purpose. herdr's own
agent skill (`herdr --skill`) checks `HERDR_ENV=1` and uses `$HERDR_PANE_ID`
to mean "the pane I am running in", so a model asked to split its pane or
start a second agent beside itself needs them. The cost of keeping them is
that another agent with herdr hooks installed (Codex, say), run by the model
inside a tool shell, can still report against this pane while it runs — the
same thing it does inside any other agent's shell in herdr.

The smoke suite unsets every `HERDR_*` variable (`smoke_sanitize_env`), so a
developer running it inside herdr never has a hundred test sessions report
against their pane.

## Living inside herdr

- **Ctrl+B is herdr's prefix key.** Moving a running command to the
  background takes **Ctrl+B Ctrl+B** inside herdr (the prefix pressed twice
  sends it through), or rebind herdr's prefix.
- **Pictures fall back to half-blocks.** herdr's panes speak the kitty
  graphics protocol, but they set `TERM=xterm-256color` and
  `TERM_PROGRAM=herdr` and drop `KITTY_WINDOW_ID`, so
  `images::store::ImageStore::detect` (`docs/images.md`) finds nothing it
  recognises. Whether the pictures reach the screen depends on the terminal
  herdr itself runs in, which nothing inside the pane can see — so it is
  opt-in: `ALTER_ZERO_IMAGE_PROTOCOL=kitty` when herdr runs in a kitty or
  Ghostty window.
- **Everything else carries through**: herdr honours synchronized output
  (mode 2026), bracketed paste, the kitty keyboard protocol (so Shift+Enter
  still makes a newline, `docs/shift-enter.md`), OSC 8 links and OSC 52
  `/copy` — the terminal features the inline viewport is built on.

## Known gaps

- **A herdr live handoff forgets the state.** When herdr hands its panes to a
  new server process without restarting them (an upgrade), it carries agent
  state only for its own integrations. Alter Zero's state reappears at its
  next transition; an idle pane shows no agent until then. A periodic
  re-report would close the gap at the cost of a timer waking an idle loop.
- **Another agent run inside a tool shell can still report against the
  pane.** Codex with herdr's hooks installed, say, run by the model in a
  tool shell, sees the pane's `HERDR_*` variables (left in place for herdr's
  own skill, above) and reports as the pane's agent until it exits; Alter
  Zero's own state returns at its next transition. An Alter Zero started
  there cannot, by `ALTER_ZERO_HERDR=0`.
- **Windows** is out of scope: herdr's socket is a named pipe there, and
  `herdr::send` speaks Unix sockets only.

## Tests

- `src/herdr.rs` — the environment gate, the state priority, the failure
  hold and its expiry with the conversation, the dedupe, the resume command
  and its fallbacks, the sequence, the request lines as JSON, the socket
  fence, and `send` against a real Unix listener (a reply, a silent peer, a
  regular file in the socket's place).
- `smoke.sh` Phase 132 — the real binary against a stub herdr socket: the
  `idle` claim at launch, `working` → `idle` around a turn with the session
  id riding the resume command, `blocked` with its message while a
  permission prompt waits, the release at quit, strictly increasing
  sequence numbers throughout, nothing at all under `ALTER_ZERO_HERDR=0`,
  and nothing written to a regular file in the socket's place.
- **Against herdr itself** (v0.9.3, protocol 22, by hand — herdr is not a
  CI dependency): a headless `herdr server`, a workspace, and Alter Zero
  launched in its pane. `herdr agent list` showed `alter-zero` `idle` at
  launch, `working` through a turn, `blocked` while a permission prompt was
  open and `idle` after; herdr's `session.json` held the reported
  `["alter-zero", "--resume", "{id}"]`; `/quit` emptied the agent list
  within 100 ms; and after `herdr server stop` and a fresh `herdr server`,
  the restored pane ran that command and came back as the same
  conversation, re-claiming the pane within a second. One thing to know when
  reproducing it in a deep directory: herdr's sockets must fit a Unix socket
  path's 108 bytes, so point `HERDR_SOCKET_PATH` somewhere short.
