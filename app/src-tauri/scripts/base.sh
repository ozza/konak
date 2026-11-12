# Distro base config: systemd on, Windows PATH not appended. Takes effect after the distro restarts.
set -euo pipefail
printf '[boot]\nsystemd=true\n[interop]\nappendWindowsPath=false\n' > /etc/wsl.conf
grep PRETTY_NAME /etc/os-release
