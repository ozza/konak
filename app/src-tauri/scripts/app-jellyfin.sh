# Installs and starts Jellyfin. Arg 1: the Windows media folder (e.g. C:\Users\me\Konak\Media).
# Config and cache live on the distro's ext4 disk; media is the visible Windows folder.
set -euo pipefail
media_win="$1"
dir=/var/lib/konak/apps/jellyfin
mkdir -p "$dir/config" "$dir/cache"
cd "$dir"
printf 'MEDIA_DIR=%s\n' "$(wslpath -u "$media_win")" > .env
cat > compose.yml <<'EOF'
# From the Jellyfin docs compose example; image pinned by digest.
services:
  jellyfin:
    image: docker.io/jellyfin/jellyfin@sha256:7da5184caa38c553f219f6772476540081f392a68049e2b68c923f6daf877f8e
    container_name: jellyfin
    ports:
      - 8096:8096/tcp
    volumes:
      - ./config:/config
      - ./cache:/cache
      - type: bind
        source: ${MEDIA_DIR}
        target: /media
    restart: unless-stopped
EOF
export DOCKER_HOST=unix:///run/podman/podman.sock
docker-compose -p jellyfin up -d --quiet-pull
for _ in $(seq 1 150); do
  if [ "$(curl -fsS http://localhost:8096/health 2>/dev/null)" = "Healthy" ]; then echo healthy; exit 0; fi
  sleep 2
done
echo "jellyfin did not become healthy in 5 minutes" >&2
exit 1
