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

usage() {
	cat <<'EOF'
Install or update retype from a GitHub release.

Usage:
  curl -fsSL https://justretype.com/install.sh | bash
  curl -fsSL https://justretype.com/install.sh | bash -s -- --version v0.1.0-alpha
  curl -fsSL https://justretype.com/install.sh | bash -s -- --prefix /usr/local

Options:
  --version TAG   Install a specific release tag instead of the latest
  --prefix DIR    Install under DIR (default: $HOME/.local)
  -h, --help      Show this help

Environment:
  RETYPE_VERSION       Same as --version
  RETYPE_PREFIX        Same as --prefix
  RETYPE_REPO          GitHub repository (default: LapilliWorks/retype)
  RETYPE_RELEASE_BASE  Release download base, for a mirror
  RETYPE_TARGET        Override platform detection, such as x86_64-unknown-linux-gnu

The script installs the CLI and daemon plus the shared release assets. It does
not install a user service, register a URI scheme, or ship a notifier bundle.
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

# Ubuntu 20.04 and Debian 11 define the floor; reject a Linux host below it so
# the failure is a sentence rather than a dynamic loader error.
check_glibc() {
	local reported
	reported=$(getconf GNU_LIBC_VERSION 2>/dev/null || true)
	[ -n "$reported" ] || die 'retype needs glibc 2.31 or newer; this system does not report glibc'
	reported=${reported#glibc }
	if [ "$(printf '%s\n' 2.31 "$reported" | sort -V | head -n1)" != 2.31 ]; then
		die "retype needs glibc 2.31 or newer; this system has $reported"
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
		die "Windows uses the PowerShell installer: https://justretype.com/install.ps1"
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

case $target in
*-unknown-linux-gnu) check_glibc ;;
esac

case $version in
latest)
	api=https://api.github.com/repos/$repo/releases/latest
	release=$(download "$api") || die "could not read the latest release from $api"
	tag=$(printf '%s\n' "$release" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1)
	[ -n "$tag" ] || die "could not find a tag in the latest release response from $api"
	;;
v*) tag=$version ;;
*) tag=v$version ;;
esac

release_base=${release_base%/}
base=$release_base/$tag
name=retype-$tag-$target
archive=$name.tar.gz

tmp=$(mktemp -d "${TMPDIR:-/tmp}/retype-install.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

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

bin_dir=$prefix/bin
share_dir=$prefix/share/retype
mkdir -p "$bin_dir" "$prefix/share"
[ -w "$bin_dir" ] || die "cannot write to $bin_dir; set RETYPE_PREFIX or fix the permissions"

staged_bin=$bin_dir/.retype.$$
cp "$src/bin/retype" "$staged_bin"
chmod 0755 "$staged_bin"
mv -f "$staged_bin" "$bin_dir/retype"

# Stage the shared tree before replacing the old one. Copying directly over a
# live tree would expose a half-installed set of claims and packaging files.
staged_share=$prefix/share/.retype.$$
rm -rf "$staged_share"
mkdir -p "$staged_share"
if [ -d "$src/share/retype" ]; then
	cp -R "$src/share/retype/." "$staged_share/"
fi
rm -rf "$share_dir"
mv "$staged_share" "$share_dir"

output=$("$bin_dir/retype" --version 2>&1) || die "installed $bin_dir/retype, but it does not run: $output"

printf 'retype installed at %s\n' "$bin_dir/retype"
printf '  %s\n' "$output"
case ":$PATH:" in
*":$bin_dir:"*) ;;
*) printf '\nAdd it to PATH:\n  export PATH="%s:$PATH"\n' "$bin_dir" ;;
esac
printf '\nNext: retype doctor\n'
