#!/bin/sh
# Two-node sync E2E for docs/sync.md section 14 (TASK-424).
#
# Usage: sh scripts/e2e-sync.sh /path/to/kanban4ai [WORKDIR]
#
# Runs server, laptop and mobile nodes in Linux network namespaces joined to the
# server by veth pairs, so "sleep", "trip" and "power off" are real link/process
# events while every store stays inspectable. The mobile node is outbound-only
# and syncs from `kanban daemon`, as on a phone. Needs Linux, passwordless sudo (ip netns),
# git, curl and python3. Run it only on a disposable test machine (VM 103).
#
# Implemented behaviour is asserted (FAIL -> exit 1). Spec items owned by
# unfinished tasks print PENDING <task>; STRICT=1 turns them into failures.
set -eu

BIN=$(readlink -f "${1:?usage: e2e-sync.sh BINARY [WORKDIR]}")
W=${2:-$(mktemp -d /tmp/kanban-e2e-sync.XXXXXX)}
W=$(readlink -f "$W")
STRICT=${STRICT:-0}
NS_S=kbe2e-s NS_L=kbe2e-l NS_M=kbe2e-m PORT=7420
IP_S=10.77.0.1 IP_L=10.77.0.2 IP_SM=10.78.0.1 IP_M=10.78.0.3
FAILS=0 PENDINGS=0

log() { printf '%s\n' "$*"; }
ok() { log "ok      $*"; }
fail() { log "FAIL    $*"; FAILS=$((FAILS + 1)); }
pending() {
	log "PENDING $*"
	PENDINGS=$((PENDINGS + 1))
	[ "$STRICT" = 1 ] && FAILS=$((FAILS + 1))
	return 0
}
check() { # check NAME COMMAND...
	name=$1; shift
	if "$@"; then ok "$name"; else fail "$name"; fi
}
expect() { # expect TASK NAME COMMAND...: spec item owned by an unfinished task
	task=$1 name=$2; shift 2
	if "$@"; then ok "$name"; else pending "$task: $name"; fi
}
wait_for() { # wait_for SECONDS COMMAND...
	limit=$1; shift
	i=0
	while [ "$i" -lt "$limit" ]; do
		"$@" >/dev/null 2>&1 && return 0
		sleep 1; i=$((i + 1))
	done
	return 1
}

