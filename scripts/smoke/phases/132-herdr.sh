#!/usr/bin/env bash
# Phase 132 — inside a herdr pane the session reports working, blocked and idle, then releases the pane

. "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
smoke_begin

# herdr support (docs/herdr.md). A local Python stub stands in for herdr's
# API socket: one newline-terminated JSON request per connection, appended to
# a log and answered `ok`, the way herdr's server does. Each launch carries
# the three variables herdr gives every pane — HERDR_ENV=1, HERDR_PANE_ID and
# HERDR_SOCKET_PATH pointed at the stub — and nothing else changes: no hooks,
# no setup. Every request must name the pane, the `alter-zero` source and
# agent, and a seq above the one before it.
#
# Launch 1 — a turn, then a `!` command, then the quit: idle at launch with no
#   resume command (nothing recorded yet), working, idle carrying
#   `alter-zero --resume {id}` (a PATH entry here holds an `alter-zero`, as an
#   install's would) for the rollout the session recorded — and the release,
#   last. The `!` command proves shell children carry the nested-session
#   marker.
# Launch 2 — a permission prompt: idle, working, blocked (its message naming
#   the file), working once answered, idle, release.
# Launches 3 and 4 — the off switch, and a session nested under one already
#   reporting for the pane: not one request.
S132="${S}_herdr"
HD_DIR="$(mktemp -d "$SMOKE_TMP/herdr.XXXXXX")"
HD_SOCK="$HD_DIR/herdr.sock"
HD_LOG="$HD_DIR/requests.log"
: >"$HD_LOG"
python3 -I - "$HD_SOCK" "$HD_LOG" <<'PY' &
import json, socket, sys, threading

path, log = sys.argv[1], sys.argv[2]
server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(path)
server.listen(16)
lock = threading.Lock()

def serve(conn):
    with conn:
        data = b""
        while not data.endswith(b"\n"):
            chunk = conn.recv(4096)
            if not chunk:
                return
            data += chunk
        line = data.decode()
        with lock, open(log, "a") as f:
            f.write(line)
        request = json.loads(line)
        answer = {"id": request.get("id"), "result": {"type": "ok"}}
        conn.sendall((json.dumps(answer) + "\n").encode())

while True:
    conn, _ = server.accept()
    threading.Thread(target=serve, args=(conn,), daemon=True).start()
PY
HD_PID=$!
smoke_on_exit "kill $HD_PID"
poll 5 test -S "$HD_SOCK" || fail "the stub herdr socket never came up"

# An `alter-zero` on PATH, the way an install puts one there: herdr types the
# resume command into a fresh shell, so without one the reports carry none.
HD_BIN="$HD_DIR/bin"
mkdir -p "$HD_BIN"
ln -s "$BIN_ABS" "$HD_BIN/alter-zero"
HD_PANE="HERDR_ENV=1 HERDR_PANE_ID=w1:p1 HERDR_SOCKET_PATH=$HD_SOCK PATH=$HD_BIN:$PATH"
hd_app() { # STARTUP_DELAY_MS [EXTRA_ENV…] → the launch string
	local delay="$1"
	shift
	printf 'env %s ALTER_ZERO_STARTUP_DELAY_MS=%s %s %s %s' "$CFG_ENV_NOHIST" "$delay" "$HD_PANE" "$*" "$BIN_ABS"
}

# The log as one `method state [message] resume` row per request, then a
# PROBLEMS line: what every request must hold — the pane, the source, the
# agent, a string id, a seq strictly above the last.
hd_rows() {
	python3 -I - "$HD_LOG" <<'PY'
import json, sys

rows, problems, last_seq = [], [], 0
for n, line in enumerate(open(sys.argv[1]), 1):
    request = json.loads(line)
    params = request["params"]
    if not isinstance(request["id"], str):
        problems.append(f"request {n}: the id is not a string")
    for key, want in (("pane_id", "w1:p1"), ("source", "alter-zero"), ("agent", "alter-zero")):
        if params.get(key) != want:
            problems.append(f"request {n}: {key} = {params.get(key)!r}")
    seq = params.get("seq")
    if not isinstance(seq, int) or seq <= last_seq:
        problems.append(f"request {n}: seq {seq!r} is not above {last_seq}")
    else:
        last_seq = seq
    resume = " ".join(params["resume_argv"]) if "resume_argv" in params else "-"
    rows.append(f'{request["method"]} {params.get("state", "-")} [{params.get("message", "")}] {resume}')
print("\n".join(rows))
print("PROBLEMS: " + ("; ".join(problems) if problems else "none"))
PY
}
# hd_states — the requests in order as one line: `idle working … release`.
hd_states() {
	hd_rows | sed -n -E 's/^pane\.report_agent ([a-z]+) .*/\1/p; s/^pane\.release_agent .*/release/p' | tr '\n' ' ' | sed 's/ $//'
}
# hd_has SECONDS PATTERN — wait until a request row matches (grep -E).
hd_has() {
	local secs="$1" pattern="$2" n i
	n="$(smoke_ticks "$secs")"
	for ((i = 0; i < n; i++)); do
		if hd_rows | grep -qE "$pattern"; then return 0; fi
		sleep "$SMOKE_POLL_INTERVAL"
	done
	return 1
}

