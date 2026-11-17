#!/usr/bin/env bash
# Install external test tools with existing package managers; never bootstrap a package manager.
set -euo pipefail

if [ "${GITHUB_ACTIONS:-}" = true ]; then
	: "${GITHUB_PATH:?GitHub Actions path file is required}"
fi

case "$(uname -s)" in
Linux)
	if ! command -v apt-get >/dev/null 2>&1; then
		printf 'Automatic Linux preparation targets current Debian stable; see tests/README.md.\n' >&2
		exit 2
	fi
	apt=(apt-get)
	if [ "$(id -u)" -ne 0 ]; then apt=(sudo apt-get); fi
	"${apt[@]}" update -y
	"${apt[@]}" install -y --no-install-recommends imagemagick ffmpeg pandoc 7zip fonts-dejavu-core
	for tool in ffmpeg ffprobe; do
		path=$(command -v "$tool")
		owner=$(dpkg-query -S "$path")
		case "$owner" in
			'ffmpeg: '*) printf '%s\n' "$owner" ;;
			*) printf 'Unexpected package for %s: %s\n' "$path" "$owner" >&2; exit 1 ;;
		esac
	done
	printf 'Debian converters installed. PDF tests also need a compatible engine; see docs/advanced/converters.md#older-pandoc-and-typst.\n'
	printf 'Run ./tests/run.sh --check.\n'
	;;
Darwin)
	if ! command -v brew >/dev/null 2>&1; then
		printf 'Install Homebrew first, or install the prerequisites manually: see tests/README.md.\n' >&2
		exit 2
	fi
	export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 HOMEBREW_NO_ANALYTICS=1
	brew install imagemagick ffmpeg pandoc typst sevenzip
	prefix=$(brew --prefix)
	if [ "${GITHUB_ACTIONS:-}" = true ]; then printf '%s/bin\n' "$prefix" >> "$GITHUB_PATH"; fi
	printf 'Tools installed in %s/bin. Ensure it is on PATH, then run ./tests/run.sh --check.\n' "$prefix"
	;;
*)
	printf 'Unsupported CI setup platform: %s\n' "$(uname -s)" >&2
	exit 2
	;;
esac
