# Installs one catalog app, or re-renders it. Idempotent: generated secrets are kept, and
# `up -d` recreates only containers whose config changed (a rename re-renders KONAK_HOST/KONAK_URL
# this way). Empty arguments are passed as "-" because wsl.exe may drop empty strings.
# Args: id port health_path compose_b64 env_b64 secrets mounts_b64 [post_hook_b64] [mode]
#   env_b64:    KEY=VALUE lines, already templated by Konak (KONAK_HOST, KONAK_URL, KONAK_PORT, ...)
#   secrets:    space-separated var names; a missing one gets `openssl rand -hex 32`
#   mounts_b64: "VAR<TAB>C:\Windows\path" lines, turned into Linux paths with wslpath
#   mode:       up (default) starts the app; render only writes compose.yml and .env, so a stopped
#               app stays stopped (its next `up -d` uses them)
set -euo pipefail
dash() { [ "$1" = "-" ] && echo "" || echo "$1"; }
id="$1" port="$2" health="$3"
compose=$(dash "$4" | base64 -d) env=$(dash "$5" | base64 -d) secrets=$(dash "$6")
mounts=$(dash "$7" | base64 -d) hook=$(dash "${8:--}" | base64 -d) mode="${9:-up}"
[[ "$id" =~ ^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$ ]] || { echo "bad app id: $id" >&2; exit 2; }
[[ "$port" =~ ^[0-9]+$ ]] || { echo "bad port: $port" >&2; exit 2; }
[[ "$mode" = up || "$mode" = render ]] || { echo "bad mode: $mode" >&2; exit 2; }

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
      val=$(grep -m1 "^$s=" .env 2>/dev/null | cut -d= -f2- || true)
      if [ -z "$val" ]; then
        # Plain assignments, so a failing openssl stops the script instead of writing an empty secret.
        val=$(openssl rand -hex 32)
        [[ "$val" =~ ^[0-9a-f]{64}$ ]] || { echo "could not generate $s" >&2; exit 1; }
      fi
      printf '%s=%s\n' "$s" "$val"
    done
  } > .env.new
  chmod 600 .env.new
  mv -f .env.new .env
)
if [ "$mode" = render ]; then echo rendered; exit 0; fi

export DOCKER_HOST=unix:///run/podman/podman.sock
# Progress goes to stdout with the rest, so the success line at the end is the output's last line
# (Konak reads stdout, then stderr).
docker-compose -p "$id" up -d --quiet-pull --remove-orphans 2>&1

# "<container IP>:<container port>" of the app's container that publishes host port $port, or
# nothing yet (container not created or not running). The probe goes to the container directly:
# under WSL mirrored networking, traffic the distro sends to its own 127.0.0.1:<port> never gets
# netavark's masquerade (WSL's WSLOUTPUT chain overwrites the packet mark), so the container answers
# its own loopback and the connection hangs. The container address works in NAT and mirrored mode.
probe_target() {
  local c
  for c in $(podman ps -q --filter "label=com.docker.compose.project=$id" 2>/dev/null); do
    podman inspect "$c" --format '{{range $p, $b := .NetworkSettings.Ports}}{{range $b}}P {{.HostPort}} {{$p}}{{println}}{{end}}{{end}}{{range .NetworkSettings.Networks}}I {{.IPAddress}}{{println}}{{end}}' 2>/dev/null |
      awk -v hp="$port" '$1 == "P" && $2 == hp && $3 ~ /\/tcp$/ { split($3, a, "/"); cp = a[1] } $1 == "I" && $2 != "" && ip == "" { ip = $2 } END { if (cp != "" && ip != "") print ip ":" cp }'
  done | head -1
}

# A wall-clock deadline: a black-holed port makes every curl hang until its own timeout.
# The Host header stays what the app saw when it was probed on 127.0.0.1:<port>.
# Failed probes are expected while the app starts: only the last one's error is kept, and it is
# printed only when the app never answers.
healthy=0 err="" start=$SECONDS
deadline=$((SECONDS + 300))
while [ "$SECONDS" -lt "$deadline" ]; do
  target=$(probe_target || true)
  if err=$(curl -fsS -o /dev/null --max-time 3 -H "Host: 127.0.0.1:$port" "http://${target:-127.0.0.1:$port}$health" 2>&1); then healthy=1; break; fi
  sleep 2
done
[ "$healthy" = 1 ] || { [ -z "$err" ] || echo "$err" >&2; echo "$id did not answer on $health within 5 minutes" >&2; exit 1; }

if [ -n "$hook" ]; then
  # .env is exported line by line instead of sourced, so values are never run as shell code.
  while IFS= read -r line; do if [ -n "$line" ]; then export "$line"; fi; done < .env
  bash -euo pipefail -c "$hook" 2>&1
fi
echo "healthy after $((SECONDS - start)) s"
