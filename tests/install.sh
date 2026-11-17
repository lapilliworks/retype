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
${3:-:}
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

# GitHub's public release list includes prereleases; /releases/latest does not.
real_curl=$(command -v curl)
mkdir -p "$tmp/bin"
cat >"$tmp/bin/curl" <<EOF
#!/bin/sh
case "\$*" in
*api.github.com/*/releases?per_page=100*)
	printf '%s\\n' '[{"tag_name":"v0.0.1-test","prerelease":true,"draft":false},{"tag_name":"v0.0.0-test"}]'
	;;
*api.github.com*) exit 22 ;;
*) exec '$real_curl' "\$@" ;;
esac
EOF
chmod 0755 "$tmp/bin/curl"
env "${common[@]}" PATH="$tmp/bin:$PATH" RETYPE_VERSION=latest bash "$root/packaging/install.sh" >/dev/null
[ "$("$prefix/bin/retype" --version)" = 'retype 0.0.1-test' ] || {
	echo 'the default installer did not select the published prerelease' >&2
	exit 1
}

# Failure before publication must leave the live installation untouched.
release v0.0.2-broken 'broken' 'exit 1'
printf 'retain these assets\n' >"$prefix/share/retype/previous.txt"
if env "${common[@]}" RETYPE_VERSION=v0.0.2-broken bash "$root/packaging/install.sh" >"$tmp/failure.log" 2>&1; then
	echo 'installer accepted a binary that does not run' >&2
	exit 1
fi
[ "$("$prefix/bin/retype" --version)" = 'retype 0.0.1-test' ]
[ "$(<"$prefix/share/retype/previous.txt")" = 'retain these assets' ]

# Simulate failure after moving the previous shared tree into its persistent backup.
real_mv=$(command -v mv)
cat >"$tmp/bin/mv" <<EOF
#!/bin/sh
case "\$1:\$2" in
'$prefix/share/.retype.'*:'$prefix/share/retype') exit 1 ;;
*) exec '$real_mv' "\$@" ;;
esac
EOF
chmod 0755 "$tmp/bin/mv"
if env "${common[@]}" PATH="$tmp/bin:$PATH" RETYPE_VERSION=v0.0.1-test \
	bash "$root/packaging/install.sh" >"$tmp/failure.log" 2>&1; then
	echo 'installer did not report shared-tree publication failure' >&2
	exit 1
fi
[ "$("$prefix/bin/retype" --version)" = 'retype 0.0.1-test' ]
[ "$(<"$prefix/share/retype/previous.txt")" = 'retain these assets' ]

# A failure after both publications must restore the previous binary and assets.
rm "$tmp/bin/mv"
release v0.0.3-late 'late failure' 'case $0 in */bin/retype) exit 1 ;; esac'
if env "${common[@]}" RETYPE_VERSION=v0.0.3-late bash "$root/packaging/install.sh" >"$tmp/failure.log" 2>&1; then
	echo 'installer did not report post-publication failure' >&2
	exit 1
fi
grep -q 'the installed binary does not run' "$tmp/failure.log"
[ "$("$prefix/bin/retype" --version)" = 'retype 0.0.1-test' ]
[ "$(<"$prefix/share/retype/previous.txt")" = 'retain these assets' ]
[ -n "$(find "$prefix/share" -path '*/retype-backup.*/retype' -type f -print -quit)" ]

if [ "$os" = unknown-linux-gnu ]; then
	cat >"$tmp/bin/getconf" <<'EOF'
#!/bin/sh
printf 'glibc %s\n' "$RETYPE_TEST_GLIBC"
EOF
	chmod 0755 "$tmp/bin/getconf"
	if env "${common[@]}" PATH="$tmp/bin:$PATH" RETYPE_TEST_GLIBC=2.40 RETYPE_VERSION=v0.0.1-test \
		bash "$root/packaging/install.sh" >"$tmp/floor.log" 2>&1; then
		echo 'installer accepted a runtime below current Debian stable' >&2
		exit 1
	fi
	grep -q 'needs glibc 2.41 or newer' "$tmp/floor.log"
	[ "$("$prefix/bin/retype" --version)" = 'retype 0.0.1-test' ]
	env "${common[@]}" PATH="$tmp/bin:$PATH" RETYPE_TEST_GLIBC=2.41 RETYPE_VERSION=v0.0.1-test \
		bash "$root/packaging/install.sh" >"$tmp/floor.log" 2>&1
fi

echo 'installer test passed'
