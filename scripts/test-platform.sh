#!/usr/bin/env bash
# Shared native runner for the Linux and macOS entry points.
set -euo pipefail

platform=${1:-}
if [ "$#" -ne 1 ] || { [ "$platform" != Linux ] && [ "$platform" != Darwin ]; }; then
	printf 'Usage: test-platform.sh Linux|Darwin\n' >&2
	exit 2
fi
if [ "$(uname -s)" != "$platform" ]; then
	printf 'Run this script on %s, not %s.\n' "$platform" "$(uname -s)" >&2
	exit 2
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
root=$(cd -- "$script_dir/.." && pwd -P)
cd "$root"

for command in git cargo rustc tee mktemp ps; do
	if ! command -v "$command" >/dev/null 2>&1; then
		printf 'Required command is missing: %s\n' "$command" >&2
		exit 2
	fi
done
if [ "$platform" = Darwin ]; then
	if [ ! -x /usr/bin/perl ]; then
		printf 'The macOS runner requires the system Perl at /usr/bin/perl.\n' >&2
		exit 2
	fi
elif ! command -v timeout >/dev/null 2>&1; then
	printf 'Required command is missing: timeout\n' >&2
	exit 2
fi

bounded() {
	local seconds=$1 grace=$2
	shift 2
	if [ "$platform" = Darwin ]; then
		/usr/bin/perl "$script_dir/run-bounded.pl" "$seconds" "$grace" "$@"
	else
		timeout --kill-after="${grace}s" "${seconds}s" "$@"
	fi
}

if ! revision=$(bounded 10 3 git rev-parse --verify HEAD); then
	printf 'Cannot determine the Git revision from %s.\n' "$root" >&2
	exit 2
fi

limit=${RETYPE_TEST_TIMEOUT_SECONDS:-300}
case "$limit" in ''|*[!0-9]*) printf 'RETYPE_TEST_TIMEOUT_SECONDS must be a positive integer.\n' >&2; exit 2;; esac
if [ "${#limit}" -gt 4 ] || [ "$limit" -lt 1 ] || [ "$limit" -gt 3600 ]; then
	printf 'RETYPE_TEST_TIMEOUT_SECONDS must be between 1 and 3600.\n' >&2
	exit 2
fi

