#!/bin/sh
# Runs inside the container. Installs the generator's dependencies into the mounted
# volume when package.json changes, then hands over to it.
set -eu

app=${1:?usage: entrypoint.sh <app> <dev|build|shell>}
action=${2:?usage: entrypoint.sh <app> <dev|build|shell>}
shift 2

work=/src/sites/$app
if [ ! -d "$work" ]; then
	echo "no such site: $app" >&2
	exit 2
fi
cd "$work"

stamp=node_modules/.installed
if [ ! -f "$stamp" ] || [ package.json -nt "$stamp" ]; then
	echo "installing $app dependencies" >&2
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
