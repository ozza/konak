#!/bin/bash
# Step 4 part B, run in a freshly installed distro: restore Immich from the visible folder and verify.
set -eu
VIS=/mnt/c/Users/rust_/AppData/Local/Konak-spike/visible
export DOCKER_HOST=unix:///run/podman/podman.sock
mkdir -p /root/apps/immich; cp -a /mnt/j/Dev/self_hosting_project/spike/apps/immich/. /root/apps/immich/
cd /root/apps/immich
sed -i "s#^UPLOAD_LOCATION=.*#UPLOAD_LOCATION=$VIS/immich-library#" .env
docker-compose pull -q >/dev/null 2>&1
docker-compose create >/dev/null 2>&1
docker-compose start database >/dev/null 2>&1
until podman exec immich_postgres pg_isready -U postgres >/dev/null 2>&1; do sleep 2; done; sleep 5
gunzip --stdout "$VIS/backup/immich-db.sql.gz" \
 | sed "s/SELECT pg_catalog.set_config('search_path', '', false);/SELECT pg_catalog.set_config('search_path', 'public, pg_catalog', true);/g" \
 | podman exec -i immich_postgres psql --dbname=postgres --username=postgres >/root/restore.log 2>&1
echo "restore_errors=$(grep -c ERROR /root/restore.log || true)"
docker-compose up -d >/dev/null 2>&1
until curl -fsS -o /dev/null http://localhost:2283/api/server/ping; do sleep 2; done
TOKEN=$(curl -fsS -X POST http://localhost:2283/api/auth/login -H 'Content-Type: application/json' -d '{"email":"spike@konak.local","password":"spike-pass-1"}' | jq -r .accessToken)
echo "api_photos=$(curl -fsS http://localhost:2283/api/server/statistics -H "Authorization: Bearer $TOKEN" | jq .photos)"
cd "$VIS/immich-library" && sha256sum --quiet -c "$VIS/backup/checksums.txt" && echo "checksums=all_match"
ID=$(curl -fsS "http://localhost:2283/api/search/metadata" -X POST -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -d '{"size":1}' | jq -r '.assets.items[0].id')
curl -fsS -o /dev/null -w "original_download_http=%{http_code}\n" "http://localhost:2283/api/assets/$ID/original" -H "Authorization: Bearer $TOKEN"
