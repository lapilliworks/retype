#!/usr/bin/env bash
# Exercise release assembly without publishing or using real build artifacts.
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/retype-release-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

first=retype-dev-1234567-x86_64-unknown-linux-gnu.tar.gz
second=retype-dev-1234567-aarch64-apple-darwin.tar.gz

fixture() {
	local directory=$1
	mkdir -p "$directory"
	printf 'first synthetic archive\n' > "$directory/$first"
	printf 'second synthetic archive\n' > "$directory/$second"
	(
		cd "$directory"
		sha256sum -- "$first" > "$first.sha256"
		sha256sum --binary -- "$second" > "$second.sha256"
	)
}

expect_failure() {
	local name=$1
	if bash "$root/scripts/assemble-release.sh" "$tmp/$name" > "$tmp/$name.log" 2>&1; then
		printf 'Invalid release assembly unexpectedly passed: %s\n' "$name" >&2
		exit 1
	fi
}

fixture "$tmp/valid"
if ! bash "$root/scripts/assemble-release.sh" "$tmp/valid" > "$tmp/valid.log" 2>&1; then
	cat "$tmp/valid.log" >&2
	exit 1
fi
(
	cd "$tmp/valid"
	sha256sum --check SHA256SUMS
	for file in "$first" "$second" install.sh install.ps1; do
		sha256sum --check -- "$file.sha256"
	done
	[ "$(wc -l < SHA256SUMS)" -eq 4 ]
	cmp -- "$root/packaging/install.sh" install.sh
	cmp -- "$root/packaging/install.ps1" install.ps1
)

fixture "$tmp/corrupted"
printf 'altered\n' > "$tmp/corrupted/$second"
expect_failure corrupted

fixture "$tmp/missing-checksum"
rm "$tmp/missing-checksum/$first.sha256"
expect_failure missing-checksum

fixture "$tmp/wrong-target"
printf 'different file\n' > "$tmp/wrong-target/other.bin"
(cd "$tmp/wrong-target" && sha256sum -- other.bin > "$first.sha256")
expect_failure wrong-target

mkdir "$tmp/missing-archive"
expect_failure missing-archive

fixture "$tmp/occupied"
printf 'preserve this file\n' > "$tmp/occupied/install.sh"
expect_failure occupied
[ "$(< "$tmp/occupied/install.sh")" = 'preserve this file' ]
[ ! -e "$tmp/occupied/SHA256SUMS" ]

printf 'Release assembly tests passed.\n'
