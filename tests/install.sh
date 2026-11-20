#!/usr/bin/env bash
# Exercise packaging/install.sh against a local release tree, with no network.
set -euo pipefail

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/retype-install-test.XXXXXX")
cleanup() {
	local status=$?
	if [ "$status" -ne 0 ]; then
		local report=${RETYPE_TEST_REPORT_DIR:-$root/target/test-reports}/installer-$(basename "$tmp")
		mkdir -p "$report"
		cp -a "$tmp/." "$report/"
		printf 'Failed installer fixture preserved: %s\n' "$report" >&2
	fi
	rm -rf "$tmp"
}
trap cleanup EXIT

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

# These fixtures execute shell scripts; exercise the Debian runtime guard independently of the test host.
if [ "$os" = unknown-linux-gnu ]; then
	mkdir -p "$tmp/bin"
	cat >"$tmp/bin/getconf" <<'EOF'
#!/bin/sh
printf 'glibc %s\n' "${RETYPE_TEST_GLIBC:-2.41}"
EOF
	chmod 0755 "$tmp/bin/getconf"
fi

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
if [ -n "\${RETYPE_TEST_LAUNCH_RECORD:-}" ]; then
	printf '%s\\n' "\$@" >"\$RETYPE_TEST_LAUNCH_RECORD"
fi
echo '$message'
EOF
	chmod 0755 "$dir/$name/bin/retype"
	printf 'test = true\n' >"$dir/$name/share/retype/fidelity.toml"
	mkdir -p "$dir/$name/share/retype/packaging"
	cp -R "$root/packaging/linux" "$dir/$name/share/retype/packaging/"
	tar -czf "$dir/$name.tar.gz" -C "$dir" "$name"
	printf '%s  %s\n' "$(sha256_of "$dir/$name.tar.gz")" "$name.tar.gz" >"$dir/SHA256SUMS"
}

release v0.0.0-test 'retype 0.0.0-test'
release v0.0.1-test 'retype 0.0.1-test'

prefix="$tmp/prefix with spaces % and \$cash"
mkdir -p "$tmp/bin" "$tmp/config" "$tmp/data/applications" "$tmp/state/retype"
printf 'keep recovery data\n' >"$tmp/state/retype/recovery"
printf 'keep configuration\n' >"$tmp/config/retype.toml"
cat >"$tmp/config/mimeapps.list" <<'EOF'
[Default Applications]
text/plain=notes.desktop;
x-scheme-handler/retype=previous.desktop;
EOF
cp "$tmp/config/mimeapps.list" "$tmp/mimeapps-before"
cp "$tmp/config/mimeapps.list" "$tmp/config/gnome-mimeapps.list"
mv "$tmp/config/mimeapps.list" "$tmp/linked-preferences"
ln -s "$tmp/linked-preferences" "$tmp/config/mimeapps.list"
cat >"$tmp/data/applications/previous.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=Previous handler
Exec=true %u
MimeType=x-scheme-handler/retype;
EOF
cat >"$tmp/bin/systemctl" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$RETYPE_TEST_SYSTEMCTL_RECORD"
case $2 in
show-environment) printf '%q\n' "HOME=$HOME" "XDG_CONFIG_HOME=$XDG_CONFIG_HOME" ;;
is-active) [ -f "$RETYPE_TEST_SERVICE_ACTIVE" ] ;;
is-enabled) [ -f "$RETYPE_TEST_SERVICE_ENABLED" ] && printf 'enabled\n' ;;
stop) rm -f "$RETYPE_TEST_SERVICE_ACTIVE" ;;
restart | try-restart) touch "$RETYPE_TEST_SERVICE_ACTIVE" ;;
enable) touch "$RETYPE_TEST_SERVICE_ENABLED" ;;
disable) rm -f "$RETYPE_TEST_SERVICE_ENABLED" ;;
daemon-reload)
	if [ -f "$RETYPE_TEST_FAIL_RELOAD" ]; then
		rm "$RETYPE_TEST_FAIL_RELOAD"
		exit 1
	fi
	;;
