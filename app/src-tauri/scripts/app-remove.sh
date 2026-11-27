# Stops and removes one app's containers. Data under /var/lib/konak/apps/<id>/ is kept unless the
# second argument is --delete-data. Windows folders (media, photos) are never touched.
set -euo pipefail
id="$1" mode="${2:-keep}"
[[ "$id" =~ ^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$ ]] || { echo "bad app id: $id" >&2; exit 2; }
dir=/var/lib/konak/apps/$id
export DOCKER_HOST=unix:///run/podman/podman.sock
unset COMPOSE_FILE COMPOSE_PATH_SEPARATOR
volumes=()
if [ "$mode" = "--delete-data" ]; then volumes=(--volumes); fi
if [ -f "$dir/compose.yml" ]; then
  (cd "$dir" && docker-compose -f compose.yml -p "$id" down --remove-orphans "${volumes[@]}")
else
  # By project name alone, from a folder that holds no compose file.
  (cd / && docker-compose -p "$id" down --remove-orphans "${volumes[@]}") || true
fi
if [ "$mode" = "--delete-data" ]; then
  rm -rf --one-file-system "$dir"
fi
echo removed
