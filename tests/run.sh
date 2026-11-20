#!/usr/bin/env bash
# Native Linux/macOS validation. Full command output stays in the report directory.
set -euo pipefail

check_only=0
case "${1:-}" in
	'') [ "$#" -eq 0 ] || { printf 'Usage: run.sh [--check]\n' >&2; exit 2; } ;;
	--check) [ "$#" -eq 1 ] || { printf 'Usage: run.sh [--check]\n' >&2; exit 2; }; check_only=1 ;;
	-h|--help) printf 'Usage: tests/run.sh [--check]\n'; exit 0 ;;
	*) printf 'Usage: tests/run.sh [--check]\n' >&2; exit 2 ;;
esac
platform=$(uname -s)
case "$platform" in Linux|Darwin) ;; *) printf 'Unsupported native test host: %s\n' "$platform" >&2; exit 2;; esac
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
root=$(cd -- "$script_dir/.." && pwd -P)
cd "$root"

for command in git cargo rustc tee mktemp ps cat grep tail; do
	if ! command -v "$command" >/dev/null 2>&1; then
		printf 'Required command is missing: %s\n' "$command" >&2
		exit 2
	fi
done
if [ "$platform" = Darwin ]; then
	[ -x /usr/bin/perl ] || { printf 'System Perl is required at /usr/bin/perl.\n' >&2; exit 2; }
elif ! command -v timeout >/dev/null 2>&1; then
	printf 'GNU timeout is required.\n' >&2
	exit 2
fi
bounded() {
	local seconds=$1 grace=$2
	shift 2
	if [ "$platform" = Darwin ]; then
		/usr/bin/perl "$script_dir/support/run-bounded.pl" "$seconds" "$grace" "$@" </dev/null
	else
		timeout --kill-after="${grace}s" "${seconds}s" "$@" </dev/null
	fi
}

limit=${RETYPE_TEST_TIMEOUT_SECONDS:-300}
case "$limit" in ''|*[!0-9]*) printf 'RETYPE_TEST_TIMEOUT_SECONDS must be a positive integer.\n' >&2; exit 2;; esac
if [ "${#limit}" -gt 4 ] || [ "$limit" -lt 1 ] || [ "$limit" -gt 3600 ]; then
	printf 'RETYPE_TEST_TIMEOUT_SECONDS must be between 1 and 3600.\n' >&2
	exit 2
