#!/bin/sh
# Build one deployable site: the product page at the root, the manual under /docs/.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
site=$root/sites
out=$site/dist

"$site/bin/site.sh" build product
SITE_BASE=/docs/ SITE_CLEAN_URLS=false "$site/bin/site.sh" build manual

rm -rf "$out"
mkdir -p "$out"
cp -a "$site/product/dist/." "$out/"
mkdir -p "$out/docs"
cp -a "$site/manual/dist/." "$out/docs/"

echo "built $out" >&2
