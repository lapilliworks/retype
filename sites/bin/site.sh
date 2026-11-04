#!/bin/sh
# Entry point for the documentation site. The generator runs in a container, so the
# host needs no Node, no node_modules and no build cache.
set -eu

usage() {
	echo 'usage: sites/bin/site.sh [--host <addr>] <dev|build|shell|clean> [app]' >&2
	exit 2
}

# Flags may sit before or after the action, so positionals are collected in order
# rather than counted.
action=''
app=''
host_arg=''
while [ $# -gt 0 ]; do
	case $1 in
	--host)
		[ $# -ge 2 ] || usage
		host_arg=$2
		shift 2
		;;
	--host=*)
		host_arg=${1#--host=}
		shift
		;;
	-*)
		echo "unknown option: $1" >&2
		usage
		;;
	*)
		if [ -z "$action" ]; then
			action=$1
		elif [ -z "$app" ]; then
			app=$1
		else
			echo "unexpected argument: $1" >&2
			usage
		fi
		shift
		;;
	esac
done
[ -n "$action" ] || usage
app=${app:-manual}

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
site=$root/sites
app_dir=$site/$app
[ -d "$app_dir" ] || {
	echo "no such site: $app" >&2
	exit 2
}

runtime=${SITE_RUNTIME:-$(command -v podman || command -v docker || true)}
runtime=${runtime##*/}
[ -n "$runtime" ] || {
	echo 'needs podman or docker: install one, or set SITE_RUNTIME to host' >&2
	exit 1
}

# A stock Node image: the container supplies a toolchain, the app scripts come from
# the mounted working tree, so the site needs no image of its own to build.
image=node:22-bookworm-slim
deps=retype-site-$app-deps
cache=retype-site-npm
out=retype-site-$app-out
# A container keeps the build in a volume and copies it out; a host build writes the
# app's own dist directory, which is what a hosted builder produces.
if [ "$runtime" = host ]; then
	out_dir=$app_dir/dist
else
	out_dir=/out
fi
# An app's dev script owns its port; this is the container side of the mapping.
app_port=${SITE_APP_PORT:-5173}
# 0.0.0.0 serves the site to the network, 127.0.0.1 serves it to this host only.
# A typed flag outranks the environment; both outrank the loopback default.
bind_host=${host_arg:-${SITE_HOST:-127.0.0.1}}

# Files the container writes into the working tree must belong to the calling user.
# Rootless podman maps container root to the host user already; docker does not.
export_args=''
case $runtime in
docker) export_args="--user $(id -u):$(id -g)" ;;
esac

ensure_image() {
	[ "$runtime" = host ] && return 0
	$runtime image inspect "$image" >/dev/null 2>&1 && return 0
	$runtime pull "$image"
}

# The site reads its shape from the environment. One file feeds both runtimes: a
# container takes it with --env-file, a host build sources it.
site_env() {
	set -- "SITE_OUT_DIR=$out_dir" "SITE_BASE=${SITE_BASE:-/}" "SITE_APP_PORT=$app_port"
	[ -n "${SITE_ALLOWED_HOSTS:-}" ] && set -- "$@" "SITE_ALLOWED_HOSTS=$SITE_ALLOWED_HOSTS"
	[ -n "${SITE_DOCS_URL:-}" ] && set -- "$@" "SITE_DOCS_URL=$SITE_DOCS_URL"
	[ -n "${SITE_MANUAL_PORT:-}" ] && set -- "$@" "SITE_MANUAL_PORT=$SITE_MANUAL_PORT"
	if [ -n "${SITE_PRODUCT_URL:-}" ]; then
		set -- "$@" "SITE_PRODUCT_URL=$SITE_PRODUCT_URL"
	elif [ "$action" = dev ]; then
		set -- "$@" "SITE_PRODUCT_URL=http://127.0.0.1:${SITE_PORT:-$app_port}/"
	fi
	[ -n "${SITE_CLEAN_URLS:-}" ] && set -- "$@" "SITE_CLEAN_URLS=$SITE_CLEAN_URLS"
	printf '%s\n' "$@"
}

env_file=$(mktemp)
trap 'rm -f "$env_file"' EXIT
site_env > "$env_file"

run() {
	# A hosted builder has Node and no container runtime, so the same entrypoint
	# runs on the host there; only the toolchain's home differs.
	if [ "$runtime" = host ]; then
		set -a
		. "$env_file"
		set +a
		CI=${CI:-true}
		export CI
		exec "$site/bin/entrypoint.sh" "$app" "$@"
	fi
	interactive=''
	if [ -t 0 ] && [ -t 1 ]; then interactive='-it'; fi
	# shellcheck disable=SC2086
	$runtime run --rm $interactive \
		--mount "type=bind,source=$root,target=/src" \
		--mount "type=volume,source=$deps,target=/src/sites/$app/node_modules" \
		--mount "type=volume,source=$cache,target=/npm-cache" \
		--mount "type=volume,source=$out,target=/out" \
		--publish "$bind_host:${SITE_PORT:-$app_port}:$app_port" \
		--env HOME=/tmp \
		--env NPM_CONFIG_CACHE=/npm-cache \
		--env NPM_CONFIG_UPDATE_NOTIFIER=false \
		--env CI=true \
		--env-file "$env_file" \
		--entrypoint /src/sites/bin/entrypoint.sh \
		"$image" "$app" "$@"
}

# Copies the build out of its volume so the exported files belong to the host user.
export_build() {
	[ "$runtime" = host ] && return 0
	mkdir -p "$app_dir/dist"
	# shellcheck disable=SC2086
	$runtime run --rm $export_args \
		--mount "type=volume,source=$out,target=/out" \
		--mount "type=bind,source=$app_dir/dist,target=/dist" \
		--entrypoint sh "$image" -c 'cp -a /out/. /dist/'
}

clean() {
	# The image is stock, hence shared: only the volumes and the build are ours.
	if [ "$runtime" != host ]; then
		for volume in "$deps" "$cache" "$out"; do
			$runtime volume rm --force "$volume" 2>/dev/null || true
		done
	fi
	rm -rf "$app_dir/dist"
}

ensure_image
case $action in
dev)
	if [ "$bind_host" = '0.0.0.0' ]; then
		echo "serving on port ${SITE_PORT:-$app_port} of every interface: use this machine's address, not localhost" >&2
	else
		echo "serving http://$bind_host:${SITE_PORT:-$app_port}/ (SITE_HOST=0.0.0.0 or --host 0.0.0.0 to serve the network)" >&2
	fi
	run dev
	;;
build)
	run build
	export_build
	echo "built $app_dir/dist" >&2
	;;
shell) run shell ;;
clean) clean ;;
*) usage ;;
esac
