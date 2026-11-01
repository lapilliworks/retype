#!/usr/bin/env bash
#
# Probe every stage a supersession can break at, on this host, outside the test
# suite, so a failure names the stage instead of needing another round trip.
#
#   delivery   the platform reported the rename to a watcher at all
#   batching   the daemon collected the event
#   fact       correlation turned the event into a rename fact
#   holder     a running job holds the source name
#   cancel     the job was stopped and recorded as superseded
#
# Usage: scripts/probe-supersede.sh [--keep] [trial ...]
# Trials: renames supersede canonical (default: all three)
#
# --keep leaves the trees and their daemon logs in place and prints where.
set -u

root=$(cd "$(dirname "$0")/.." && pwd)
keep=0
trials=()

while [ $# -gt 0 ]; do
	case $1 in
	--keep) keep=1 ;;
	-*)
		echo "unknown option $1" >&2
		exit 2
		;;
	*) trials+=("$1") ;;
	esac
	shift
done

[ ${#trials[@]} -gt 0 ] || trials=(renames supersede canonical)

bin=${RETYPE_BIN:-${CARGO_TARGET_DIR:-$root/target}/debug/retype}

if [ ! -x "$bin" ]; then
	echo "building $bin"
	(cd "$root" && cargo build -p retype-cli) || exit 1
fi

trees=()
watcher_pid=
daemon_pid=
tree=

cleanup() {
	[ -n "$watcher_pid" ] && kill "$watcher_pid" 2>/dev/null
	[ -n "$daemon_pid" ] && kill "$daemon_pid" 2>/dev/null
	if [ "$keep" = 1 ]; then
		[ ${#trees[@]} -gt 0 ] && printf '\ntrees kept:\n%s\n' "${trees[*]}"
		return
	fi

	[ ${#trees[@]} -gt 0 ] || return

	for kept in "${trees[@]}"; do
		"$bin" daemon stop --now --state-dir "$kept/state" >/dev/null 2>&1
		rm -rf "$kept"
	done
}

trap cleanup EXIT

say() {
	printf '  %-12s %s\n' "$1" "$2"
}

now_ms() {
	if command -v python3 >/dev/null 2>&1; then
		python3 -c 'import time; print(int(time.time() * 1000))'
	else
		echo $(($(date +%s) * 1000))
	fi
}

resolve() {
	if command -v python3 >/dev/null 2>&1; then
		python3 -c 'import os, sys; print(os.path.realpath(sys.argv[1]))' "$1"
	else
		echo "$1"
	fi
}

# Device and inode, the identity the correlation uses on Unix.
inode() {
	case $(uname -s) in
	Darwin) stat -f '%d:%i' "$1" 2>/dev/null ;;
	*) stat -c '%d:%i' "$1" 2>/dev/null ;;
	esac
}

observed() {
	"$bin" daemon status --json --state-dir "$1/state" 2>/dev/null |
		sed -n 's/.*"observed":[[:space:]]*\([0-9][0-9]*\).*/\1/p'
}

# Wait until the watcher has looked at one more path than `before`, so a file
# written after the snapshot is known by identity before it is renamed: macOS
# reports a create and a rename inside one window as a create, which carries no
# pair to correlate.
wait_growth() {
	local tree=$1 before=$2 limit=${3:-20} deadline now
	deadline=$((SECONDS + limit))

	while :; do
		now=$(observed "$tree")
		[ -n "$now" ] && [ "$now" -gt "$before" ] && return 0
		[ $SECONDS -ge $deadline ] && return 1
		sleep 0.05
	done
}

wait_fact() {
	local log=$1 pattern=$2 limit=${3:-10} deadline
	deadline=$((SECONDS + limit))

	while ! grep -Eq -- "$pattern" "$log" 2>/dev/null; do
		[ $SECONDS -ge $deadline ] && return 1
		sleep 0.05
	done
}

facts() {
	grep -o 'rename [^ ]* -> [^ ]*' "$1" 2>/dev/null | sed "s|$2/||g"
}

journal_states() {
	grep -o '"state":"[a-z]*"' "$1/state/jobs.jsonl" 2>/dev/null |
		sed 's/.*"state":"\([a-z]*\)".*/\1/' | sort | uniq -c | awk '{printf "%s %s  ", $1, $2}'
}

setup() {
	TREE=$(mktemp -d "${TMPDIR:-/tmp}/retype-probe.XXXXXX")
	mkdir -p "$TREE/photos" "$TREE/state"
	trees+=("$TREE")
	tree=$TREE

	cat >"$tree/slow.sh" <<'SCRIPT'
#!/bin/sh
sleep 4
cp "$1" "$2"
SCRIPT
	chmod +x "$tree/slow.sh"

	cat >"$TREE/retype.toml" <<CONFIG
version = 'retype-config-1'

[notify]
enabled = false

[backends]
prefer = ['slow']

[tools.slow]
command = '$TREE/slow.sh'
layout = ['{input}', '{ops}', '{output}']

[formats.png]
family = 'image'
lossy = false
alpha = true
tier = 'exact'

[formats.jpg]
family = 'image'
lossy = true
alpha = false
tier = 'converted'

[formats.webp]
family = 'image'
lossy = true
alpha = true
tier = 'converted'
CONFIG
}

start() {
	local tree=$1 deadline
	RETYPE_CONFIG="$tree/retype.toml" "$bin" watch "$tree/photos" --state-dir "$tree/state" \
		>>"$tree/daemon.log" 2>&1 || return 1
	RETYPE_CONFIG="$tree/retype.toml" RETYPE_LOG=debug "$bin" daemon --detached --state-dir "$tree/state" \
		>>"$tree/daemon.log" 2>&1 &
	daemon_pid=$!

	deadline=$((SECONDS + 20))
	while [ ! -e "$tree/state/endpoint" ]; do
		[ $SECONDS -ge $deadline ] && return 1
		sleep 0.05
	done

	# The supervisor picks a registration up within a second, so a fact is only
	# worth reading after that.
	sleep 1.5
}

# An independent watcher, so "not delivered" can be told from "we dropped it".
watch_independently() {
	local tree=$1
	watcher_pid=

	case $(uname -s) in
	Darwin)
		command -v fswatch >/dev/null 2>&1 || return 1
		fswatch -x -l 0.05 "$tree/photos" >"$tree/events.txt" 2>/dev/null &
		;;
	Linux)
		command -v inotifywait >/dev/null 2>&1 || return 1
		inotifywait -m -r -e create,moved_to,moved_from,delete,close_write "$tree/photos" \
			>"$tree/events.txt" 2>/dev/null &
		;;
	*) return 1 ;;
	esac

	watcher_pid=$!
	sleep 0.5
}

stop_watching_independently() {
	[ -n "$watcher_pid" ] && kill "$watcher_pid" 2>/dev/null
	watcher_pid=
	sleep 0.2
}

# Renames with no job attached: unknown formats, so nothing but the watcher acts.
trial_renames() {
	local tree log photos mark from to pair
	setup
	tree=$TREE
	log=$tree/daemon.log
	photos=$tree/photos

	start "$tree" || {
		say "start" "the daemon did not start"
		return 1
	}

	for pair in a.aaa:a.bbb b.aaa:b.bbb; do
		from=${pair%%:*}
		to=${pair##*:}
		mark=$(observed "$tree")
		printf 'pixels' >"$photos/$from"

		if ! wait_growth "$tree" "$mark"; then
			say "$from" "the watcher never looked at it"
			continue
		fi

		mv "$photos/$from" "$photos/$to"

		if wait_fact "$log" "rename .*${from} -> .*${to}" 10; then
			say "$from -> $to" "delivered, a fact"
		else
			say "$from -> $to" "NOT delivered: no fact"
		fi
	done

	# The same file twice, which is the shape a supersession chains.
	mark=$(observed "$tree")
	printf 'pixels' >"$photos/c.aaa"
	wait_growth "$tree" "$mark" || say "c.aaa" "the watcher never looked at it"
	mv "$photos/c.aaa" "$photos/c.bbb"

	if wait_fact "$log" 'rename .*c\.aaa -> .*c\.bbb' 10; then
		say "c.aaa -> c.bbb" "first rename delivered"
	else
		say "c.aaa -> c.bbb" "first rename NOT delivered"
	fi

	mv "$photos/c.bbb" "$photos/c.ccc"

	if wait_fact "$log" 'rename .*c\.bbb -> .*c\.ccc' 10; then
		say "c.bbb -> c.ccc" "second rename delivered"
	else
		say "c.bbb -> c.ccc" "second rename NOT delivered"
	fi

	say "facts" "$(facts "$log" "$photos" | wc -l | tr -d ' ')"
}

# The supersession the test expects, with every stage reported.
trial_supersede() {
	local tree log photos state mark deadline png webp jpg
	local have_fact=no saw_watcher=no
	setup
	tree=$TREE
	log=$tree/daemon.log
	state=$tree/state
	photos=$tree/photos

	if [ "${CANONICAL:-0}" = 1 ]; then
		photos=$(resolve "$photos")
	fi

	png=$photos/photo.png
	webp=$photos/photo.webp
	jpg=$photos/photo.jpg

	start "$tree" || {
		say "start" "the daemon did not start"
		return 1
	}

	watch_independently "$tree" && saw_watcher=yes || saw_watcher=no

	mark=$(observed "$tree")
	printf 'pixels' >"$png"
	wait_growth "$tree" "$mark" || say "create" "the watcher never looked at the file"

	say "inode" "$(inode "$png") at photo.png"

	mark=$(observed "$tree")
	mv "$png" "$webp"
	say "inode" "$(inode "$webp") at photo.webp"

	if wait_fact "$log" 'rename .*photo\.png -> .*photo\.webp' 10; then
		say "first fact" "photo.png -> photo.webp"
	else
		say "first fact" "MISSING (delivery)"
	fi

	# The job restores the source name while it runs, which is the name a person
	# renames next, `docs/dev/architecture/state.md` §5.4.
	deadline=$((SECONDS + 20))
	while [ ! -e "$png" ] || [ -e "$webp" ]; do
		if [ $SECONDS -ge $deadline ]; then
			say "restore" "the source name never came back"
			break
		fi
		sleep 0.05
	done
	[ -e "$png" ] && say "restore" "photo.png again, inode $(inode "$png")"

	say "journal" "$(journal_states "$tree")"

	local renamed_at after
	renamed_at=$(now_ms)
	mv "$png" "$jpg"
	wait_fact "$log" 'rename .*photo\.jpg' 10

	say "inode" "$(inode "$jpg") at photo.jpg"

	# Whatever came out, not only the fact the test expects: a fact that starts at
	# the name we restored is our own rename, and it should not chain.
	after=$(facts "$log" "$photos" | tail -n +2 | tr '\n' ';')
	[ -n "$after" ] || after=none
	say "facts after" "$after"
	grep -q 'photo\.png -> .*photo\.jpg' <<<"$after" && have_fact=yes

	local batched=no
	grep -q 'batch:.*photo\.jpg' "$log" && batched=yes
	say "batching" "$([ "$batched" = yes ] && echo 'a batch carried photo.jpg' || echo 'no batch names photo.jpg')"

	local holders
	holders=$(grep -o '[0-9]* job(s) hold [^ ]* while [^ ]* is decided' "$log" | tail -1)
	[ -n "$holders" ] && say "holders" "$holders"

	# Give the background side time to finish both records.
	deadline=$((SECONDS + 20))
	while [ $SECONDS -lt $deadline ]; do
		grep -q '"state":"superseded"' "$state/jobs.jsonl" 2>/dev/null && break
		sleep 0.1
	done

	# The test expects the chain to finish, not only the first job to be replaced.
	local done=no
	deadline=$((SECONDS + 30))
	while [ $SECONDS -lt $deadline ]; do
		grep -q '"state":"done"' "$state/jobs.jsonl" 2>/dev/null && done=yes && break
		sleep 0.1
	done
	say "chain" "$([ "$done" = yes ] && echo 'the chained job finished' || echo 'no done record')"

	say "journal" "$(journal_states "$tree")"

	if grep -q '"code":"E_TOOL_FAILED"' "$state/jobs.jsonl" 2>/dev/null; then
		say "tool" "ran after the source name was gone"
	fi

	if [ "$saw_watcher" = yes ]; then
		stop_watching_independently
		if grep -q 'photo\.jpg' "$tree/events.txt" 2>/dev/null; then
			say "platform" "an independent watcher saw photo.jpg"
		else
			say "platform" "an independent watcher did NOT see photo.jpg"
		fi
	else
		say "platform" "skipped: install fswatch (macOS) or inotifywait (Linux)"
	fi

	local superseded=no
	grep -q '"state":"superseded"' "$state/jobs.jsonl" 2>/dev/null && superseded=yes

	if [ "$superseded" = yes ]; then
		say "verdict" "pass: the job was superseded"
	elif [ "$have_fact" = yes ] && grep -q '^0 job(s) hold' <<<"$holders"; then
		say "verdict" "producers: a fact exists, no job holds photo.png (path matching)"
	elif [ "$have_fact" = yes ]; then
		say "verdict" "cancellation: a job holds photo.png and was not stopped"
	elif [ "$batched" = yes ]; then
		say "verdict" "correlation: the batch carried photo.jpg, no fact came out"
	else
		say "verdict" "delivery: no batch named photo.jpg"
	fi

	# The window matters: a rename after the job ended cannot supersede it.
	say "renamed" "$renamed_at ms after epoch"
}

main() {
	printf 'probe: supersede\n'
	printf '  %-12s %s\n' "host" "$(uname -srm)"
	printf '  %-12s %s (%s)\n' "binary" "$bin" "$(cd "$root" && git rev-parse --short HEAD 2>/dev/null)"
	printf '  %-12s %s\n' "watcher" "$(command -v fswatch || command -v inotifywait || echo none)"

	for trial in "${trials[@]}"; do
		printf '\ntrial: %s\n' "$trial"

		case $trial in
		renames) trial_renames ;;
		supersede) CANONICAL=0 trial_supersede ;;
		canonical) CANONICAL=1 trial_supersede ;;
		*)
			echo "unknown trial $trial" >&2
			exit 2
			;;
		esac

		"$bin" daemon stop --now --state-dir "$TREE/state" >/dev/null 2>&1
		wait "$daemon_pid" 2>/dev/null
		daemon_pid=
	done
}

main