ns_of() { case $1 in server) echo $NS_S ;; laptop) echo $NS_L ;; mobile) echo $NS_M ;; esac; }
# kb NODE ARGS...: run the binary as NODE (its store, its namespace).
kb() {
	node=$1; shift
	sudo ip netns exec "$(ns_of "$node")" sudo -u "$(id -un)" env \
		HOME="$HOME" PATH="$PATH" KANBAN_HOME="$W/$node" \
		FAKE_AGENT_LOG="$W/$node.agent.log" FAKE_AGENT_SECS="${FAKE_AGENT_SECS:-60}" \
		GIT_AUTHOR_NAME=e2e GIT_AUTHOR_EMAIL=e2e@test GIT_COMMITTER_NAME=e2e \
		GIT_COMMITTER_EMAIL=e2e@test \
		"$BIN" "$@"
}
api() { # api NODE METHOD PATH [JSON] -> body on stdout
	sudo ip netns exec "$(ns_of "$1")" curl -s -X "$2" \
		-H "Authorization: Bearer $(cat "$W/$1.tok")" \
		-H 'Content-Type: application/json' ${4:+-d "$4"} \
		"http://127.0.0.1:$PORT$3"
}
P=/api/v1/projects/work/tasks
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
probe_code() { kb "$1" serve --probe | json 'd[0]["code"]'; }
start_serve() { kb "$1" serve >>"$W/$1.serve.log" 2>&1 </dev/null & }
start_daemon() { kb "$1" daemon --interval 5 >>"$W/$1.daemon.log" 2>&1 </dev/null & }
# Processes of NODE outside its namespace too (the sudo wrappers around it).
node_procs() { ps -eo pid=,args= | awk -v w="KANBAN_HOME=$W/$1 " '$2 == "sudo" && index($0, w) {print $1}'; }
power_off() { # SIGKILL every process of NODE, as a power cut would
	sudo ip netns pids "$(ns_of "$1")" | xargs -r sudo kill -9
}
link() { sudo ip -n $NS_L link set kbe2el0 "$1"; }
mobile_link() { sudo ip -n $NS_M link set kbe2em0 "$1"; }
serve_pid() {
	for pid in $(sudo ip netns pids "$(ns_of "$1")"); do
		case $(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null) in
		"$BIN serve "*) echo "$pid" ;;
		esac
	done
}
same() { # same FILE_A FILE_B: JSON equality, ignoring the node-local thread `rev`
	python3 -c 'import json,sys
a,b=(json.load(open(p)) for p in sys.argv[1:])
for x in (a,b):
    if isinstance(x,dict): x.pop("rev",None)
sys.exit(a!=b)' "$1" "$2"
}
boards_equal() {
	api server GET /api/v1/projects/work/board >"$W/board.server.json"
	api laptop GET /api/v1/projects/work/board >"$W/board.laptop.json"
	same "$W/board.server.json" "$W/board.laptop.json"
}
threads_equal() {
	for t in "$@"; do
		api server GET "$P/$t/thread" >"$W/thread.server.json"
		api laptop GET "$P/$t/thread" >"$W/thread.laptop.json"
		same "$W/thread.server.json" "$W/thread.laptop.json" || return 1
	done
}
probe_is() { [ "$(probe_code "$1")" = "$2" ]; }
api_has() { # api_has NODE METHOD PATH JSON|"" PATTERN
	api "$1" "$2" "$3" "$4" | grep -q "$5"
}
runs_count_is() { [ "$(api "$1" GET "$P/$2" | json 'len(d["runs"])')" = "$3" ]; }
status_on_both() {
	for n in server laptop; do
		[ "$(api "$n" GET "$P/$1" | json 'd["task"]["status"]')" = "$2" ] || return 1
	done
}
both_started() { grep -q '"session"' "$1" && grep -q '"session"' "$2"; }
# Replicated task fields from the CLI: works for the mobile node, which has no API.
tasks_of() {
	kb "$1" --project work list --format json | python3 -c 'import json,sys
print(sorted((t["id"],t["title"],t["status"],t["description"]) for t in json.load(sys.stdin)))'
}
replicas_equal() {
	s=$(tasks_of server) && [ "$s" = "$(tasks_of laptop)" ] && [ "$s" = "$(tasks_of mobile)" ]
}
phone_creates() {
	i=0
	while [ "$i" -lt "$1" ]; do
		i=$((i + 1))
		kb mobile --project work create "phone alone $i" >/dev/null || return 1
	done
}
task_count_is() { [ "$(kb "$1" --project work list --format json | json 'len(d)')" = "$2" ]; }
value_is() { [ "$(json "$2" <"$1")" = "$3" ]; }
only_conflicts() {
	[ "$(python3 -c 'import json,sys; print(sorted({(c["id"],c["field"]) for c in map(json.loads,open(sys.argv[1]))}))' "$1")" = "$2" ]
}

cleanup() {
	set +e
	for node in server laptop mobile; do
		pids=$(node_procs "$node")
		[ -n "$pids" ] && sudo kill -CONT $pids && sudo kill -9 $pids
	done
	for ns in $NS_S $NS_L $NS_M; do
		sudo ip netns pids "$ns" 2>/dev/null | xargs -r sudo kill -9
		sudo ip netns delete "$ns" 2>/dev/null
	done
	log "logs and stores kept in $W"
}

# --- setup -------------------------------------------------------------------
for ns in $NS_S $NS_L $NS_M; do
	if ip netns list | grep -qw "$ns"; then
		log "namespace $ns already exists; another run in progress?"; exit 2
	fi
done
trap cleanup EXIT INT TERM
mkdir -p "$W/server" "$W/laptop" "$W/mobile"
sudo ip netns add $NS_S
sudo ip netns add $NS_L
sudo ip link add kbe2es0 type veth peer name kbe2el0
sudo ip link set kbe2es0 netns $NS_S
sudo ip link set kbe2el0 netns $NS_L
sudo ip -n $NS_S addr add $IP_S/24 dev kbe2es0
sudo ip -n $NS_L addr add $IP_L/24 dev kbe2el0
sudo ip netns add $NS_M
sudo ip link add kbe2es1 type veth peer name kbe2em0
sudo ip link set kbe2es1 netns $NS_S
sudo ip link set kbe2em0 netns $NS_M
sudo ip -n $NS_S addr add $IP_SM/24 dev kbe2es1
sudo ip -n $NS_M addr add $IP_M/24 dev kbe2em0
for ns in $NS_S $NS_L $NS_M; do sudo ip -n "$ns" link set lo up; done
sudo ip -n $NS_S link set kbe2es0 up
sudo ip -n $NS_S link set kbe2es1 up
link up
mobile_link up

