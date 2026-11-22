# Installs one catalog app, or re-renders it. Idempotent: generated secrets are kept, and
# `up -d` recreates only containers whose config changed (a rename re-renders KONAK_HOST/KONAK_URL
# this way). Empty arguments are passed as "-" because wsl.exe may drop empty strings.
# Args: id port health_path compose_b64 env_b64 secrets mounts_b64 [post_hook_b64]
#   env_b64:    KEY=VALUE lines, already templated by Konak (KONAK_HOST, KONAK_URL, KONAK_PORT, ...)
#   secrets:    space-separated var names; a missing one gets `openssl rand -hex 32`
#   mounts_b64: "VAR<TAB>C:\Windows\path" lines, turned into Linux paths with wslpath
set -euo pipefail
dash() { [ "$1" = "-" ] && echo "" || echo "$1"; }
id="$1" port="$2" health="$3"
compose=$(dash "$4" | base64 -d) env=$(dash "$5" | base64 -d) secrets=$(dash "$6")
mounts=$(dash "$7" | base64 -d) hook=$(dash "${8:--}" | base64 -d)
[[ "$id" =~ ^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$ ]] || { echo "bad app id: $id" >&2; exit 2; }
[[ "$port" =~ ^[0-9]+$ ]] || { echo "bad port: $port" >&2; exit 2; }

# KONAK_APPS_DIR only exists so the script can be tested outside the distro.
dir=${KONAK_APPS_DIR:-/var/lib/konak/apps}/$id
mkdir -p "$dir"
cd "$dir"
printf '%s\n' "$compose" > compose.yml
(
  umask 077
  {
    if [ -n "$env" ]; then printf '%s\n' "$env"; fi
    while IFS=$'\t' read -r var win; do
      # A plain assignment, so a wslpath failure stops the script (errexit skips printf arguments).
      if [ -n "$var" ]; then linux=$(wslpath -u "$win"); printf '%s=%s\n' "$var" "$linux"; fi
    done <<< "$mounts"
    for s in $secrets; do
      old=$(grep -m1 "^$s=" .env 2>/dev/null | cut -d= -f2- || true)
      printf '%s=%s\n' "$s" "${old:-$(openssl rand -hex 32)}"
    done
  } > .env.new
  chmod 600 .env.new
  mv -f .env.new .env
)

export DOCKER_HOST=unix:///run/podman/podman.sock
docker-compose -p "$id" up -d --quiet-pull --remove-orphans
healthy=0
for _ in $(seq 1 150); do
  if curl -fsS -o /dev/null --max-time 5 "http://127.0.0.1:$port$health"; then healthy=1; break; fi
  sleep 2
done
[ "$healthy" = 1 ] || { echo "$id did not answer on $health within 5 minutes" >&2; exit 1; }

if [ -n "$hook" ]; then
  # .env is exported line by line instead of sourced, so values are never run as shell code.
  while IFS= read -r line; do if [ -n "$line" ]; then export "$line"; fi; done < .env
  bash -euo pipefail -c "$hook"
fi
echo healthy
