#!/usr/bin/env bash
# Phase 132 — herdr pane-state reports

. "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
smoke_begin

# herdr — inside a herdr pane the session reports its own state over herdr's
# socket: idle when ready, working while a turn runs, blocked while a prompt
# waits on the user, the command that reopens the conversation, and a release
# at quit (docs/herdr.md). A Python stub stands in for herdr: it listens on a
# Unix socket, appends every request line it reads to a log, and answers each
# one the way herdr does. The launches set the three variables herdr exports
# into its panes (smoke_sanitize_env unset the developer's own).
S132="${S}_herdr"
HD_DIR="$(mktemp -d "$SMOKE_TMP/herdr.XXXXXX")"
HD_SOCK="$HD_DIR/herdr.sock"
HD_LOG="$HD_DIR/requests.log"
python3 - "$HD_SOCK" "$HD_LOG" <<'PY' &
import json, socket, sys

path, log = sys.argv[1], sys.argv[2]
server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(path)
server.listen(16)
while True:
    conn, _ = server.accept()
    with conn:
        data = b""
        while not data.endswith(b"\n"):
            chunk = conn.recv(65536)
            if not chunk:
                break
            data += chunk
        line = data.decode("utf-8", "replace").strip()
        if not line:
            continue
        with open(log, "a") as f:
            f.write(line + "\n")
        try:
            request_id = json.loads(line).get("id", "")
        except ValueError:
            request_id = ""
        reply = {"id": request_id, "result": {"type": "ok"}}
        conn.sendall((json.dumps(reply) + "\n").encode())
PY
HD_SERVER=$!
smoke_on_exit kill "$HD_SERVER"
poll 5 test -S "$HD_SOCK" || fail "the stub herdr socket never came up"

HD_PANE="HERDR_ENV=1 HERDR_SOCKET_PATH=$HD_SOCK HERDR_PANE_ID=w1:p1"
HD_APP="env $CFG_ENV_NOHIST ALTER_ZERO_STARTUP_DELAY_MS=$SMOKE_STARTUP_MS $HD_PANE $BIN"
hd_count() {
	if [ -f "$HD_LOG" ]; then wc -l <"$HD_LOG" | tr -d ' '; else echo 0; fi
}
# One row per request the stub received: `method state message resume…`,
# the message `-` when absent. Every request is also checked here for what
# never varies: one integration, one pane, a sequence number above the last.
hd_rows() {
	python3 - "$HD_LOG" <<'PY'
import json, sys

last = 0
for raw in open(sys.argv[1]):
    request = json.loads(raw)
    params = request["params"]
    problems = []
    if params.get("source") != "alter-zero" or params.get("agent") != "alter-zero":
        problems.append("not alter-zero's integration")
    if params.get("pane_id") != "w1:p1":
        problems.append("the wrong pane")
    if request.get("id") != f"alter-zero:{params.get('seq')}":
        problems.append("an id that is not the sequence number")
    seq = params.get("seq", 0)
    if seq <= last:
        problems.append(f"seq {seq} not above {last}")
    last = seq
    message = (params.get("message") or "-").replace(" ", "_")
    resume = " ".join(params.get("resume_argv") or [])
    flag = "BAD:" + ",".join(problems) if problems else "ok"
    print(request["method"], params.get("state", "-"), message, flag, resume)
PY
}
# The state of every report, in order, one line.
hd_states() { hd_rows | awk '$1 == "pane.report_agent" { printf "%s ", $2 }' | sed 's/ $//'; }
# Wait until the stub has logged a request whose row matches PATTERN.
hd_wait() { poll "$1" hd_has "$2"; }
hd_has() { [ -f "$HD_LOG" ] && hd_rows | grep -qE "$1"; }

# (a) The launch claims the pane: one idle report whose resume command is a
# fresh launch — there is no conversation to resume before the first message.
launch "$S132" 100 30 "$HD_APP"
if ! hd_wait 5 '^pane\.report_agent idle - ok alter-zero$'; then
	fail "no idle claim at launch with a bare 'alter-zero' resume command"
fi

# (b) A turn: working while it runs, idle once it ends — and from the first
# message on, the resume command names the conversation.
submit "$S132" "hello there"
hd_done_1="$(wait_summaries 20 "$S132" 1)" || fail "the first turn never settled"
dump "the first turn, settled" "$hd_done_1"
if ! hd_wait 5 '^pane\.report_agent idle - ok alter-zero --resume [0-9a-f]+-[0-9a-f]+$'; then
	fail "the end of the first turn was not reported idle with an --resume command"
fi

# (c) A permission prompt: blocked, saying what it asks; answered, the turn
# carries on and ends idle.
submit "$S132" "permission demo please"
hd_prompt="$(wait_pane 20 "$S132" -F "Do you want to create hello.py?")" ||
	fail "the permission prompt never opened"
dump "the permission prompt" "$hd_prompt"
if ! hd_wait 5 '^pane\.report_agent blocked Create_file:_hello\.py ok '; then
	fail "the open prompt was not reported blocked with 'Create file: hello.py'"
fi
keys "$S132" 1
hd_done_2="$(wait_summaries 20 "$S132" 2)" || fail "the answered turn never settled"
dump "the answered turn, settled" "$hd_done_2"
poll 5 test "$(hd_states)" = "idle working idle working blocked working idle" ||
	fail "the states went '$(hd_states)', not 'idle working idle working blocked working idle'"

# (d) Quitting releases the pane, last.
submit "$S132" "/quit"
wait_gone 10 "$S132" || fail "the app did not quit"
if ! hd_wait 5 '^pane\.release_agent - - ok'; then
	fail "the quit sent no release"
fi
note "every request the stub received"
hd_rows
if hd_rows | grep -q 'BAD:'; then
	fail "a request broke the contract (see the BAD: rows above)"
fi
expect_eq "$(hd_rows | tail -1 | cut -d' ' -f1)" "pane.release_agent" "the release was not the last request"

# (e) ALTER_ZERO_HERDR=0 is the off switch: a whole session sends nothing.
hd_before="$(hd_count)"
launch "$S132" 100 30 "env ALTER_ZERO_HERDR=0 $CFG_ENV_NOHIST ALTER_ZERO_STARTUP_DELAY_MS=$SMOKE_STARTUP_MS $HD_PANE $BIN"
submit "$S132" "hello again"
wait_summaries 20 "$S132" 1 >/dev/null || fail "the turn under the off switch never settled"
submit "$S132" "/quit"
wait_gone 10 "$S132" || fail "the app did not quit under the off switch"
sleep 0.5
expect_eq "$(hd_count)" "$hd_before" "ALTER_ZERO_HERDR=0 still reported to herdr"

# (f) A regular file where the socket should be is never written to, and the
# session runs as though herdr were absent.
HD_FILE="$HD_DIR/not-a-socket"
: >"$HD_FILE"
launch "$S132" 100 30 "env $CFG_ENV_NOHIST ALTER_ZERO_STARTUP_DELAY_MS=$SMOKE_STARTUP_MS HERDR_ENV=1 HERDR_SOCKET_PATH=$HD_FILE HERDR_PANE_ID=w1:p1 $BIN"
submit "$S132" "hello once more"
wait_summaries 20 "$S132" 1 >/dev/null || fail "the turn beside a non-socket never settled"
submit "$S132" "/quit"
wait_gone 10 "$S132" || fail "the app did not quit beside a non-socket"
expect_eq "$(wc -c <"$HD_FILE" | tr -d ' ')" "0" "a regular file in the socket's place was written to"