cat >"$W/fake-agent" <<'EOF'
#!/bin/sh
# Stand-in agent: heartbeat FAKE_AGENT_SECS, commit, record context, report done.
echo "start $KANBAN_TASK_ID $KANBAN_SESSION" >>"$FAKE_AGENT_LOG"
end=$(($(date +%s) + FAKE_AGENT_SECS))
while [ "$(date +%s)" -lt "$end" ]; do
	"$KANBAN_CMD" heartbeat --session "$KANBAN_SESSION" >/dev/null 2>&1
	sleep 2
done
echo "$KANBAN_SESSION" >"fake-$KANBAN_TASK_ID.txt"
git add . && git commit -qm "fake $KANBAN_TASK_ID"
"$KANBAN_CMD" context "$KANBAN_TASK_ID" "fake agent finished" --source agent >/dev/null 2>&1
"$KANBAN_CMD" done "$KANBAN_TASK_ID" --session "$KANBAN_SESSION" --agent >>"$FAKE_AGENT_LOG" 2>&1
echo "done $KANBAN_TASK_ID" >>"$FAKE_AGENT_LOG"
EOF
chmod +x "$W/fake-agent"

git init -q -b master "$W/work"
git -C "$W/work" -c user.name=e2e -c user.email=e2e@test commit -q --allow-empty -m init
(cd "$W/work" && kb server init >/dev/null)
kb laptop project list >/dev/null 2>&1 || true
kb mobile project list >/dev/null 2>&1 || true
sed -i 's/^  name: laptop$/  name: server/; s/^  role: laptop$/  role: server/' "$W/server/config.yaml"
sed -i 's/^  name: laptop$/  name: phone/; s/^  role: laptop$/  role: mobile/; s/^  dial: inbound$/  dial: outbound/' \
	"$W/mobile/config.yaml"
for node in server laptop mobile; do
	cat >>"$W/$node/config.yaml" <<EOF
serve:
  port: $PORT
  shutdown_timeout: 2
sync:
  interval: 5
  probe_timeout: 2
agents:
  opencode:
    command: $W/fake-agent
EOF
done
start_serve server
start_serve laptop
check "server and laptop serve answer /health" \
	wait_for 20 sh -c "sudo ip netns exec $NS_L curl -sf http://$IP_S:$PORT/health && sudo ip netns exec $NS_S curl -sf http://$IP_L:$PORT/health"
code=$(kb server serve --pair-code | grep -o '[0-9]\{6\}' | head -n 1)
kb laptop serve --pair "$IP_S:$PORT" --code "$code" --address "$IP_L:$PORT" >/dev/null
check "laptop bootstraps the server replica" \
	wait_for 30 sh -c "grep -q 'replica bootstrapped' '$W/laptop.serve.log'"
git clone -q "$W/work" "$W/laptop-work"
kb laptop project set-path work "$W/laptop-work" >/dev/null
code=$(kb server serve --pair-code | grep -o '[0-9]\{6\}' | head -n 1)
kb mobile serve --pair "$IP_SM:$PORT" --code "$code" >/dev/null
start_daemon mobile
check "outbound mobile node bootstraps from its daemon" \
	wait_for 40 sh -c "grep -q 'replica bootstrapped' '$W/mobile.daemon.log'"
kb server serve --issue-token node >"$W/server.tok"
kb laptop serve --issue-token node >"$W/laptop.tok"

# --- 14.1 night on the server -> morning on the laptop -------------------------
log "== 14.1 night on server, morning on laptop"
kb server --project work create "srv baseline" >/dev/null   # TASK-001
check "laptop gets an ID lease and creates a task" \
	wait_for 30 kb laptop --project work create "lap baseline"
