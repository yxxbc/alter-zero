#!/usr/bin/env bash
# Phase 132 — a herdr pane hears the agent through the installed hooks: idle, working, blocked, released

. "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"
smoke_begin

# The herdr integration (docs/herdr.md), end to end through the REAL backend
# against a stub provider. `alter-zero integration install herdr` writes
# hooks.d/herdr.json into the phase's config home, and a session launched
# with herdr's pane variables runs the reporter at every lifecycle event,
# which tells a stub herdr socket what changed. The stub logs each request
# line, and the phase reads them back:
#
# 1. launch → `idle` at once: SessionStart fires in the background after the
#    first frame (Claude Code's timing), not at the first prompt — its
#    restore command a bare `alter-zero`, since a fresh session has no
#    rollout to resume (and a stale `--resume` herdr kept would reopen the
#    wrong conversation).
# 2. a turn → `working`, then `idle`, both naming the conversation and
#    carrying `resume_argv` = `alter-zero --resume {id}` — the id the quit
#    hint prints, which is the one `--resume` resolves; the rollout exists
#    before the turn's first hook, so even the first `working` carries it.
# 3. a `bash` call's permission prompt → `blocked` while it is on screen,
#    and Esc → `idle` (the Interrupt event: Esc used to fire nothing).
# 4. quit → `pane.release_agent`, and the pane's state file gone.
# 5. herdr's restore: the resume command it was given, run in the pane's
#    directory, brings the conversation back — and that session reports
#    the same conversation at its launch.
# 6. the user's own hooks (docs/hooks.md *Integrations*): a prompt they
#    refuse never reaches herdr, a Stop they turn into another round keeps
#    the pane `working` through it, and a turn cut short by `/clear` or by a
#    quit ends with `Interrupt` before the session's `SessionEnd`.
# Every `seq` is higher than the one before it, across both processes.
S132="${S}_herdr"
WORK="$(work_dir herdr)"
PANE="w9:p1"
HERDR_SOCK="$SMOKE_TMP/herdr.sock"
HERDR_LOG="$SMOKE_TMP/herdr.log"
STUB_LOG="$SMOKE_TMP/stub132.log"
: >"$HERDR_LOG"

# ---- the stub herdr: one request line per connection, answered `ok` ----
python3 - "$HERDR_SOCK" "$HERDR_LOG" <<'PY' &
import json, os, socket, sys

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
        line = data.decode(errors="replace").strip()
        if not line:
            continue
        with open(log, "a") as f:
            f.write(line + "\n")
        try:
            rid = json.loads(line).get("id")
        except ValueError:
            rid = None
        conn.sendall((json.dumps({"id": rid, "result": {"type": "ok"}}) + "\n").encode())
PY
HERDR_PID=$!
smoke_on_exit kill "$HERDR_PID"
for _ in $(seq 1 50); do
	[ -S "$HERDR_SOCK" ] && break
	sleep 0.1
done
[ -S "$HERDR_SOCK" ] || fail "the stub herdr socket never came up"

# ---- the stub provider: a reply, or a `bash` call for the prompt turn ----
STUB_PORT="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')"
python3 - "$STUB_PORT" "$STUB_LOG" <<'PY' &
import json, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

port, log = int(sys.argv[1]), sys.argv[2]

def sse(frames):
    body = "".join("data: %s\n\n" % json.dumps(f) for f in frames) + "data: [DONE]\n\n"
    return body.encode()

def text(message):
    content = message.get("content")
    if isinstance(content, list):
        return " ".join(part.get("text", "") for part in content if isinstance(part, dict))
    return content or ""

