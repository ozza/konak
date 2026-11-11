#!/bin/bash
# Step 4 part A: Immich with its library in a visible Windows folder (drvfs) and Postgres on the distro disk.
# Uploads test photos, records checksums and asset count, dumps the database next to the library.
set -eu
VIS=/mnt/c/Users/rust_/AppData/Local/Konak-spike/visible
export DOCKER_HOST=unix:///run/podman/podman.sock
cd /root/apps/immich && docker-compose down >/dev/null 2>&1 || true
rm -rf /root/apps/immich/library /root/apps/immich/postgres "$VIS"; mkdir -p "$VIS/immich-library" "$VIS/backup"
sed -i "s#^UPLOAD_LOCATION=.*#UPLOAD_LOCATION=$VIS/immich-library#" /root/apps/immich/.env
docker-compose up -d >/dev/null 2>&1
until curl -fsS -o /dev/null http://localhost:2283/api/server/ping; do sleep 2; done
command -v convert >/dev/null || (apt-get install -y -qq imagemagick >/dev/null)
mkdir -p /root/photos; for i in $(seq 1 30); do convert -size 800x600 plasma:fractal "/root/photos/img$i.jpg"; done
curl -fsS -X POST http://localhost:2283/api/auth/admin-sign-up -H 'Content-Type: application/json' -d '{"email":"spike@konak.local","password":"spike-pass-1","name":"Spike"}' >/dev/null
TOKEN=$(curl -fsS -X POST http://localhost:2283/api/auth/login -H 'Content-Type: application/json' -d '{"email":"spike@konak.local","password":"spike-pass-1"}' | jq -r .accessToken)
echo "$TOKEN" > /root/immich-token
n=0; for f in /root/photos/*.jpg; do
  now=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  curl -fsS -X POST http://localhost:2283/api/assets -H "Authorization: Bearer $TOKEN" \
    -F "assetData=@$f" -F "deviceAssetId=$(basename $f)" -F deviceId=spike -F "fileCreatedAt=$now" -F "fileModifiedAt=$now" >/dev/null && n=$((n+1))
done
echo "uploaded=$n"
sleep 20
COUNT=$(curl -fsS http://localhost:2283/api/server/statistics -H "Authorization: Bearer $TOKEN" | jq .photos)
echo "api_photos=$COUNT"
(cd "$VIS/immich-library" && find . -type f -path '*upload*' -o -type f -path '*library*' | sort | xargs -r sha256sum) > "$VIS/backup/checksums.txt"
echo "checksummed_files=$(wc -l < $VIS/backup/checksums.txt)"
podman exec -t immich_postgres pg_dumpall --clean --if-exists --username=postgres | gzip > "$VIS/backup/immich-db.sql.gz"
ls -la "$VIS/backup"