LAP1=$(api laptop GET /api/v1/projects/work/board | json '[t["id"] for t in d["tasks"] if t["title"]=="lap baseline"][0]')
check "online writes converge" wait_for 15 boards_equal
kb laptop serve --bye sleep >/dev/null
LP=$(serve_pid laptop)
sudo kill -STOP "$LP"
link down
check "server reports the sleeping laptop as NODE_POWERED_OFF" wait_for 10 probe_is server NODE_POWERED_OFF
for i in 1 2 3; do kb server --project work create "night $i" >/dev/null; done   # TASK-002..004
kb server --project work context TASK-001 "night progress" --source agent >/dev/null
kb server --project work move TASK-001 in_progress >/dev/null
kb server --project work ask "$LAP1" "night question?" --agent >/dev/null 2>&1 || true
link up
# Wake: no hello, no command. sudo stopped itself with its child; resume both.
sudo kill -CONT "$LP" $(node_procs laptop) 2>/dev/null || true
check "laptop catches up after wake without any action" wait_for 20 boards_equal
check "threads are identical after catch-up" threads_equal TASK-001 "$LAP1"
check "server sees the laptop ONLINE again" wait_for 30 probe_is server ONLINE

# --- 14.2 offline trip --------------------------------------------------------
log "== 14.2 offline trip with edits on both sides"
link down
check "laptop reports the server as NODE_UNREACHABLE" wait_for 10 probe_is laptop NODE_UNREACHABLE
api server PATCH "$P/TASK-002" '{"title":"t2 from server"}' >/dev/null
api server PATCH "$P/TASK-003" '{"title":"t3 title server"}' >/dev/null
kb server --project work context "$LAP1" "server note offline" --source agent >/dev/null
for t in A B C D; do kb server --project work create "srv offline $t" >/dev/null; done   # TASK-005..008
sleep 2   # the laptop edit of the same field is the later one
api laptop PATCH "$P/TASK-002" '{"title":"t2 from laptop"}' >/dev/null
api laptop PATCH "$P/TASK-003" '{"description":"t3 desc laptop"}' >/dev/null
api laptop POST "$P/TASK-004/move" '{"status":"in_progress"}' >/dev/null
kb laptop --project work context "$LAP1" "laptop note offline" --source agent >/dev/null
kb laptop --project work create "lap offline A" >/dev/null
kb laptop --project work create "lap offline B" >/dev/null
link up
check "boards converge after the trip" wait_for 30 boards_equal
check "threads converge after the trip" threads_equal TASK-002 TASK-003 TASK-004 "$LAP1"
check "ids from both sides are disjoint: all 11 tasks present" \
	value_is "$W/board.server.json" 'len({t["id"] for t in d["tasks"]})' 11
api server GET "$P/TASK-002" >"$W/t2.json"
check "same field edited on both sides: the later edit wins" \
	value_is "$W/t2.json" 'd["task"]["title"]' "t2 from laptop"
api laptop GET "$P/TASK-003" >"$W/t3.json"
check "different fields of one task both survive" \
	value_is "$W/t3.json" 'd["task"]["title"]+"|"+d["task"]["description"]' "t3 title server|t3 desc laptop"
check "conflict journals are identical on both nodes" \
	cmp -s "$W/server/sync/meta/conflicts.jsonl" "$W/laptop/sync/meta/conflicts.jsonl"
check "conflicts only for the field edited on both sides" \
	only_conflicts "$W/server/sync/meta/conflicts.jsonl" "[('TASK-002', 'title')]"
api laptop GET "$P/$LAP1/thread" >"$W/lap1-thread.json"
check "colliding MSG numbers get a node suffix" grep -q 'MSG-[0-9]*@' "$W/lap1-thread.json"

# --- Runner 03 launch-only alternatives and wait queue ------------------------
# A separate project keeps runner assertions independent of integration refs
# changed by the duplicate-run scenario below.
log "== Runner 03 explicit unavailable runner and recovery"
git init -q -b master "$W/runner-check"
git -C "$W/runner-check" -c user.name=e2e -c user.email=e2e@test commit -q --allow-empty -m init
kb server init --path "$W/runner-check" >/dev/null
kb server --project runner-check create "Runner recovery" >/dev/null
R=/api/v1/projects/runner-check/tasks/TASK-001
check "runner fixture reaches the laptop" wait_for 15 api_has laptop GET "$R" "" '"title":"Runner recovery"'
git clone -q "$W/runner-check" "$W/laptop-runner-check"
kb laptop project set-path runner-check "$W/laptop-runner-check" >/dev/null
kb server serve --bye shutdown >/dev/null
power_off server
check "runner fallback sees the server powered off" wait_for 10 probe_is laptop NODE_POWERED_OFF
check "the refusal offers alternatives" \
	api_has laptop POST "$R/launch" '{"runner":"server"}' alternatives
check "waiting keeps the explicit server runner queued" \
	api_has laptop POST "$R/launch" '{"runner":"server","on_unavailable":"queue"}' '"queued":true'
