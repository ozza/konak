#!/bin/bash
# Usage: bench.sh <engine-label> <app> <health-url>
# Runs the upstream compose file for <app> unmodified and records timings as one JSON line.
set -u
ENGINE=$1 APP=$2 URL=$3
SRC=/mnt/j/Dev/self_hosting_project/spike/apps/$APP
WORK=/root/apps/$APP
if [ "$ENGINE" = podman ]; then export DOCKER_HOST=unix:///run/podman/podman.sock; C="docker-compose"; else C="docker compose"; fi
rm -rf "$WORK"; mkdir -p "$WORK"; cp -a "$SRC/." "$WORK/"; cd "$WORK"
# Install-time secret generation, as Konak would do: replace upstream 'change-me' placeholders in the local copy only.
for f in .env *.env; do [ -f "$f" ] && sed -i "s/=change-me\$/=$(head -c 48 /dev/urandom | base64 | tr -dc A-Za-z0-9)/" "$f"; done
LOG=/root/bench-$APP.log
t0=$(date +%s)
$C pull >"$LOG" 2>&1; pull_rc=$?
t1=$(date +%s)
$C up -d >>"$LOG" 2>&1; up_rc=$?
healthy=0
for i in $(seq 1 300); do
  if curl -fsS -o /dev/null --max-time 3 "$URL"; then healthy=1; break; fi
  sleep 2
done
t2=$(date +%s)
mem=$(free -m | awk '/Mem:/{print $3}')
states=$($C ps --format '{{.Service}}={{.State}}/{{.Health}}' 2>/dev/null | tr '\n' ' ')
echo "{\"engine\":\"$ENGINE\",\"app\":\"$APP\",\"pull_rc\":$pull_rc,\"pull_s\":$((t1-t0)),\"up_rc\":$up_rc,\"healthy\":$healthy,\"to_healthy_s\":$((t2-t1)),\"vm_used_mb\":$mem,\"services\":\"$states\"}"
