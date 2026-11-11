set -e
install -m 0644 /mnt/j/Dev/self_hosting_project/spike/distro/konak-boot.service /etc/systemd/system/konak-boot.service
systemctl disable podman-restart.service >/dev/null 2>&1 || true
systemctl enable konak-boot.service
