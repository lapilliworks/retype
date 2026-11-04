#!/bin/sh
# Installs the app's dependencies, then hands over to it. Runs inside the container,
# and directly on the host when there is no container runtime to use.
set -eu

app=${1:?usage: entrypoint.sh <app> <dev|build|shell>}
action=${2:?usage: entrypoint.sh <app> <dev|build|shell>}
shift 2

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
work=$root/sites/$app
if [ ! -d "$work" ]; then
	echo "no such site: $app" >&2
	exit 2
fi
cd "$work"

stamp=node_modules/.installed
if [ ! -f "$stamp" ] || [ package.json -nt "$stamp" ]; then
	echo "installing $app dependencies" >&2
	mkdir -p node_modules
	npm install --no-audit --no-fund --loglevel=error
	touch "$stamp"
fi

case $action in
	dev) exec npm run dev -- "$@" ;;
	build) exec npm run build ;;
	shell) exec sh ;;
	*)
		echo "unknown action: $action" >&2
		exit 2
		;;
esac
