#!/usr/bin/env bash
# Exercise packaging/install.sh against a local release tree, with no network.
set -euo pipefail

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/retype-install-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

case $(uname -s) in
Linux) os=unknown-linux-gnu ;;
Darwin) os=apple-darwin ;;
*)
	echo "skipping installer test: unsupported host $(uname -s)" >&2
	exit 0
	;;
esac
case $(uname -m) in
x86_64 | amd64) arch=x86_64 ;;
arm64 | aarch64) arch=aarch64 ;;
*)
	echo "skipping installer test: unsupported host $(uname -m)" >&2
	exit 0
	;;
esac
target=$arch-$os

sha256_of() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | awk '{ print $1; exit }'
	else
		shasum -a 256 "$1" | awk '{ print $1; exit }'
	fi
}

# Build a release tree with one fake binary per version. The installer only runs
# --version, so the test needs no compiled artifact.
release() {
	local version=$1
	local message=$2
	local name=retype-$version-$target
	local dir=$tmp/releases/$version

	mkdir -p "$dir/$name/bin" "$dir/$name/share/retype"
	cat >"$dir/$name/bin/retype" <<EOF
#!/bin/sh
echo '$message'
EOF
	chmod 0755 "$dir/$name/bin/retype"
	printf 'test = true\n' >"$dir/$name/share/retype/fidelity.toml"
	tar -czf "$dir/$name.tar.gz" -C "$dir" "$name"
	printf '%s  %s\n' "$(sha256_of "$dir/$name.tar.gz")" "$name.tar.gz" >"$dir/SHA256SUMS"
}

release v0.0.0-test 'retype 0.0.0-test'
release v0.0.1-test 'retype 0.0.1-test'

prefix=$tmp/prefix
common=(
	RETYPE_RELEASE_BASE="file://$tmp/releases"
	RETYPE_PREFIX="$prefix"
	RETYPE_TARGET="$target"
)

env "${common[@]}" RETYPE_VERSION=v0.0.0-test bash "$root/packaging/install.sh" >/dev/null
[ -x "$prefix/bin/retype" ] || {
	echo 'installer did not create bin/retype' >&2
	exit 1
}
[ "$("$prefix/bin/retype" --version)" = 'retype 0.0.0-test' ] || {
	echo 'installer did not install the requested version' >&2
	exit 1
}
[ -f "$prefix/share/retype/fidelity.toml" ] || {
	echo 'installer did not install the shared assets' >&2
	exit 1
}

env "${common[@]}" bash "$root/packaging/install.sh" --version v0.0.1-test >/dev/null
[ "$("$prefix/bin/retype" --version)" = 'retype 0.0.1-test' ] || {
	echo 'installer did not update in place' >&2
	exit 1
}

echo 'installer test passed'
