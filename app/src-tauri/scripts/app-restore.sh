# Restore helpers for one app (see restore.rs). Args: command id [more]
#   probe id [podman volume names]   what exists: the folder, set-aside copies, the app's volumes
#   aside id                         removes the containers, moves the folder to <id>.before-restore
#                                    and every volume's content to <id>.before-restore-volumes/<vol>
#   extract id                       stdin: GNU tar of the app folder (members <id>/...), unpacked in
#                                    a staging folder and moved into place
#   import id compose_name podman_name   stdin: `podman volume export` output for that volume
#   rollback id had_old              removes what the restore made; with had_old=1 moves the set-aside
#                                    folder and volume content back
#   commit id                        deletes the set-aside copies
# Every command but extract and import keeps stdin closed; those two read the data from it.
set -euo pipefail
shopt -s nullglob
cmd="$1" id="$2"
[[ "$id" =~ ^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$ ]] || { echo "bad app id: $id" >&2; exit 2; }
apps=/var/lib/konak/apps
dir=$apps/$id aside=$apps/$id.before-restore vaside=$apps/$id.before-restore-volumes
staging=/var/lib/konak/restore-staging/$id
export DOCKER_HOST=unix:///run/podman/podman.sock
label="label=com.docker.compose.project=$id"

project_volumes() { podman volume ls -q --filter "$label" </dev/null; }
mountpoint_of() { podman volume inspect "$1" --format '{{.Mountpoint}}' </dev/null; }
empty() { find "$1" -mindepth 1 -delete </dev/null; }
# Moves everything inside $1 (dotfiles too) into $2.
move_content() { find "$1" -mindepth 1 -maxdepth 1 -exec mv -t "$2" {} + </dev/null; }

remove_containers() {
  if [ -f "$dir/compose.yml" ]; then
    # Up to a minute per container for a clean database shutdown, as a backup stops them.
    (cd "$dir" && docker-compose -p "$id" down --remove-orphans -t 60 </dev/null)
  fi
  # Whatever is left without a compose file (an extract that failed before it was in place).
  local c n
  for c in $(podman ps -aq --filter "$label" </dev/null); do podman rm -f -t 60 "$c" </dev/null >/dev/null; done
  for n in $(podman network ls -q --filter "$label" </dev/null); do podman network rm -f "$n" </dev/null >/dev/null; done
}

case "$cmd" in
  probe)
    shift 2
    echo "folder $([ -e "$dir" ] && echo 1 || echo 0)"
    echo "aside $([ -e "$aside" ] || [ -e "$vaside" ] && echo 1 || echo 0)"
    echo "volumes $(project_volumes | grep -c . || true)"
    for v in "$@"; do
      [[ "$v" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]*$ ]] || { echo "bad volume name: $v" >&2; exit 2; }
      if podman volume exists "$v" </dev/null; then echo "named $v"; fi
    done
    echo end
    ;;
  aside)
    [ ! -e "$aside" ] && [ ! -e "$vaside" ] || { echo "an earlier restore left $aside" >&2; exit 3; }
    remove_containers
    mkdir -p "$vaside"
    if [ -e "$dir" ]; then mv "$dir" "$aside"; else touch "$vaside/.no-folder"; fi
    for v in $(project_volumes); do
      mkdir "$vaside/$v"
      move_content "$(mountpoint_of "$v")" "$vaside/$v"
    done
    echo aside
    ;;
  extract)
    [ ! -e "$dir" ] || { echo "$dir exists" >&2; exit 3; }
    rm -rf --one-file-system "$staging"
    mkdir -p "$staging"
    # GNU tar refuses members with "..", strips leading "/", and creates symlinks only at the end, so
    # nothing is written outside the staging folder.
    tar -C "$staging" --numeric-owner --xattrs --xattrs-include='*' --acls -xpf -
    top=("$staging"/* "$staging"/.[!.]* "$staging"/..?*)
    if [ "${#top[@]}" != 1 ] || [ "${top[0]}" != "$staging/$id" ] || [ -L "${top[0]}" ] || [ ! -d "${top[0]}" ]; then
      echo "the archive does not hold just the folder $id/" >&2
      exit 4
    fi
    mv "$staging/$id" "$dir"
    rmdir "$staging"
    echo extracted
    ;;
  import)
    name="$3" v="$4"
    [[ "$name" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]*$ && "$v" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]*$ ]] || { echo "bad volume name: $name $v" >&2; exit 2; }
    if ! podman volume exists "$v" </dev/null; then
      # The labels docker-compose looks for, so `up` takes this volume as the project's own.
      podman volume create --label "com.docker.compose.project=$id" --label "com.docker.compose.volume=$name" "$v" </dev/null >/dev/null
    fi
    owner=$(podman volume inspect "$v" --format '{{index .Labels "com.docker.compose.project"}}' </dev/null)
    [ "$owner" = "$id" ] || { echo "volume $v does not belong to $id" >&2; exit 5; }
    # Empty: created just now, or its content was set aside.
    empty "$(mountpoint_of "$v")"
    podman volume import "$v" -
    echo imported
    ;;
  rollback)
    had_old="$3"
    remove_containers
    rm -rf --one-file-system "$staging"
    if [ "$had_old" != 1 ]; then
      # Nothing of this app existed before the restore: the folder and volumes are the restored ones.
      rm -rf --one-file-system "$dir"
      for v in $(project_volumes); do podman volume rm -f "$v" </dev/null >/dev/null; done
    elif [ -d "$vaside" ]; then
      # `aside` ran: put back what it moved. Without the set-aside folder (aside stopped before it
      # moved it), $dir is still the app's own folder and stays.
      if [ -e "$aside" ]; then
        rm -rf --one-file-system "$dir"
        mv "$aside" "$dir"
      elif [ -e "$vaside/.no-folder" ]; then
        rm -rf --one-file-system "$dir"
      fi
      # Volumes the restore created, which the app did not have before.
      for v in $(project_volumes); do
        if [ ! -d "$vaside/$v" ]; then podman volume rm -f "$v" </dev/null >/dev/null; fi
      done
      for d in "$vaside"/*/; do
        v=$(basename "$d")
        mp=$(mountpoint_of "$v")
        empty "$mp"
        move_content "$d" "$mp"
        rmdir "$d"
      done
      rm -f "$vaside/.no-folder"
      rmdir "$vaside"
    fi
    echo rolled back
    ;;
  commit)
    rm -rf --one-file-system "$aside" "$vaside"
    echo committed
    ;;
  *) echo "bad command: $cmd" >&2; exit 2 ;;
esac
