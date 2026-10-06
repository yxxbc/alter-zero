# herdr — the pane's sidebar row says what the session is doing

**Inside a [herdr](https://herdr.dev) pane, alter-zero tells herdr whether it
is working, waiting on you, or ready — natively, over herdr's own socket, with
nothing to install and nothing to configure.** herdr is a terminal
multiplexer for coding agents: every pane gets a sidebar row whose state —
**working**, **blocked**, **done**, **idle** — is the answer to the one
question someone running five agents keeps asking (*which of them needs me?*),
and herdr notifies on the transitions. It knows the agents it ships support
for by their screens; an agent it does not know shows up as a plain terminal.
alter-zero is not on that list, so it reports for itself, the way herdr's
[*Add Herdr support to your agent*](https://herdr.dev/docs/add-herdr-support/)
guide describes — the route herdr documents for an agent that wants exact
state rather than screen-scraped guesses.

The pure half is `src/herdr.rs` — whether to report, what the state is, when
a report is due, the requests, the socket client and the worker thread — and
the boundary is `src/tui/herdr.rs`, which reads the environment once and calls
it at three moments. Outside herdr none of it runs: no thread, no socket, no
file.

## When it reports, and what

herdr gives every process it starts in a pane `HERDR_ENV=1`, the pane's id
(`HERDR_PANE_ID`, `w1:p2`) and its API socket (`HERDR_SOCKET_PATH`). Its own
rule is to integrate only when all three are set, and `herdr::Pane::from_env`
is exactly that rule plus two ways to stay quiet (*Turning it off*, *Nested
sessions*).

| state | while | the message |
|---|---|---|
| `working` | a turn is in flight — a reply, a `!` command, a `/compact` — **or a background agent is still running** | — |
| `blocked` | a permission prompt or an `AskUserQuestion` modal waits on the user — the main turn's or a background agent's | the prompt's title and subject: `Bash command: cargo test`, `Create file: src/new.rs`, `Question: Which database…?` |
| `idle` | none of the above | — |

`done` is herdr's, not ours: an `idle` the user has not looked at since the
pane was last `working` or `blocked`. Reporting `idle` at a turn's end is what
makes herdr mark the pane done, play its sound and raise its toast — so the
app never sends a notification of its own.

The derivation is `herdr::status_of`, over `App`'s own predicates —
`permission()` and `ask()` (the two halves of `App::modal_open`, the one
definition of "a thread is parked on the user"), `turn_active()`, and the
agent roster. Blocked outranks working, as in herdr's own Pi integration.

**A background agent keeps the pane working.** When the lead's turn ends with
agents still running, the work the user asked for has not: each agent's
completion starts a follow-up turn on its own (`docs/agent-tool.md`).
Reporting `idle` in between would have herdr call the pane done — sound and
all — while the agent was still going, then do it again at the real end; and
an orchestrator's `herdr agent wait` would return early. A background
**shell** does not count: it may be a dev server that runs for the rest of the
day, and a pane that never leaves `working` says nothing.

### When — the loop bottom, on change

`Session::herdr_tick` runs last in `after_iteration`, the loop-bottom
bookkeeping every iteration ends with. By then every way a turn can start or
end has happened in that iteration — a queued turn chained after the last one
ended, an auto-compaction begun, an abandoned prompt released, the rollout
(and so its id) materialized by the recorder's sync — so the reading is
settled, and a turn that ends and hands over to the next queued one in the same
iteration is `working` throughout instead of flickering through `idle` (which
herdr would announce as done). `herdr::Tracker` turns the reading into a report
only when the state, the message or the session changed: an ordinary iteration
— thirty a second while a turn animates — costs a few compares and no I/O. The
same tick runs once more at the end of `bootstrap`, so herdr learns at launch
that the pane runs alter-zero (`idle`, or `working` when a `[PROMPT]` started
the first turn).

A turn start is reported in the iteration it starts in. That matters beyond
looks: `herdr agent prompt --wait` gives an agent five seconds after the text
is submitted to report `working` or `blocked` before it gives up on it.

## The wire

Verified against herdr's source (`src/api/server.rs`,
`src/api/schema/panes.rs`, `src/terminal/state.rs`) as of 0.9.3 — the
fields used here have been stable since 0.6:

- **One request per connection**, newline-delimited JSON over the Unix socket:
  connect, write one line, read one line, close. herdr reads a request up to
  its `\n` and drops one that ends without it; it answers each with
  `{"id":…,"result":{"type":"ok"}}` or an `error`. `herdr::send` bounds the
  write and the read with `IO_TIMEOUT` (500 ms) each — a local socket answers
  in milliseconds, so the bound only matters for a wedged server.
- **The request** (`herdr::report_line`), with a string `id` (herdr rejects a
  number):

  ```json
  {"id":"alter-zero:1791244800123456789","method":"pane.report_agent","params":{"pane_id":"w1:p2","source":"alter-zero","agent":"alter-zero","state":"blocked","message":"Bash command: cargo test","seq":1791244800123456789,"resume_argv":["alter-zero","--resume","18a9f2c33d41e5b6-1a2b"]}}
  ```

  `source` namespaces reporters (several can coexist in one pane) and stays
  clear of the `herdr:` prefix herdr keeps for its own integrations — it also
  passes herdr's strictest source rule (ASCII letters, digits, `:._-`, ≤ 80),
  pinned by a test. `message` rides only a `blocked` report, one line of at
  most 80 characters (`herdr::MESSAGE_MAX_CHARS`, the cap herdr applies to
  display text elsewhere): the first line of a multi-line command closed by
  ` …`, a long one cut with `…`.
- **`seq`** — herdr keeps, per pane and per source, the highest number it has
  accepted, for as long as the pane lives, and ignores anything not strictly
  higher; an unsequenced report is dropped once a sequenced one was accepted.
  So every request carries one, and it is a clock: nanoseconds since the epoch
  (`herdr::SeqClock`, herdr's own hooks' scale), bumped by one when the clock
  has not moved on. A clock is what lets a session relaunched in the same pane
  start above the one before it — and the scale, once used, must never shrink.
- **`pane.release_agent`** (`herdr::release_line`) — `pane_id`, `source`,
  `agent`, `seq` — clears the pane's agent name, state and resume command. herdr
  honours it only when the source and agent match the pane's current reporter.

### The worker

`herdr::Reporter` owns a thread and a channel, so the event loop never waits
on herdr. `report` queues and returns; the worker sends in order, every
request numbered by its one `SeqClock`. Several reports queued behind a slow
send collapse to the newest — herdr's guide: *"Send only the latest state. If
several changes queue up while a report is in flight, drop the older ones"* —
a report identical to the last one delivered is skipped, and failures are
dropped: herdr gone, its socket stale, a pane it no longer knows, all cost
nothing and say nothing. The release is the last thing the worker sends; it
supersedes any report still queued.

## The resume command

herdr can bring an agent back after its own server restarts: it reopens the
pane in the same directory and types the command the agent reported into a
fresh shell there (herdr ≥ 0.9.2; older servers ignore the field). Every
report carries `resume_argv` once the conversation has a rollout —
`alter-zero --resume {id}`, the exit hint's own command (`docs/cli.md`) — so a
restored pane reopens the conversation, on the model it was using
(`docs/session-model.md`). A fresh session's first reports carry none: the
rollout, and its id, exist only once something is recorded. A `/clear` or a
`/resume` changes the id, and the next report carries the new command.

Two rules decide whether a command is sent at all:

- **herdr's own check, mirrored** (`herdr::valid_resume_argv`): at most 64
  arguments and 8 KiB, no control characters or apostrophes, and a first word
  that is a plain command name, not a path. A command herdr would refuse costs
  the whole report, so one is never sent.
- **The shell must be able to find it** (`herdr::resume_bin`): the name is the
  one this binary was invoked as (`cli::bin_name` — a renamed install resumes
  under its own name), and only when an absolute `PATH` entry holds an
  executable by that name. A binary run from a checkout
  (`./target/release/alter-zero`) gets no resume command rather than one that
  restores as `command not found`. herdr types the command into a fresh shell
  in the pane, which has never heard of a path relative to here.

## Quitting

The quit's last act (`Session::release_herdr`, at the end of `shutdown`) is
the release, waited on for at most `herdr::RELEASE_WAIT` (1 s — a report in
flight plus the release, each bounded by `IO_TIMEOUT`; normally a few
milliseconds), so the process does not exit before it has gone out. herdr's
guide asks for exactly this: release when the user quits, never when one
session replaces another in the same process (a `/clear` or `/resume` reports
the new session instead).

A loop that bails out on an I/O error never reaches `shutdown`; the reporter
releases on **drop** too, so the pane is handed back either way. A process
killed outright (the pane closed, `SIGKILL`) sends nothing — herdr's own
safety net covers it: once the pane is back at its idle shell prompt it clears
the agent and its resume command within a second or two.

## Nested sessions

Every shell child of a reporting session inherits the pane's `HERDR_*`
variables — herdr's agent skill needs them, so they are not stripped. An
alter-zero started from one of them (the `bash` tool running it in its own
terminal, say) would report for the same pane, from the same source: its
reports would overwrite the parent's, and its release on quit would clear the
parent's state until the parent's next transition — which, the parent being
mid-tool-call, is the end of the turn, whose `idle` then has no `working` to
complete and raises no done.

So a reporting session marks its children: `herdr::start` records the pane,
and the two spawn paths every model and user command goes through — the
detach tier walk (`subprocess`: the `!` command, the hooks, the `bash` tool on
a pipe) and the `bash` tool's own pseudo-terminal (`pty::spawn`) — call
`herdr::stamp_child`, setting `ALTER_ZERO_HERDR_REPORTER={pane}`. A session
that finds the marker naming its own pane stays quiet. codex guards its
nested sessions the same way (`CODEX_THREAD_ID`). The marker is an
environment variable on the child commands rather than on this process
because the crate forbids `unsafe`, which `std::env::set_var` is in edition
2024.

## Turning it off

| how | scope |
|---|---|
| `ALTER_ZERO_HERDR=0` (`false`/`no`/`off`) | this run — the session stays a plain terminal to herdr |
| outside herdr | always — nothing is spawned |

There is no `/settings` row: outside herdr the integration costs nothing, and
inside it the sidebar row is the point of running there.

## What this is not

- **Not hooks.** The lifecycle hooks (`docs/hooks.md`) could shell out to
  `herdr pane report-agent` on `UserPromptSubmit`/`Stop`, and that is how
  herdr wires agents it cannot change. Native reporting needs no process per
  transition, no setup, and sees what no hook event does — a permission prompt
  opening and closing, a background agent still running after `Stop`.
- **Not the herdr CLI.** `herdr pane report-agent` makes two round trips (a
  protocol `ping`, then the request), has no default timeout, and refuses to
  run when its binary's protocol differs from the server's (after
  `herdr update`, before the server restarts). The socket API has none of
  that, and herdr documents it alongside the CLI.
- **Not metadata.** `pane.report_metadata` can set a display name, a title and
  sidebar tokens; the default sidebar shows the agent label already, and the
  tokens render only in rows a user configures. Not worth a request yet.
- **Not Windows.** herdr's socket is a named pipe there; `herdr::send` answers
  `Unsupported` and the worker drops it like any other failure.

## Testing

- **Unit** (`src/herdr.rs`): the activation rules (all three variables, `1`
  exactly, the off switch's grammar, the nested marker naming this pane or
  another), the derivation from real `App` states (a turn, a `!` command, a
  compaction, a background agent running and finished, a main-turn prompt and
  a background agent's with no turn, a question), the messages (display paths,
  MCP names, session input, the first-line and length cuts), the tracker's
  change rule and resume command, the clock, herdr's resume validator, the
  `PATH` rule, the exact request JSON, and the transport and worker against a
  real Unix socket — delivery, a missing socket failing at once, a silent
  server costing at most the timeout, the order and a strictly increasing
  `seq`, the release on drop, a bounded release wait, coalescing, and the
  release superseding a queued report (the last two deterministic: the stub
  answers only when the test hands it a token).
- **Integration** (`tests/herdr_children.rs`, its own binary because marking
  a process is process-wide): the marker on a `!`-style detached shell, a
  terminal session, and a real `bash` tool call through the executor.
- **Smoke** (Phase 132): a Python stub serving herdr's wire on a Unix socket,
  the real binary launched with the three variables. One launch runs a turn and
  a `!` command and quits — `idle` (no resume command yet), `working`, `idle`
  with `alter-zero --resume {id}` for a rollout that exists, and the release,
  last; the `!` command prints the marker. A second runs a permission prompt —
  exactly `idle working blocked working idle release`, the blocked report
  naming the file. Two more launches — `ALTER_ZERO_HERDR=0`, and the marker
  set — send nothing. Every request is checked for the pane, the source, the
  agent, a string id and a strictly increasing `seq`. The suite unsets every
  `HERDR_*` variable for all the other phases, so running it from a herdr pane
  never reports into the developer's own herdr.
