# Container engine: Podman with its API socket, plus the official docker-compose binary (pinned and
# checksum-verified), plus Konak's boot unit that restarts containers and rebuilds their netavark rules.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
COMPOSE_VERSION=v5.7.1

apt-get update -qq
apt-get install -y -qq podman curl ca-certificates >/dev/null
systemctl enable --now podman.socket

# Konak's own subnet pool for compose networks. All WSL distros share one network namespace, so
# another Podman install on the default pool (10.89.0.0/16) would get the same subnets and
# aardvark-dns gateway addresses, and whichever starts second cannot start its containers.
# 10.234.0.0/16 is a private range that home routers, VPNs, WSL and other container tools rarely
# use. PROXY_NET in apps.rs must match it.
mkdir -p /etc/containers/containers.conf.d
cat > /etc/containers/containers.conf.d/konak.conf <<'EOF'
[network]
default_subnet_pools = [
  {"base" = "10.234.0.0/16", "size" = 24},
]
EOF

cd /tmp
base="https://github.com/docker/compose/releases/download/$COMPOSE_VERSION"
curl -fsSL -o docker-compose "$base/docker-compose-linux-x86_64"
curl -fsSL -o docker-compose.sha256 "$base/docker-compose-linux-x86_64.sha256"
echo "$(cut -d' ' -f1 docker-compose.sha256)  docker-compose" | sha256sum -c - >/dev/null
install -m 0755 docker-compose /usr/local/bin/docker-compose
rm -f docker-compose docker-compose.sha256

cat > /etc/systemd/system/konak-boot.service <<'EOF'
[Unit]
Description=Konak: start restartable containers, then rebuild their netavark rules
After=network-online.target podman.socket
[Service]
Type=oneshot
RemainAfterExit=true
ExecStart=/usr/bin/podman start --all --filter restart-policy=always
ExecStart=/usr/bin/podman start --all --filter restart-policy=unless-stopped
# Rebuild rules for Konak's own networks only. Dropping the whole `inet netavark` table would also
# wipe the port forwards of any other Podman distro, since all WSL distros share one network namespace.
ExecStartPost=-/usr/bin/podman network reload --all
[Install]
WantedBy=default.target
EOF
systemctl disable podman-restart.service >/dev/null 2>&1 || true
systemctl enable konak-boot.service >/dev/null 2>&1
podman --version
DOCKER_HOST=unix:///run/podman/podman.sock docker-compose version --short
