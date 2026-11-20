#!/usr/bin/env bash
# Install or update retype from a GitHub release.
#
# The release archive is selected from the host platform, its SHA-256 is checked
# against the release checksums, and the binary and shared assets are installed
# under a prefix. Running the script again updates an existing installation.
#
# This is not a code signature. The installed tier still needs platform signing.
set -euo pipefail

repo=${RETYPE_REPO:-LapilliWorks/retype}
version=${RETYPE_VERSION:-latest}
prefix=${RETYPE_PREFIX:-${HOME:?HOME is not set}/.local}
target=${RETYPE_TARGET:-}
release_base=${RETYPE_RELEASE_BASE:-https://github.com/$repo/releases/download}
uninstall=false

usage() {
	cat <<'EOF'
Install or update retype from a GitHub release.

Usage:
  curl -fsSL https://retype.name/install.sh | bash
  curl -fsSL https://retype.name/install.sh | bash -s -- --version v0.1.0-alpha
  curl -fsSL https://retype.name/install.sh | bash -s -- --prefix /usr/local

Options:
  --version TAG   Install a specific release tag instead of the latest
  --prefix DIR    Install under DIR (default: $HOME/.local)
  --uninstall     Remove this Linux installation, preserving configuration and recovery data
  -h, --help      Show this help

Environment:
  RETYPE_VERSION       Same as --version
  RETYPE_PREFIX        Same as --prefix
  RETYPE_REPO          GitHub repository (default: LapilliWorks/retype)
  RETYPE_RELEASE_BASE  Release download base, for a mirror
  RETYPE_TARGET        Override platform detection, such as x86_64-unknown-linux-gnu

On Linux, installs the notification/URI desktop entry and systemd user unit;
new services stay disabled. Requires python3 and xdg-mime (xdg-utils).
On macOS, installs the CLI, daemon and shared assets only.
EOF
}

die() {
	printf 'install: %s\n' "$*" >&2
	exit 1
}

have() {
	command -v "$1" >/dev/null 2>&1
}

download() {
	if have curl; then
		curl -fsSL --retry 3 "$1"
	elif have wget; then
		wget -qO- "$1"
	else
		die 'needs curl or wget'
	fi
}

download_file() {
	if have curl; then
		curl -fsSL --retry 3 -o "$2" "$1"
	elif have wget; then
		wget -qO "$2" "$1"
	else
		die 'needs curl or wget'
	fi
}

sha256_of() {
	if have sha256sum; then
		sha256sum "$1" | awk '{ print $1; exit }'
	elif have shasum; then
		shasum -a 256 "$1" | awk '{ print $1; exit }'
	elif have openssl; then
		openssl dgst -sha256 -r "$1" | awk '{ print $1; exit }'
	else
		die 'needs sha256sum, shasum or openssl'
	fi
}

# Current Debian stable (trixie) defines the Linux floor. Report an older runtime
# before attempting to execute the downloaded binary.
check_glibc() {
	local reported
	reported=$(getconf GNU_LIBC_VERSION 2>/dev/null || true)
	[ -n "$reported" ] || die 'retype targets current Debian stable and needs glibc 2.41 or newer; this system does not report glibc'
	reported=${reported#glibc }
	if [ "$(printf '%s\n' 2.41 "$reported" | sort -V | head -n1)" != 2.41 ]; then
		die "retype targets current Debian stable and needs glibc 2.41 or newer; this system has $reported"
	fi
}

while [ $# -gt 0 ]; do
	case $1 in
	--version)
		[ $# -ge 2 ] || die '--version needs a tag'
		version=$2
		shift 2
		;;
	--version=*)
		version=${1#*=}
		shift
		;;
	--prefix)
		[ $# -ge 2 ] || die '--prefix needs a directory'
		prefix=$2
		shift 2
		;;
	--prefix=*)
		prefix=${1#*=}
		shift
		;;
	--uninstall)
		uninstall=true
		shift
		;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		die "unknown option: $1"
		;;
	esac
done

[ -n "$prefix" ] || die 'the install prefix is empty'
case $prefix in
/*) ;;
*) prefix=$PWD/$prefix ;;
esac

if [ -z "$target" ]; then
	case $(uname -s) in
	Linux) os=unknown-linux-gnu ;;
	Darwin) os=apple-darwin ;;
	MINGW* | MSYS* | CYGWIN*)
		die "Windows uses the PowerShell installer: https://retype.name/install.ps1"
		;;
	*) die "unsupported operating system: $(uname -s)" ;;
	esac
	case $(uname -m) in
	x86_64 | amd64) arch=x86_64 ;;
	arm64 | aarch64) arch=aarch64 ;;
	*) die "unsupported architecture: $(uname -m)" ;;
	esac
	target=$arch-$os
fi

bin_dir=$prefix/bin
share_dir=$prefix/share/retype
integration_helper=

if $uninstall; then
	case $target in
	*-unknown-linux-gnu) ;;
	*) die '--uninstall currently supports Linux only' ;;
	esac
	have python3 || die 'Linux installation removal needs python3'
	[ -f "$share_dir/packaging/linux/integrate.py" ] || die 'this installation has no Linux integration helper'
else
	case $target in
	*-unknown-linux-gnu)
		check_glibc
		have python3 || die 'Linux desktop integration needs python3'
		;;
	esac
fi

case $version in
latest)
	if $uninstall; then
		tag=uninstall
	else
		api=https://api.github.com/repos/$repo/releases?per_page=100
		release=$(download "$api") || die "could not read published releases from $api"
		tag_pattern='"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)"'
		[[ $release =~ $tag_pattern ]] || die "could not find a tag in the published release response from $api"
		tag=${BASH_REMATCH[1]}
	fi
	;;
v*) tag=$version ;;
*) tag=v$version ;;
esac

release_base=${release_base%/}
base=$release_base/$tag
name=retype-$tag-$target
archive=$name.tar.gz

tmp=$(mktemp -d "${TMPDIR:-/tmp}/retype-install.XXXXXX")
staged_bin=
staged_share=
backup=
changed_bin=false
changed_share=false
had_share=false

restore_installation() {
	if $changed_bin; then
		if [ -e "$backup/retype" ] || [ -L "$backup/retype" ]; then
			cp -pP "$backup/retype" "$staged_bin" || return 1
			mv -f "$staged_bin" "$bin_dir/retype" || return 1
		else
			rm -f "$bin_dir/retype" || return 1
		fi
	fi
	if $changed_share; then
		if [ -e "$backup/share" ] || [ -L "$backup/share" ]; then
			rm -rf "$share_dir" || return 1
			mv "$backup/share" "$share_dir" || return 1
		elif ! $had_share; then
			rm -rf "$share_dir" || return 1
		fi
	fi
}

cleanup() {
	local status=$?
	trap - EXIT
	if [ "$status" -ne 0 ] && [ -n "$backup" ]; then
		if restore_installation && { [ -z "$integration_helper" ] ||
			python3 "$integration_helper" rollback --prefix "$prefix" --backup "$backup"; }; then
			printf 'install: previous installation preserved; backup: %s\n' "$backup" >&2
		else
			printf 'install: rollback failed; recover the previous installation from %s\n' "$backup" >&2
		fi
	fi
	[ -z "$staged_bin" ] || rm -f "$staged_bin"
	[ -z "$staged_share" ] || rm -rf "$staged_share"
	rm -rf "$tmp"
	exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if $uninstall; then
	backup=$(mktemp -d "$prefix/share/retype-backup.XXXXXX")
	integration_helper=$backup/integrate.py
	cp "$share_dir/packaging/linux/integrate.py" "$integration_helper"
	if [ -e "$bin_dir/retype" ] || [ -L "$bin_dir/retype" ]; then
		staged_bin=$(mktemp "$bin_dir/.retype.XXXXXX")
		cp -pP "$bin_dir/retype" "$backup/retype"
	fi
	python3 "$integration_helper" remove --prefix "$prefix" --backup "$backup"
	changed_bin=true
	rm -f "$bin_dir/retype"
	had_share=true
	changed_share=true
	mv "$share_dir" "$backup/share"
	printf 'retype removed; configuration and recovery data preserved. Backup: %s\n' "$backup"
	exit 0
fi

sums=$(download "$base/SHA256SUMS") || die "could not download checksums: $base/SHA256SUMS"
expected=$(printf '%s\n' "$sums" | awk -v name="$archive" '$2 == name || $2 == "*" name { print $1; exit }')
[ -n "$expected" ] || die "release $tag has no checksum for $archive"

download_file "$base/$archive" "$tmp/$archive" || die "could not download $base/$archive"
actual=$(sha256_of "$tmp/$archive")
[ "$actual" = "$expected" ] || die "checksum mismatch for $archive: expected $expected, got $actual"

have tar || die 'needs tar'
tar -xzf "$tmp/$archive" -C "$tmp"
src=$tmp/$name
[ -f "$src/bin/retype" ] || die "$archive does not contain bin/retype"

mkdir -p "$bin_dir" "$prefix/share"
[ -w "$bin_dir" ] || die "cannot write to $bin_dir; set RETYPE_PREFIX or fix the permissions"

staged_bin=$(mktemp "$bin_dir/.retype.XXXXXX")
cp "$src/bin/retype" "$staged_bin"
chmod 0755 "$staged_bin"
staged_share=$(mktemp -d "$prefix/share/.retype.XXXXXX")
if [ -d "$src/share/retype" ]; then
	cp -R "$src/share/retype/." "$staged_share/"
fi
output=$("$staged_bin" --version 2>&1) || die "the staged binary does not run; installation unchanged: $output"
case $target in
*-unknown-linux-gnu)
	[ -f "$staged_share/packaging/linux/integrate.py" ] || die 'the release lacks Linux desktop integration assets'
	python3 "$staged_share/packaging/linux/integrate.py" check --prefix "$prefix"
	;;
esac

# Keep the previous installation on the destination filesystem, including after success.
backup=$(mktemp -d "$prefix/share/retype-backup.XXXXXX")
case $target in
*-unknown-linux-gnu)
	integration_helper=$backup/integrate.py
	cp "$staged_share/packaging/linux/integrate.py" "$integration_helper"
	;;
esac
if [ -e "$bin_dir/retype" ] || [ -L "$bin_dir/retype" ]; then
	cp -pP "$bin_dir/retype" "$backup/retype"
fi
if [ -e "$share_dir" ] || [ -L "$share_dir" ]; then
	had_share=true
	changed_share=true
	mv "$share_dir" "$backup/share"
fi
changed_share=true
mv "$staged_share" "$share_dir"
changed_bin=true
mv -f "$staged_bin" "$bin_dir/retype"
output=$("$bin_dir/retype" --version 2>&1) || die "the installed binary does not run: $output"
if [ -n "$integration_helper" ]; then
	python3 "$integration_helper" install --prefix "$prefix" --backup "$backup"
fi

printf 'retype installed at %s\n' "$bin_dir/retype"
printf '  %s\n' "$output"
printf 'Previous installation backup: %s\n' "$backup"
case ":$PATH:" in
*":$bin_dir:"*) ;;
*) printf '\nAdd it to PATH:\n  export PATH="%s:$PATH"\n' "$bin_dir" ;;
esac
printf '\nNext: retype doctor\n'
