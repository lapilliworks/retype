#!/usr/bin/env bash
# Assemble verified build archives and installers in a private release directory.
set -euo pipefail

if [ "$#" -ne 1 ]; then
	printf 'Usage: %s <release-directory>\n' "$0" >&2
	exit 2
fi

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd -- "$1"

for archive in *.tar.gz; do
	if [ ! -f "$archive" ] || [ ! -f "$archive.sha256" ]; then
		printf 'Missing release archive or checksum: %s\n' "$archive" >&2
		exit 1
	fi
	if ! sha256sum -- "$archive" | cmp -s -- "$archive.sha256" - &&
		! sha256sum --binary -- "$archive" | cmp -s -- "$archive.sha256" -; then
		printf 'Build checksum does not match archive: %s\n' "$archive" >&2
		exit 1
	fi
	printf '%s: OK\n' "$archive"
done

for file in install.sh install.ps1 install.sh.sha256 install.ps1.sha256 SHA256SUMS; do
	if [ -e "$file" ] || [ -L "$file" ]; then
		printf 'Refusing to overwrite release asset: %s\n' "$file" >&2
		exit 1
	fi
done

cp "$root/packaging/install.sh" install.sh
cp "$root/packaging/install.ps1" install.ps1
sha256sum -- *.tar.gz install.sh install.ps1 > SHA256SUMS
sha256sum -- install.sh > install.sh.sha256
sha256sum -- install.ps1 > install.ps1.sha256
sha256sum --check SHA256SUMS