start_serve server
check "the queued runner comes back ONLINE" wait_for 30 probe_is laptop ONLINE
kb laptop --project runner-check check-sessions >"$W/runner-wait-dispatch.log"
check "the waiting launch goes to the server after it returns" \
	wait_for 15 api_has server GET "$R" "" '"state":"running"'
api laptop POST "$R/stop" '{}' >"$W/runner-wait-stop.json"

# --- 14.3 double launch -------------------------------------------------------
log "== 14.3 double launch"
api laptop POST "$P/TASK-005/launch" '{"runner":"server"}' >/dev/null
check "laptop sees the remote run" wait_for 15 api_has laptop GET "$P/TASK-005" "" '"state":"running"'
check "relaunch on the same runner -> RUN_ALREADY_RUNNING" \
	api_has laptop POST "$P/TASK-005/launch" '{"runner":"server"}' RUN_ALREADY_RUNNING
expect TASK-419 "launch on another node while running online -> RUN_ALREADY_RUNNING" \
	api_has laptop POST "$P/TASK-005/launch" '{"runner":"laptop"}' RUN_ALREADY_RUNNING
link down
api laptop POST "$P/TASK-006/launch" '{"runner":"laptop"}' >"$W/l6.json"
api server POST "$P/TASK-006/launch" '{"runner":"server"}' >"$W/s6.json"
link up
check "partitioned launches both start" both_started "$W/l6.json" "$W/s6.json"
expect TASK-419 "concurrent runs on two nodes get distinct session ids" \
	sh -c "! cmp -s '$W/l6.json' '$W/s6.json'"
check "both run descriptors replicate" wait_for 20 runs_count_is laptop TASK-006 2
expect TASK-421 "two runs of one generation are marked RUN_DUPLICATE" \
	api_has laptop GET "$P/TASK-006" "" RUN_DUPLICATE
check "both runs finish and the status converges to review" \
	wait_for $((${FAKE_AGENT_SECS:-60} + 90)) status_on_both TASK-006 review
expect TASK-422 "the second run's branch is kanban/TASK-006@<node>" \
	sh -c "git -C '$W/work' branch -a | grep -q 'kanban/TASK-006@'"

# --- 14.4 runner powered off mid-run ------------------------------------------
log "== 14.4 runner powered off mid-run"
api laptop POST "$P/TASK-007/launch" '{"runner":"server"}' >"$W/runner-launch.json"
check "laptop sees the remote run" wait_for 15 api_has laptop GET "$P/TASK-007" "" '"state":"running"'
kb server serve --bye shutdown >/dev/null
power_off server
check "laptop reports the server as NODE_POWERED_OFF" wait_for 10 probe_is laptop NODE_POWERED_OFF
check "explicit launch on the powered-off server is refused" \
	api_has laptop POST "$P/TASK-008/launch" '{"runner":"server"}' NODE_POWERED_OFF
check "the refusal offers alternatives" \
	api_has laptop POST "$P/TASK-008/launch" '{"runner":"server"}' alternatives
check "the interrupted run carries RUN_NODE_POWERED_OFF" \
	wait_for 20 api_has laptop GET "$P/TASK-007" "" RUN_NODE_POWERED_OFF
start_serve server
check "server comes back ONLINE" wait_for 30 probe_is laptop ONLINE

# --- 14.5 home powered off: the phone works alone ------------------------------
log "== 14.5 home powered off, phone alone"
check "phone replica converges with server and laptop" wait_for 30 replicas_equal
BEFORE=$(kb server --project work list --format json | json 'len(d)')
power_off server   # power cut: no bye, and the host stops answering at all
sudo ip -n $NS_S link set kbe2es1 down
link down          # the laptop is away too
sleep 3
log "info    phone sees the server as $(probe_code mobile)"
check "phone creates tasks from its ID reserve while alone" phone_creates 3
kb mobile --project work context TASK-001 "phone note while home is dark" --source agent >/dev/null
sudo ip -n $NS_S link set kbe2es1 up
start_serve server
link up
check "power back: all three replicas converge" wait_for 60 replicas_equal
check "phone task ids did not collide: $((BEFORE + 3)) tasks" task_count_is laptop $((BEFORE + 3))
check "phone thread note reached the laptop" threads_equal TASK-001

log "== summary: $FAILS failed, $PENDINGS pending (STRICT=$STRICT)"
[ "$FAILS" = 0 ]
