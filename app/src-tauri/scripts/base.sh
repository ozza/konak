# Distro base config: systemd on, and no interop, so nothing in the distro (a container escape, a
# compromised app) can start Windows programs. Konak needs none: it drives the distro from Windows,
# and wslpath and the /mnt/c mounts work without interop. Takes effect after the distro restarts.
set -euo pipefail
printf '[boot]\nsystemd=true\n[interop]\nenabled=false\nappendWindowsPath=false\n' > /etc/wsl.conf
grep PRETTY_NAME /etc/os-release