class Stub(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send(b'{"object":"list","data":[{"id":"stub-model"}]}', "application/json")

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length") or 0)).decode()
        with open(log, "a") as f:
            f.write(body.replace("\n", " ") + "\n")
        last = json.loads(body).get("messages", [{}])[-1]
        said = text(last) if last.get("role") == "user" else ""
        # A turn slow enough to cut short (section 6), and a Stop hook's
        # extra round slow enough to look at the pane during it.
        if "smoke132 slow" in said:
            time.sleep(8)
        elif "Stop hook feedback" in said:
            time.sleep(2)
        if "smoke132 touch" in said:
            call = {"index": 0, "id": "call_132", "type": "function",
                    "function": {"name": "bash", "arguments": json.dumps(
                        {"command": "touch herdr-was-here", "description": "Touch a file"})}}
            frames = [{"choices": [{"delta": {"tool_calls": [call]}}]},
                      {"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}]
        else:
            frames = [{"choices": [{"delta": {"content": "All set for herdr."}}]},
                      {"choices": [{"delta": {}, "finish_reason": "stop"}]}]
        self.send(sse(frames), "text/event-stream")

    def send(self, data, kind):
        try:
            self.send_response(200)
            self.send_header("content-type", kind)
            self.send_header("content-length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except OSError:
            pass  # a turn cut short hung up first

    def log_message(self, *_args):
        pass

ThreadingHTTPServer(("127.0.0.1", port), Stub).serve_forever()
PY
STUB_PID=$!
smoke_on_exit kill "$STUB_PID"
PROVIDERS="$SMOKE_TMP/providers132.toml"
cat >"$PROVIDERS" <<EOF
[providers.stub]
name = "Stub"

[providers.stub.kwargs]
api_base = "http://127.0.0.1:$STUB_PORT/v1"
EOF

# The pane's variables, as herdr sets them for every process in a pane.
# ALTER_ZERO_AGENT stays unset (the sanitized env has none): it marks an
# agent's own terminal, where the reporter must stay quiet.
HERDR_VARS="HERDR_ENV=1 HERDR_PANE_ID=$PANE HERDR_SOCKET_PATH=$HERDR_SOCK"
REAL_APP="env NO_PROXY=127.0.0.1 no_proxy=127.0.0.1 $CFG_ENV_NOHIST ALTER_ZERO_PROVIDERS_FILE=$PROVIDERS ALTER_ZERO_PROVIDER=stub ALTER_ZERO_MODEL=stub-model STUB_API_KEY=stand-in $HERDR_VARS $BIN_ABS"

# herdr_lines — the requests the stub has logged so far.
herdr_lines() { grep -c . "$HERDR_LOG"; }
# herdr_wait SECONDS COUNT PATTERN — wait until COUNT requests are logged
# and the newest matches PATTERN (fixed string).
herdr_wait() {
	local secs="$1" count="$2" pattern="$3" n i
	n="$(smoke_ticks "$secs")"
	for ((i = 0; i < n; i++)); do
		if [ "$(herdr_lines)" -ge "$count" ] && tail -1 "$HERDR_LOG" | grep -qF "$pattern"; then
			return 0
		fi
		sleep "$SMOKE_POLL_INTERVAL"
	done
	return 1
}

# ---- install, and status from inside the pane ----
install_out="$(env ALTER_ZERO_CONFIG_DIR="$SMOKE_CFG" "$BIN_ABS" integration install herdr 2>&1)"
dump "integration install herdr" "$install_out"
expect_has "$install_out" -F "Installed the herdr integration" "install did not report success"
[ -f "$SMOKE_CFG/hooks.d/herdr.json" ] || fail "install wrote no hooks.d/herdr.json"
status_out="$(env ALTER_ZERO_CONFIG_DIR="$SMOKE_CFG" $HERDR_VARS "$BIN_ABS" integration status 2>&1)"
dump "integration status, inside the pane" "$status_out"
expect_has "$status_out" -F "herdr: installed" "status does not report the integration installed"
expect_has "$status_out" -F "this terminal is herdr pane $PANE" "status does not name the pane"

# ---- 1. launch: idle at once, nothing to resume yet ----
tmux new-session -d -s "$S132" -x 100 -y 30 -c "$WORK" "$REAL_APP; echo HERDR_APP_EXITED; sleep 60"
wait_for 10 "$S132" -E '^❯' || fail "the app never came up"
wait_for 10 "$S132" -F "stub-model" || fail "the footer never named the stub model (the real backend did not activate)"
herdr_wait 10 1 '"state":"idle"' || fail "herdr heard nothing at launch (SessionStart waited for a prompt?)"
dump "herdr, after launch" "$(cat "$HERDR_LOG")"
expect_eq "$(herdr_lines)" "1" "launch sent other than one report"
expect_has "$(cat "$HERDR_LOG")" -F '"resume_argv":["alter-zero"]' \
	"a fresh session's restore command is not a fresh start"
expect_lacks "$(cat "$HERDR_LOG")" -F "agent_session_id" "a fresh session with no rollout named one to resume"
STATE_FILES="$SMOKE_CFG/herdr"
[ -n "$(find "$STATE_FILES" -name '*.json' 2>/dev/null)" ] || fail "the reporter kept no state for the pane"

# ---- 2. a turn: working, then idle, with the resume command ----
submit "$S132" "smoke132 hello"
turn="$(wait_settled 20 "$S132" -F "All set for herdr.")" || fail "the turn never settled"
dump "the turn" "$turn"
herdr_wait 10 3 '"state":"idle"' || fail "herdr never heard the turn end"
dump "herdr, after the turn" "$(cat "$HERDR_LOG")"
expect_eq "$(sed -n 2p "$HERDR_LOG" | grep -cF '"state":"working"')" "1" "the turn did not report working first"
expect_has "$(sed -n 2p "$HERDR_LOG")" -F '"resume_argv":["alter-zero","--resume",' \
	"the turn's first working carries no resume command (the rollout came after its hook)"
expect_has "$(sed -n 3p "$HERDR_LOG")" -F '"resume_argv":["alter-zero","--resume",' \
	"the idle after the turn carries no resume command"

# ---- 3. a permission prompt: blocked; Esc: idle ----
submit "$S132" "smoke132 touch"
prompt="$(wait_pane 20 "$S132" -F "This command requires approval")" ||
	fail "the bash call's permission prompt never opened"
dump "the permission prompt" "$prompt"
herdr_wait 10 5 '"state":"blocked"' || fail "herdr never heard the prompt (still: $(tail -1 "$HERDR_LOG"))"
blocked_at="$(herdr_lines)"
keys "$S132" Escape
herdr_wait 10 $((blocked_at + 1)) '"state":"idle"' || fail "herdr never heard the Esc (still: $(tail -1 "$HERDR_LOG"))"
escaped="$(wait_settled 10 "$S132" -E '^❯')" || fail "the composer never came back after Esc"
dump "after Esc" "$escaped"
[ -e "$WORK/herdr-was-here" ] && fail "the refused command ran"

# ---- 4. quit: the release, the hint, the state forgotten ----
keys "$S132" C-c
wait_for 10 "$S132" -F "HERDR_APP_EXITED" || fail "the app did not quit"
quit_pane="$(pane "$S132" -S -60)"
dump "after quitting" "$quit_pane"
herdr_wait 10 $((blocked_at + 2)) '"method":"pane.release_agent"' || fail "the quit released nothing"
[ -z "$(find "$STATE_FILES" -name '*.json' 2>/dev/null)" ] || fail "the pane's state outlived the release"
hint_id="$(printf '%s\n' "$quit_pane" | sed -n 's/.*--resume \([A-Za-z0-9-]*\).*/\1/p' | tail -1)"
[ -n "$hint_id" ] || fail "the quit printed no resume hint"
tmux kill-session -t "$S132" 2>/dev/null

# ---- 5. herdr's restore: the resume command brings the conversation back ----
released_at="$(herdr_lines)"
resume_id="$(python3 -c 'import json, sys
for line in open(sys.argv[1]):
    argv = json.loads(line)["params"].get("resume_argv")
    if argv and len(argv) == 3:
        print(argv[2])' "$HERDR_LOG" | tail -1)"
expect_eq "$resume_id" "$hint_id" "herdr's resume command does not name the conversation the quit hint does"
tmux new-session -d -s "$S132" -x 100 -y 30 -c "$WORK" "$REAL_APP --resume $resume_id; echo HERDR_APP_EXITED; sleep 60"
restored="$(wait_pane 10 "$S132" -S -80 -- -F "smoke132 hello")" ||
	fail "the resume command did not bring the conversation back"
dump "the restored conversation" "$restored"
herdr_wait 10 $((released_at + 1)) '"state":"idle"' || fail "the resumed session reported nothing at launch"
expect_has "$(tail -1 "$HERDR_LOG")" -F "\"agent_session_id\":\"$resume_id\"" \
	"the resumed session does not report the conversation it resumed"
keys "$S132" C-c
wait_for 10 "$S132" -F "HERDR_APP_EXITED" || fail "the resumed app did not quit"
herdr_wait 10 $((released_at + 2)) '"method":"pane.release_agent"' || fail "the resumed session's quit released nothing"
tmux kill-session -t "$S132" 2>/dev/null

# ---- the whole log: the states in order, every request well-formed ----
python3 - "$HERDR_LOG" "$PANE" "$resume_id" <<'PY' || fail "the requests herdr saw are wrong (see above)"
import json, sys

log, pane, session = sys.argv[1], sys.argv[2], sys.argv[3]
requests = [json.loads(line) for line in open(log) if line.strip()]
problems = []
steps = []
last_seq = 0
for r in requests:
    p = r.get("params", {})
    if p.get("pane_id") != pane or p.get("source") != "alter-zero" or p.get("agent") != "alter-zero":
        problems.append("a request names the wrong pane or source: %r" % r)
    seq = p.get("seq", 0)
    if seq <= last_seq:
        problems.append("seq did not rise: %d after %d" % (seq, last_seq))
    last_seq = seq
    if r.get("id") != "alter-zero:%d" % seq:
        problems.append("the request id is not the source and seq: %r" % r.get("id"))
    if r.get("method") == "pane.release_agent":
        steps.append("release")
    else:
        steps.append(p.get("state"))
    argv = p.get("resume_argv")
    if argv == ["alter-zero"]:
        if "agent_session_id" in p:
            problems.append("a fresh start names a conversation: %r" % p)
    elif argv is not None and (argv != ["alter-zero", "--resume", session] or p.get("agent_session_id") != session):
        problems.append("a resume command names another conversation: %r" % p)
# Consecutive repeats collapse: PreToolUse reports `working` again under the turn's.
collapsed = [s for i, s in enumerate(steps) if i == 0 or s != steps[i - 1]]
want = ["idle", "working", "idle", "working", "blocked", "idle", "release", "idle", "release"]
if collapsed != want:
    problems.append("the states herdr heard: %s (want %s)" % (collapsed, want))
for problem in problems:
    print("FAIL:", problem)
sys.exit(1 if problems else 0)
PY

# ---- outside herdr: the installed hooks send nothing ----
# The same install, a session with no pane variables: the hook's `sh` guard
# exits before the reporter starts.
before="$(herdr_lines)"
QUIET_APP="env NO_PROXY=127.0.0.1 no_proxy=127.0.0.1 $CFG_ENV_NOHIST ALTER_ZERO_PROVIDERS_FILE=$PROVIDERS ALTER_ZERO_PROVIDER=stub ALTER_ZERO_MODEL=stub-model STUB_API_KEY=stand-in $BIN_ABS"
launch -c "$WORK" "${S132}_out" 100 30 "$QUIET_APP"
submit "${S132}_out" "smoke132 outside"
wait_settled 20 "${S132}_out" -F "All set for herdr." >/dev/null || fail "the outside turn never settled"
keys "${S132}_out" C-c
wait_gone 10 "${S132}_out" || fail "the outside session did not quit"
expect_eq "$(herdr_lines)" "$before" "a session outside herdr reported to it"

# ---- 6. the user's own hooks: what they refuse, herdr never hears ----
# A user hooks file beside the integration: UserPromptSubmit refuses one
# prompt, Stop asks for one more round once, and every boundary event is
# recorded. The integration observes: it runs after the user's hooks and
# not at all for an event they stopped (docs/hooks.md *Integrations*).
HOOKS132="$SMOKE_TMP/hooks132.json"
EVENTS132="$SMOKE_TMP/events132.jsonl"
ONCE132="$SMOKE_TMP/stop-once132"
: >"$EVENTS132"
rm -f "$ONCE132"
python3 - "$HOOKS132" "$EVENTS132" "$ONCE132" <<'PY'
import json, sys

path, events, once = sys.argv[1:]
record = {"type": "command", "command": "cat >> %s; echo >> %s" % (events, events)}
refuse = {"type": "command",
          "command": 'case "$(cat)" in *"smoke132 refuse"*) echo "refused by the smoke policy" >&2; exit 2;; esac'}
again = {"type": "command",
         "command": "cat > /dev/null; [ -e %s ] && exit 0; touch %s; "
                    "printf '%%s' '{\"decision\":\"block\",\"reason\":\"one more round\"}'" % (once, once)}
hooks = {
    "UserPromptSubmit": [{"hooks": [refuse, record]}],
    "Stop": [{"hooks": [again, record]}],
    "Interrupt": [{"hooks": [record]}],
    "SessionStart": [{"hooks": [record]}],
    "SessionEnd": [{"hooks": [record]}],
}
with open(path, "w") as f:
    json.dump({"hooks": hooks}, f)
PY
VETO_APP="env NO_PROXY=127.0.0.1 no_proxy=127.0.0.1 $CFG_ENV_NOHIST ALTER_ZERO_HOOKS=1 ALTER_ZERO_HOOKS_FILE=$HOOKS132 ALTER_ZERO_PROVIDERS_FILE=$PROVIDERS ALTER_ZERO_PROVIDER=stub ALTER_ZERO_MODEL=stub-model STUB_API_KEY=stand-in $HERDR_VARS $BIN_ABS"
S132V="${S132}_veto"
vetoed_at="$(herdr_lines)"
tmux new-session -d -s "$S132V" -x 100 -y 30 -c "$WORK" "$VETO_APP; echo HERDR_APP_EXITED; sleep 60"
wait_for 10 "$S132V" -E '^❯' || fail "the hooked app never came up"
herdr_wait 10 $((vetoed_at + 1)) '"state":"idle"' || fail "the hooked session reported nothing at launch"

# A refused prompt: the turn never happens, so herdr never hears it.
submit "$S132V" "smoke132 refuse"
refused="$(wait_pane 10 "$S132V" -F "refused by the smoke policy")" || fail "the user's hook did not refuse the prompt"
dump "the refused prompt" "$refused"
sleep 1
expect_eq "$(herdr_lines)" "$((vetoed_at + 1))" "herdr heard a prompt the user's hook refused"
keys "$S132V" C-c # the refused text came back to the composer: clear it

# A Stop the user's hook turns into another round: `working` all through it.
stop_feedback() { grep -c "Stop hook feedback" "$STUB_LOG"; }
before_round="$(stop_feedback)"
submit "$S132V" "smoke132 again"
herdr_wait 10 $((vetoed_at + 2)) '"state":"working"' || fail "herdr never heard the hooked turn start"
for _ in $(seq 1 50); do
	[ "$(stop_feedback)" -gt "$before_round" ] && break
	sleep 0.1
done
[ "$(stop_feedback)" -gt "$before_round" ] || fail "the user's Stop hook asked for no extra round"
expect_has "$(tail -1 "$HERDR_LOG")" -F '"state":"working"' "herdr went idle on a Stop the user's hook turned into another round"
herdr_wait 15 $((vetoed_at + 3)) '"state":"idle"' || fail "herdr never heard the extra round end"
expect_eq "$(herdr_lines)" "$((vetoed_at + 3))" "the continued turn reported other than working, then idle"

# A turn cut short by /clear: its Interrupt, then the session boundary.
slow_requests() { grep -c "smoke132 slow" "$STUB_LOG"; }
before_slow="$(slow_requests)"
submit "$S132V" "smoke132 slow"
herdr_wait 10 $((vetoed_at + 4)) '"state":"working"' || fail "herdr never heard the slow turn start"
for _ in $(seq 1 50); do
	[ "$(slow_requests)" -gt "$before_slow" ] && break
	sleep 0.1
done
submit "$S132V" "/clear"
herdr_wait 10 $((vetoed_at + 6)) '"state":"idle"' || fail "herdr never heard the cleared turn end"

# …and by a quit: its Interrupt, then the release.
before_slow="$(slow_requests)"
submit "$S132V" "smoke132 slow"
herdr_wait 10 $((vetoed_at + 7)) '"state":"working"' || fail "herdr never heard the second slow turn start"
for _ in $(seq 1 50); do
	[ "$(slow_requests)" -gt "$before_slow" ] && break
	sleep 0.1
done
keys "$S132V" C-c
wait_for 10 "$S132V" -F "HERDR_APP_EXITED" || fail "the hooked app did not quit mid-turn"
herdr_wait 10 $((vetoed_at + 9)) '"method":"pane.release_agent"' || fail "the mid-turn quit released nothing"
tmux kill-session -t "$S132V" 2>/dev/null
dump "herdr, the hooked session" "$(tail -n +$((vetoed_at + 1)) "$HERDR_LOG")"
dump "the user's hooks, the hooked session" "$(cat "$EVENTS132")"

python3 - "$HERDR_LOG" "$vetoed_at" "$EVENTS132" <<'PY' || fail "the hooked session's events are wrong (see above)"
import json, sys

log, skip, events = sys.argv[1], int(sys.argv[2]), sys.argv[3]
requests = [json.loads(line) for line in open(log) if line.strip()][skip:]
heard = ["release" if r["method"] == "pane.release_agent" else r["params"]["state"] for r in requests]
want = ["idle", "working", "idle", "working", "idle", "idle", "working", "idle", "release"]
problems = []
if heard != want:
    problems.append("herdr heard %s (want %s)" % (heard, want))
fired = []
for line in open(events):
    if not line.strip():
        continue
    e = json.loads(line)
    name = e["hook_event_name"]
    detail = e.get("prompt") or e.get("reason") or e.get("source") or ""
    fired.append("%s(%s)" % (name, detail) if detail else name)
want_fired = [
    "SessionStart(startup)",
    "UserPromptSubmit(smoke132 refuse)",
    "UserPromptSubmit(smoke132 again)",
    "Stop",
    "Stop",
    "UserPromptSubmit(smoke132 slow)",
    "Interrupt",
    "SessionEnd(clear)",
    "SessionStart(clear)",
    "UserPromptSubmit(smoke132 slow)",
    "Interrupt",
    "SessionEnd(prompt_input_exit)",
]
if fired != want_fired:
    problems.append("the user's hooks heard %s (want %s)" % (fired, want_fired))
for problem in problems:
    print("FAIL:", problem)
sys.exit(1 if problems else 0)
PY
