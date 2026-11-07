#!/usr/bin/env bash
# Provision real tools on disposable GitHub-hosted Unix runners.
set -euo pipefail

if [ "${GITHUB_ACTIONS:-}" != true ]; then
	printf 'This setup is intended for GitHub-hosted CI.\n' >&2
	exit 2
fi
: "${GITHUB_PATH:?GitHub Actions path file is required}"

export HOMEBREW_NO_AUTO_UPDATE=1
export HOMEBREW_NO_INSTALL_CLEANUP=1
export HOMEBREW_NO_ANALYTICS=1

case "$(uname -s)" in
Linux)
	sudo apt-get update -y
	sudo apt-get install -y --no-install-recommends ffmpeg
	brew install imagemagick pandoc typst sevenzip
	for tool in ffmpeg ffprobe; do
		path=$(command -v "$tool")
		owner=$(dpkg-query -S "$path")
		case "$owner" in
			'ffmpeg: '*) printf '%s\n' "$owner" ;;
			*) printf 'Unexpected package for %s: %s\n' "$path" "$owner" >&2; exit 1 ;;
		esac
	done
	;;
Darwin)
	brew install imagemagick ffmpeg pandoc typst sevenzip
	;;
*)
	printf 'Unsupported CI setup platform: %s\n' "$(uname -s)" >&2
	exit 2
	;;
esac

prefix=$(brew --prefix)
printf '%s/bin\n' "$prefix" >> "$GITHUB_PATH"