umask 077
report_base=${RETYPE_TEST_REPORT_DIR:-"$root/target/platform-test-reports"}
scratch_base=${RETYPE_TEST_TEMP_DIR:-/tmp}
case "$report_base" in /*) ;; *) printf 'RETYPE_TEST_REPORT_DIR must be absolute.\n' >&2; exit 2;; esac
case "$scratch_base" in /*) ;; *) printf 'RETYPE_TEST_TEMP_DIR must be absolute.\n' >&2; exit 2;; esac
if [ ! -d "$scratch_base" ] || [ ! -w "$scratch_base" ]; then
	printf 'Test temporary directory is not writable: %s\n' "$scratch_base" >&2
	exit 2
fi
mkdir -p "$report_base"
stamp=$(date -u '+%Y%m%dT%H%M%SZ')
report=$(mktemp -d "$report_base/$platform-$stamp.XXXXXXXX")
scratch=$(mktemp -d "$scratch_base/retype-test.XXXXXXXX")
scratch=$(cd -- "$scratch" && pwd -P)
printf 'Platform test: %s\nReports: %s\nScratch retained: %s\n' "$platform" "$report" "$scratch"
if [ "${#scratch}" -gt 56 ]; then
	printf 'Temporary path is too long for Unix sockets: %s\nSet RETYPE_TEST_TEMP_DIR to a shorter path.\n' "$scratch" >&2
	printf 'Report: %s\nScratch retained: %s\n' "$report" "$scratch" >&2
	exit 2
fi

printf "version = 'retype-config-1'\n[notify]\nenabled = false\n" > "$scratch/retype.toml"
export TMPDIR="$scratch"
export XDG_CONFIG_HOME="$scratch/config"
export XDG_STATE_HOME="$scratch/state"
export XDG_DATA_HOME="$scratch/data"
export RETYPE_CONFIG="$scratch/retype.toml"
export RETYPE_REQUIRE_TEST_TOOLS=1
export RETYPE_REQUIRE_NATIVE_WORKFLOW=1
unset RETYPE_STATE_DIR RETYPE_NO_SPAWN

failures=0
cleaned=0
cleanup() {
	if [ "$cleaned" -eq 1 ]; then return; fi
	cleaned=1
	local pid command executable current deadline listing
	deadline=$((SECONDS + 15))
	if ! listing=$(bounded 5 2 ps -ww -axo pid=,command=); then
		printf 'Could not inspect processes for test cleanup.\n' | tee -a "$report/cleanup.log" "$report/summary.txt"
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
			retype|magick|ffmpeg|ffprobe|pandoc|7z|7zz|7za|typst|xelatex|lualatex|pdflatex|\
			weasyprint|wkhtmltopdf|sh|bash) ;;
			*) continue;;
		esac
		current=$(ps -p "$pid" -o command= 2>/dev/null || true)
		case "$current" in *"$scratch"/*) ;; *) continue;; esac
		printf 'Stopping leaked test process %s (%s).\n' "$pid" "$executable" |
			tee -a "$report/cleanup.log" "$report/summary.txt"
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
			printf 'Cleanup deadline reached; inspect %s for remaining test processes.\n' "$scratch" |
				tee -a "$report/cleanup.log" "$report/summary.txt"
			failures=1
			break
		fi
	done <<< "$listing"
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

probe() {
	local name=$1
	shift
	printf '\n$ %s' "$name"
	printf ' %s' "$@"
	printf '\n'
	if ! command -v "$name" >/dev/null 2>&1; then
		printf 'Not installed\n'
	elif ! bounded 10 3 "$name" "$@" 2>&1; then
		printf 'Probe failed or timed out\n'
	fi
}
{
	printf 'Platform: %s\nRevision: %s\n' "$platform" "$revision"
	printf '\nWorking tree:\n'
	if ! bounded 10 3 git status --short; then
		printf 'Could not inspect working tree\n' | tee -a "$report/summary.txt"
		failures=1
	fi
	printf '\nHost:\n'
	uname -a
	df -h "$scratch"
	probe rustc -vV
	probe cargo --version
	probe magick -version
	probe magick -list format
	probe ffmpeg -version
	probe ffmpeg -hide_banner -encoders
	probe ffprobe -version
	probe pandoc --version
	engine_found=0
	for engine in typst xelatex lualatex pdflatex weasyprint wkhtmltopdf; do
		if command -v "$engine" >/dev/null 2>&1; then probe "$engine" --version; engine_found=1; fi
	done
	if [ "$engine_found" -eq 0 ]; then printf '\nNo PDF engine found\n'; fi
	archiver_found=0
	for archiver in 7zz 7z 7za; do
		if command -v "$archiver" >/dev/null 2>&1; then probe "$archiver" i; archiver_found=1; break; fi
	done
	if [ "$archiver_found" -eq 0 ]; then printf '\nNo 7-Zip executable found\n'; fi
} > "$report/metadata.log" 2>&1

run_step() {
	local name=$1
	local seconds=$2
	shift 2
	printf '\n== %s (timeout %ss) ==\n' "$name" "$seconds"
	set +e
	bounded "$seconds" 5 "$@" 2>&1 | tee "$report/$name.log"
	local statuses=("${PIPESTATUS[@]}")
	set -e
	printf '%s: command exit %s, log exit %s\n' "$name" "${statuses[0]}" "${statuses[1]}" | tee -a "$report/summary.txt"
	if [ "${statuses[0]}" -ne 0 ] || [ "${statuses[1]}" -ne 0 ]; then failures=1; fi
}

printf 'The strict native workflow is enabled; it currently fails where the source is retained or undo is incomplete.\n'
run_step format 60 cargo fmt --all -- --check
run_step clippy "$limit" cargo clippy --workspace --all-targets --locked -- -D warnings
run_step installer-syntax 60 bash -n packaging/install.sh scripts/test-install.sh
run_step installer "$limit" bash scripts/test-install.sh
if grep -i 'skipping installer test' "$report/installer.log" >/dev/null; then
	printf 'The installer test did not run on this host.\n' | tee -a "$report/summary.txt"
	failures=1
fi
run_step workspace "$limit" cargo test --workspace --locked --no-fail-fast -- --nocapture
for workflow in \
	a_real_watched_heic_conversion_and_undo_preserve_the_original \
	a_real_direct_heic_conversion_and_undo_follow_watched_disposition; do
	if ! grep -F "$workflow" "$report/workspace.log" >/dev/null; then
		printf 'The strict native workflow test did not run: %s\n' "$workflow" | tee -a "$report/summary.txt"
		failures=1
	fi
done
if ! grep -E 'test result: (ok|FAILED)\. [1-9][0-9]* passed' "$report/workspace.log" >/dev/null; then
	printf 'No passing test summary was recorded.\n' | tee -a "$report/summary.txt"
	failures=1
fi
if grep -E 'SKIPPED:|so this test proves nothing|[1-9][0-9]* ignored' "$report/workspace.log" >/dev/null; then
	printf 'Workspace output contains a skip or ignored test; release evidence is incomplete.\n' |
		tee -a "$report/summary.txt"
	failures=1
fi
if grep -E '^test result: FAILED|^error: test failed' "$report/workspace.log" >/dev/null; then
	printf 'Workspace output contains a failed test summary.\n' | tee -a "$report/summary.txt"
	failures=1
fi
cleanup
printf '\nReports: %s\nScratch retained: %s\n' "$report" "$scratch"
if [ "$failures" -ne 0 ]; then
	printf 'Platform validation failed. See %s/summary.txt and the step logs.\n' "$report" >&2
	exit 1
fi
printf 'Platform validation passed.\n'