# ---- launch 1: a turn, a `!` command, the quit ----
launch "$S132" 100 40 "$(hd_app "$SMOKE_STARTUP_MS")"
hd_has 5 '^pane\.report_agent idle \[\] -$' ||
	fail "the launch never reported idle (nothing recorded yet, so no resume command)"
submit "$S132" "$USER_MSG"
hd_has 10 '^pane\.report_agent working ' || fail "the turn never reported working"
wait_for 20 "$S132" -F "$SETTLED_REPLY" || fail "the turn never settled"
hd_has 10 '^pane\.report_agent idle \[\] alter-zero --resume [0-9a-f]+-[0-9a-f]+$' ||
	fail "the settled turn never reported idle with its resume command"
resume_id="$(hd_rows | sed -n -E 's/^pane\.report_agent idle \[\] alter-zero --resume ([0-9a-f]+-[0-9a-f]+)$/\1/p' | head -1)"
if [ -n "$resume_id" ]; then
	find "$SMOKE_SESSIONS" -name "rollout-*-$resume_id.jsonl" | grep -q . ||
		fail "the resume command's id $resume_id names no rollout the session recorded"
fi
submit "$S132" '!printf "marker=%s\n" "${ALTER_ZERO_HERDR_REPORTER-unset}"'
marker="$(wait_pane 10 "$S132" -E "^ *⎿ *marker=")" || fail "the ! command never printed"
dump "the ! command's output" "$marker"
expect_has "$marker" -F "marker=w1:p1" \
	"a shell child of a reporting session does not carry ALTER_ZERO_HERDR_REPORTER=w1:p1"
wait_settled 5 "$S132" -F "marker=" >/dev/null
tmux send-keys -t "$S132" C-c
wait_gone 5 "$S132" || fail "launch 1 did not quit on Ctrl+C"
rows="$(hd_rows)"
dump "launch 1's requests" "$rows"
expect_has "$rows" -F "PROBLEMS: none" "launch 1: a request broke the protocol"
states="$(hd_states)"
expect_eq "${states%% *}" "idle" "launch 1: the first report was not idle"
expect_eq "${states##* }" "release" "launch 1: the release was not the last request"
expect_eq "$(grep -o release <<<"$states" | wc -l | tr -d ' ')" "1" \
	"launch 1: the quit did not release the pane exactly once"

# ---- launch 2: a permission prompt blocks, the answer unblocks ----
: >"$HD_LOG"
# A longer pre-stream pause keeps the prompt's arrival well after the submit.
launch "$S132" 100 40 "$(hd_app 1500)"
submit "$S132" "permission demo please"
wait_for 15 "$S132" -F "Do you want to create hello.py?" || fail "the permission prompt never opened"
hd_has 5 '^pane\.report_agent blocked \[Create file: [^]]*hello\.py\]' ||
	fail "the open permission prompt was never reported blocked, naming its file"
tmux send-keys -t "$S132" -l "1"
wait_for 20 "$S132" -F "Wrote 8 lines to hello.py" || fail "the approved write never resolved"
wait_for 20 "$S132" -E "^$SUMMARY_RE" || fail "the permission turn never settled"
hd_has 10 '^pane\.report_agent idle \[\] alter-zero --resume ' ||
	fail "the answered turn never settled to idle"
tmux send-keys -t "$S132" C-c
wait_gone 5 "$S132" || fail "launch 2 did not quit on Ctrl+C"
rows="$(hd_rows)"
dump "launch 2's requests" "$rows"
expect_has "$rows" -F "PROBLEMS: none" "launch 2: a request broke the protocol"
expect_eq "$(hd_states)" "idle working blocked working idle release" \
	"launch 2: the states did not run idle → working → blocked → working → idle → release"

# ---- launches 3 and 4: the off switch, and a nested session, stay silent ----
for quiet in "ALTER_ZERO_HERDR=0" "ALTER_ZERO_HERDR_REPORTER=w1:p1"; do
	: >"$HD_LOG"
	launch "$S132" 100 40 "$(hd_app "$SMOKE_STARTUP_MS" "$quiet")"
	submit "$S132" "$USER_MSG"
	wait_for 20 "$S132" -F "$SETTLED_REPLY" || fail "[$quiet] the turn never settled"
	tmux send-keys -t "$S132" C-c
	wait_gone 5 "$S132" || fail "[$quiet] the app did not quit"
	expect_eq "$(wc -l <"$HD_LOG" | tr -d ' ')" "0" "[$quiet] the session reported to herdr anyway"
done