fi
umask 077
report_base=${RETYPE_TEST_REPORT_DIR:-"$root/target/test-reports"}
scratch_base=${RETYPE_TEST_TEMP_DIR:-/tmp}
case "$report_base" in /*) ;; *) printf 'RETYPE_TEST_REPORT_DIR must be absolute.\n' >&2; exit 2;; esac
case "$scratch_base" in /*) ;; *) printf 'RETYPE_TEST_TEMP_DIR must be absolute.\n' >&2; exit 2;; esac
if [ ! -d "$scratch_base" ] || [ ! -w "$scratch_base" ]; then
	printf 'Invalid scratch directory: %s\n' "$scratch_base" >&2
	exit 2
fi
mkdir -p "$report_base"
report=$(mktemp -d "$report_base/$platform-$(date -u '+%Y%m%dT%H%M%SZ').XXXXXXXX")
scratch=$(mktemp -d "$scratch_base/retype-test.XXXXXXXX")
scratch=$(cd -- "$scratch" && pwd -P)
printf 'Native test: %s\nReports: %s\nScratch retained: %s\n' "$platform" "$report" "$scratch"
if [ "${#scratch}" -gt 56 ]; then
	printf 'Temporary path is too long for Unix sockets: %s\nSet RETYPE_TEST_TEMP_DIR to a shorter path.\n' "$scratch" >&2
	exit 2
fi

printf "version = 'retype-config-1'\n[notify]\nenabled = false\n" > "$scratch/retype.toml"
export TMPDIR="$scratch" XDG_CONFIG_HOME="$scratch/config" XDG_STATE_HOME="$scratch/state" XDG_DATA_HOME="$scratch/data"
export RETYPE_CONFIG="$scratch/retype.toml" RETYPE_REQUIRE_TEST_TOOLS=1 RETYPE_REQUIRE_NATIVE_WORKFLOW=1
# Debug integration binaries use a controlled login; this is not real-session evidence.
export RETYPE_TEST_SESSION=terminal
unset RETYPE_STATE_DIR RETYPE_NO_SPAWN RETYPE_DAEMON RETYPE_FIXTURES RETYPE_FIDELITY_MANIFEST
unset RETYPE_TEST_RECOVERY_ROOT RETYPE_TEST_RECOVERY_TOKEN RETYPE_TEST_RECOVERY_CHECKPOINT
unset NEXTEST_RETRIES NEXTEST_TEST_THREADS NEXTEST_MESSAGE_FORMAT
failures=0
cleaned=0
step_code=0
summary() { printf '%s\n' "$*" | tee -a "$report/summary.txt"; }

cleanup() {
	if [ "$cleaned" -eq 1 ]; then return; fi
	cleaned=1
	local pid command executable current deadline listing
	deadline=$((SECONDS + 15))
	if ! listing=$(bounded 5 2 ps -ww -axo pid=,command=); then
		summary 'Could not inspect processes for test cleanup.'
		failures=1
		return
	fi
	while read -r pid command; do
		case "$command" in *"$scratch"/*) ;; *) continue;; esac
		case "$pid" in ''|*[!0-9]*) continue;; esac
		[ "$pid" != "$$" ] || continue
		executable=$(ps -p "$pid" -o comm= 2>/dev/null || true)
		executable=${executable##*/}
		case "$executable" in
			retype|magick|ffmpeg|ffprobe|pandoc|7z|7zz|7za|typst|xelatex|lualatex|pdflatex|weasyprint|wkhtmltopdf|sh|bash) ;;
			*) continue;;
		esac
		current=$(ps -p "$pid" -o command= 2>/dev/null || true)
		case "$current" in *"$scratch"/*) ;; *) continue;; esac
		summary "Stopping leaked test process $pid ($executable)."
		failures=1
		kill -TERM "$pid" 2>/dev/null || true
		while kill -0 "$pid" 2>/dev/null && [ "$SECONDS" -lt "$deadline" ]; do
			current=$(ps -p "$pid" -o command= 2>/dev/null || true)
			case "$current" in *"$scratch"/*) ;; *) break;; esac
			sleep 0.1
		done
		current=$(ps -p "$pid" -o command= 2>/dev/null || true)
		case "$current" in *"$scratch"/*) kill -KILL "$pid" 2>/dev/null || true;; esac
		if [ "$SECONDS" -ge "$deadline" ]; then
			summary "Cleanup deadline reached; inspect $scratch for remaining test processes."
			break
		fi
	done <<< "$listing"
}
finish() {
	local status=$1
	trap - EXIT
	cleanup
	if [ "$failures" -ne 0 ] || [ "$status" -ne 0 ]; then
		summary 'Result: failed'
		printf 'See %s/summary.txt and the full logs. Scratch: %s\n' "$report" "$scratch" >&2
		[ "$status" -ne 0 ] && exit "$status"
		exit 1
	fi
	summary 'Result: passed'
	if [ "$check_only" -eq 1 ]; then
		printf 'Prerequisite probes passed; this is not conversion or undo evidence.\n'
	else
		printf 'Native validation passed.\n'
	fi
}
trap 'finish "$?"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

probe() {
	local name=$1 label="$*" code=0
	printf '\n$ %s\n' "$label" >> "$report/metadata.log"
	if ! command -v "$name" >/dev/null 2>&1; then
		printf 'Not installed: %s\n' "$name" >> "$report/metadata.log"
		summary "probe $label: command not found"
		failures=1
		return
	fi
	bounded 10 3 "$@" > "$scratch/probe.stdout" 2> "$scratch/probe.stderr" || code=$?
	cat "$scratch/probe.stdout" "$scratch/probe.stderr" >> "$report/metadata.log"
	summary "probe $label: exit $code"
	if [ "$code" -ne 0 ]; then failures=1; fi
	if [ "$label" = 'magick -list format' ] && ! grep -q '[^[:space:]]' "$scratch/probe.stdout"; then
		summary 'probe magick -list format: empty format listing'
		failures=1
	fi
}
run_step() {
	local name=$1 seconds=$2 started=$SECONDS
	shift 2
	printf '\n== %s (limit %ss); log: %s/%s.log ==\n' "$name" "$seconds" "$report" "$name"
	step_code=0
	bounded "$seconds" 5 "$@" > "$report/$name.log" 2>&1 || step_code=$?
	summary "$name: command exit $step_code; limit ${seconds}s; elapsed $((SECONDS - started))s; log: $name.log"
	if [ "$step_code" -ne 0 ]; then
		failures=1
		tail -n 20 "$report/$name.log"
	fi
}
run_suite_step() {
	local name=$1 remaining=$((limit - (SECONDS - suite_started)))
	shift
	if [ "$remaining" -le 0 ]; then
		summary "$name: not run; shared ${limit}s test-suite budget exhausted"
		printf 'Not run: suite budget exhausted.\n' > "$report/$name.log"
		step_code=124
		failures=1
	else
		run_step "$name" "$remaining" "$@"
	fi
}

summary "Platform: $platform; per-step and shared test-suite limit: ${limit}s"
printf 'Host: %s\nScratch: %s\n' "$(uname -a)" "$scratch" > "$report/metadata.log"
probe git rev-parse --verify HEAD
probe git status --short
bounded 10 3 git diff --binary --no-ext-diff --no-textconv HEAD > "$report/worktree.diff" 2>&1 || failures=1
probe rustc -vV
probe cargo --version
probe cargo nextest show-config version --config-file tests/nextest.toml --user-config-file none
probe magick -version
probe magick -list format
probe magick fixtures/native-watch/photo.heic null:
probe ffmpeg -version
probe ffmpeg -hide_banner -encoders
probe ffprobe -version
probe pandoc --version
engine_found=0
for engine in typst xelatex lualatex pdflatex weasyprint wkhtmltopdf; do
	if command -v "$engine" >/dev/null 2>&1; then probe "$engine" --version; engine_found=1; fi
done
if [ "$engine_found" -eq 0 ]; then summary 'No PDF engine found'; failures=1; fi
archiver_found=0
for archiver in 7zz 7z 7za; do
	if command -v "$archiver" >/dev/null 2>&1; then probe "$archiver" i; archiver_found=1; break; fi
done
if [ "$archiver_found" -eq 0 ]; then summary 'No 7-Zip executable found'; failures=1; fi
if [ "$check_only" -eq 1 ]; then exit "$failures"; fi

run_step format 60 cargo fmt --all -- --check
run_step clippy "$limit" cargo clippy --workspace --all-targets --locked -- -D warnings
run_step installer-syntax 60 bash -c 'for file in packaging/install.sh tests/*.sh; do bash -n "$file" || exit; done'
run_step integration-syntax 60 python3 -c \
	'import ast, pathlib; ast.parse(pathlib.Path("packaging/linux/integrate.py").read_text())'
run_step installer "$limit" bash tests/install.sh
if grep -i 'skipping installer test' "$report/installer.log" >/dev/null; then
	summary 'The installer test did not run on this host.'
	failures=1
fi
run_step build "$limit" cargo test --workspace --locked --no-run
if [ "$step_code" -ne 0 ]; then summary 'Workspace and doctests not run: build failed'; exit 1; fi
run_step recovery-build "$limit" cargo test -p retype-cli --locked --features test-checkpoints \
	--test daemon_recovery --target-dir target/test-checkpoints --no-run
if [ "$step_code" -ne 0 ]; then summary 'Daemon recovery tests not run: build failed'; exit 1; fi

suite_started=$SECONDS
run_suite_step workspace cargo nextest run --workspace --locked --config-file tests/nextest.toml \
	--profile platform --user-config-file none --ignore-default-filter --run-ignored all \
	--status-level pass --color never --success-output immediate --failure-output immediate --no-tests fail
run_suite_step daemon-recovery cargo nextest run -p retype-cli --locked --features test-checkpoints \
	--test daemon_recovery --target-dir target/test-checkpoints --config-file tests/nextest.toml \
	--profile platform --user-config-file none --ignore-default-filter --run-ignored all \
	--status-level pass --color never --success-output immediate --failure-output immediate --no-tests fail
run_suite_step doctests cargo test --workspace --doc --locked --no-fail-fast -- --nocapture
if ! grep -E 'Summary .* 14 tests run: 14 passed' "$report/daemon-recovery.log" >/dev/null; then
	summary 'The daemon interruption matrix did not pass all 14 tests.'
	failures=1
fi
for workflow in \
	a_real_watched_heic_conversion_and_undo_preserve_the_original \
	a_real_direct_heic_conversion_and_undo_follow_watched_disposition; do
	if ! grep -E "PASS[[:space:]]+\\[.*\\].*${workflow}([[:space:]]|$)" "$report/workspace.log" >/dev/null; then
		summary "The strict native workflow test did not pass: $workflow"
		failures=1
	fi
done
if ! grep -E 'Summary .* [1-9][0-9]* passed' "$report/workspace.log" >/dev/null; then
	summary 'No passing nextest summary was recorded.'
	failures=1
fi
if ! grep -F 'Doc-tests' "$report/doctests.log" >/dev/null ||
	! grep -F 'test result:' "$report/doctests.log" >/dev/null; then
	summary 'No doctest summaries were recorded.'
	failures=1
fi
if grep -E 'SKIPPED:|so this test proves nothing|[1-9][0-9]* ignored|Summary .* [1-9][0-9]* skipped' \
	"$report/workspace.log" "$report/daemon-recovery.log" "$report/doctests.log" >/dev/null; then
	summary 'Test output contains a skip or ignored test; release evidence is incomplete.'
	failures=1
fi
if grep -E '^[[:space:]]*(FAIL|TIMEOUT|LEAK)[[:space:]]+\[|test result: FAILED|^error: test failed' \
	"$report/workspace.log" "$report/daemon-recovery.log" "$report/doctests.log" >/dev/null; then
	summary 'Test output contains a failed or timed-out result.'
	failures=1
fi
