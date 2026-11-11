set -e
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq podman curl ca-certificates jq >/dev/null
podman --version
systemctl enable --now podman.socket
systemctl enable podman-restart.service
V=$(curl -fsSL https://api.github.com/repos/docker/compose/releases/latest | jq -r .tag_name)
curl -fsSL -o /usr/local/bin/docker-compose "https://github.com/docker/compose/releases/download/$V/docker-compose-linux-x86_64"
chmod +x /usr/local/bin/docker-compose
docker-compose version
echo 'export DOCKER_HOST=unix:///run/podman/podman.sock' > /etc/profile.d/podman-docker-host.sh
DOCKER_HOST=unix:///run/podman/podman.sock docker-compose version >/dev/null && curl -s --unix-socket /run/podman/podman.sock http://d/_ping; echo
