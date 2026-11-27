# Restore helpers for one app (see restore.rs). Args: command id [more]
#   probe id [podman volume names]   what exists: the folder, set-aside copies, the app's volumes
#   aside id                         removes the containers, moves the folder to <id>.before-restore
#                                    and every volume's content to <id>.before-restore-volumes/<vol>
#   extract id [bind sources]        stdin: GNU tar of the app folder (members <id>/...), unpacked in
#                                    a staging folder, checked (files and folders only; compose.yml
#                                    and .env files, the catalog's bind sources folders) and moved
#                                    into place
#   import id compose_name podman_name   stdin: `podman volume export` output for that volume
#   rollback id had_old              removes what the restore made; with had_old=1 moves the set-aside
#                                    folder and volume content back
#   commit id                        deletes the set-aside copies
#   stop id                          ends a still running extract or import of the app (Konak cut
#                                    its stream) and waits until it has ended
# Every command but extract and import keeps stdin closed; those two read the data from it, holding
# the app's lock meanwhile. rollback and stop end such a step and wait for the lock first, so a
# step whose wsl.exe was killed can never move a folder into place after a rollback.
set -euo pipefail
shopt -s nullglob
cmd="$1" id="$2"
[[ "$id" =~ ^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$ ]] || { echo "bad app id: $id" >&2; exit 2; }
# KONAK_ROOT only exists so the script can be tested outside the distro.
root=${KONAK_ROOT:-/var/lib/konak}
apps=$root/apps
dir=$apps/$id aside=$apps/$id.before-restore vaside=$apps/$id.before-restore-volumes
staging=$root/restore-staging/$id
run=${KONAK_RUN_DIR:-/run}
lockf=$run/konak-restore-$id.lock pidf=$run/konak-restore-$id.pid
export DOCKER_HOST=unix:///run/podman/podman.sock
label="label=com.docker.compose.project=$id"

project_volumes() { podman volume ls -q --filter "$label" </dev/null; }
mountpoint_of() { podman volume inspect "$1" --format '{{.Mountpoint}}' </dev/null; }
empty() { find "$1" -mindepth 1 -delete </dev/null; }
# Moves everything inside $1 (dotfiles too) into $2.
move_content() { find "$1" -mindepth 1 -maxdepth 1 -exec mv -t "$2" {} + </dev/null; }

# The app's lock, held by extract and import (and by whatever they start) until they end.
hold_lock() {
  exec 9>"$lockf"
  flock -w 600 9 || { echo "another restore step of $id is still running" >&2; exit 6; }
  echo $$ > "$pidf"
}

# TERM to a process and all its descendants (procps may not be installed: /proc is read directly).
kill_tree() {
  local p=$1 s line rest ppid k kids=()
  for s in /proc/[0-9]*/stat; do
    { read -r line < "$s"; } 2>/dev/null || continue
    rest=${line##*) }
    read -r _ ppid _ <<< "$rest"
    if [ "$ppid" = "$p" ]; then k=${s#/proc/}; kids+=("${k%/stat}"); fi
  done
  kill -TERM "$p" 2>/dev/null || true
  for k in "${kids[@]}"; do kill_tree "$k"; done
}

# Ends a running extract or import of this app and waits for its lock, then keeps it (fd 8) for
# the rest of this command. The pid in the pid file is signalled only while the lock is taken and
# only if that process holds the lock file open as its fd 9 (a stale pid may belong to another).
stop_running() {
  exec 8>"$lockf"
  flock -n 8 && return 0
  local p
  p=$(cat "$pidf" 2>/dev/null || true)
  if [[ "$p" =~ ^[0-9]+$ ]] && [ "$(readlink "/proc/$p/fd/9" 2>/dev/null)" = "$lockf" ]; then kill_tree "$p"; fi
  flock -w 120 8 || { echo "the running restore step of $id did not end" >&2; exit 6; }
}

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
    shift 2
    for b in "$@"; do [[ "$b" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]*$ ]] || { echo "bad bind source: $b" >&2; exit 2; }; done
    hold_lock
    [ ! -e "$dir" ] || { echo "$dir exists" >&2; exit 3; }
    rm -rf --one-file-system "$staging"
    # A fresh install has no app folder yet.
    mkdir -p "$staging" "$apps"
    # GNU tar refuses members with "..", strips leading "/", and creates symlinks only at the end, so
    # nothing is written outside the staging folder. Only user.* xattrs (and POSIX ACLs) come back:
    # trusted.* and security.* (file capabilities) are not taken from a backup file.
    tar -C "$staging" --numeric-owner --xattrs --xattrs-include='user.*' --acls -xpf -
    top=("$staging"/* "$staging"/.[!.]* "$staging"/..?*)
    if [ "${#top[@]}" != 1 ] || [ "${top[0]}" != "$staging/$id" ] || [ -L "${top[0]}" ] || [ ! -d "${top[0]}" ]; then
      echo "the archive does not hold just the folder $id/" >&2
      exit 4
    fi
    # Files and folders only. A symlink would turn a catalog bind mount (./forgejo:/data) into a
    # mount of anything in the distro, /mnt/c included, or send app-install.sh's root writes to its
    # target; device nodes, fifos and sockets have no place in an app folder either.
    bad=$(find "$staging/$id" \( -type l -o ! \( -type f -o -type d \) \) -print -quit </dev/null)
    [ -z "$bad" ] || { echo "the archive holds something other than files and folders: ${bad#"$staging/"}" >&2; exit 4; }
    # Hard links only between files of the folder itself: every link of a multiply linked file is in it.
    bad=$(find "$staging/$id" -type f -links +1 -printf '%i %n\n' </dev/null | sort | uniq -c | awk '$1 != $3 { print $2; exit }')
    [ -z "$bad" ] || { echo "the archive holds a hard link to a file outside the folder" >&2; exit 4; }
    # No setuid or setgid programs from a backup file.
    find "$staging/$id" -type f -perm /6000 -exec chmod ug-s {} + </dev/null
    for f in compose.yml .env .env.new; do
      p="$staging/$id/$f"
      [ ! -e "$p" ] || [ -f "$p" ] || { echo "$f is not a regular file" >&2; exit 4; }
    done
    for b in "$@"; do
      p="$staging/$id/$b"
      [ ! -e "$p" ] || [ -d "$p" ] || { echo "$b is not a folder" >&2; exit 4; }
    done
    mv "$staging/$id" "$dir"
    rmdir "$staging"
    echo extracted
    ;;
  import)
    name="$3" v="$4"
    [[ "$name" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]*$ && "$v" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]*$ ]] || { echo "bad volume name: $name $v" >&2; exit 2; }
    hold_lock
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
    stop_running
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
  stop)
    stop_running
    echo stopped
    ;;
  *) echo "bad command: $cmd" >&2; exit 2 ;;
esac