esac
EOF
chmod 0755 "$tmp/bin/systemctl"
common=(
	RETYPE_RELEASE_BASE="file://$tmp/releases"
	RETYPE_PREFIX="$prefix"
	RETYPE_TARGET="$target"
	XDG_CONFIG_HOME="$tmp/config"
	XDG_DATA_HOME="$tmp/data"
	XDG_STATE_HOME="$tmp/state"
	XDG_CURRENT_DESKTOP=GNOME
	PATH="$tmp/bin:$PATH"
	RETYPE_TEST_SYSTEMCTL_RECORD="$tmp/systemctl.log"
	RETYPE_TEST_SERVICE_ACTIVE="$tmp/service-active"
	RETYPE_TEST_SERVICE_ENABLED="$tmp/service-enabled"
	RETYPE_TEST_FAIL_RELOAD="$tmp/fail-reload"
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
if [ "$os" = unknown-linux-gnu ]; then
	[ -f "$tmp/data/applications/retype-uri.desktop" ]
	[ -f "$tmp/config/systemd/user/retype.service" ]
	[ "$(stat -c '%a' "$prefix/share/retype/installation.json")" = 600 ]
	[ "$(env "${common[@]}" xdg-mime query default x-scheme-handler/retype)" = retype-uri.desktop ]
	[ ! -e "$tmp/service-active" ] && [ ! -e "$tmp/service-enabled" ]
	! grep -q '@PREFIX@' "$tmp/config/systemd/user/retype.service" "$tmp/data/applications/retype-uri.desktop"
	[ -L "$tmp/config/mimeapps.list" ]
	env "${common[@]}" RETYPE_TEST_LAUNCH_RECORD="$tmp/launched" \
		gio launch "$tmp/data/applications/retype-uri.desktop" 'retype://undo/example' >/dev/null
	for attempt in {1..100}; do
		[ ! -f "$tmp/launched" ] || break
		sleep 0.05
	done
	[ "$(<"$tmp/launched")" = $'uri\nretype://undo/example' ]
fi

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

if [ "$os" = unknown-linux-gnu ]; then
	# A desktop registration failure rolls back the binary, assets and integration together.
	cp "$prefix/share/retype/installation.json" "$tmp/installation-before.json"
	touch "$tmp/fail-reload"
	if env "${common[@]}" RETYPE_VERSION=v0.0.0-test bash "$root/packaging/install.sh" >"$tmp/integration-failure.log" 2>&1; then
		echo 'installer did not report integration failure' >&2
		exit 1
	fi
	[ "$("$prefix/bin/retype" --version)" = 'retype 0.0.1-test' ]
	cmp "$prefix/share/retype/installation.json" "$tmp/installation-before.json"
	[ "$(env "${common[@]}" xdg-mime query default x-scheme-handler/retype)" = retype-uri.desktop ]

	# Updates retain an enabled, active service; they never enable a new service.
	touch "$tmp/service-active" "$tmp/service-enabled"
	env "${common[@]}" RETYPE_VERSION=v0.0.1-test bash "$root/packaging/install.sh" >/dev/null
	[ -f "$tmp/service-active" ] && [ -f "$tmp/service-enabled" ]
	grep -q -- '--user try-restart retype.service' "$tmp/systemctl.log"

	# Failed removal restores integration, enablement and the original active service.
	cat >"$tmp/bin/mv" <<EOF
#!/bin/sh
case "\$1" in
'$prefix/share/retype') exit 1 ;;
*) exec '$real_mv' "\$@" ;;
esac
EOF
	chmod 0755 "$tmp/bin/mv"
	if env "${common[@]}" bash "$root/packaging/install.sh" --uninstall >"$tmp/removal-failure.log" 2>&1; then
		echo 'installer did not report removal failure' >&2
		exit 1
	fi
	rm "$tmp/bin/mv"
	[ "$("$prefix/bin/retype" --version)" = 'retype 0.0.1-test' ]
	[ -f "$tmp/service-active" ] && [ -f "$tmp/service-enabled" ]
	[ "$(env "${common[@]}" xdg-mime query default x-scheme-handler/retype)" = retype-uri.desktop ]

	# Removal needs no release server and preserves configuration and recovery records.
	env "${common[@]}" RETYPE_RELEASE_BASE=file:///not-a-release bash "$root/packaging/install.sh" --uninstall >/dev/null
	[ ! -e "$prefix/bin/retype" ] && [ ! -e "$prefix/share/retype" ]
	[ ! -e "$tmp/config/systemd/user/retype.service" ] && [ ! -e "$tmp/data/applications/retype-uri.desktop" ]
	[ ! -e "$tmp/service-active" ] && [ ! -e "$tmp/service-enabled" ]
	[ "$(env "${common[@]}" xdg-mime query default x-scheme-handler/retype)" = previous.desktop ]
	grep -q 'text/plain=notes.desktop;' "$tmp/config/mimeapps.list"
	[ "$(<"$tmp/state/retype/recovery")" = 'keep recovery data' ]
	[ "$(<"$tmp/config/retype.toml")" = 'keep configuration' ]

	# Changed URI choices and edited units stay user-owned through update and removal.
	env "${common[@]}" RETYPE_VERSION=v0.0.1-test bash "$root/packaging/install.sh" >/dev/null
	printf '# local override\n' >>"$tmp/config/systemd/user/retype.service"
	env "${common[@]}" xdg-mime default previous.desktop x-scheme-handler/retype
	cp "$tmp/config/mimeapps.list" "$tmp/config/gnome-mimeapps.list"
	printf 'text/html=browser.desktop;\n' >>"$tmp/config/mimeapps.list"
	env "${common[@]}" RETYPE_VERSION=v0.0.0-test bash "$root/packaging/install.sh" >/dev/null
	grep -q '# local override' "$tmp/config/systemd/user/retype.service"
	[ "$(env "${common[@]}" xdg-mime query default x-scheme-handler/retype)" = previous.desktop ]
	env "${common[@]}" bash "$root/packaging/install.sh" --uninstall >/dev/null
	grep -q '# local override' "$tmp/config/systemd/user/retype.service"
	grep -q 'text/html=browser.desktop;' "$tmp/config/mimeapps.list"
	[ "$(env "${common[@]}" xdg-mime query default x-scheme-handler/retype)" = previous.desktop ]
fi

echo 'installer test passed'
