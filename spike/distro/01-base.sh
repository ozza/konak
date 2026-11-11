set -e
cat /etc/os-release | grep PRETTY
printf '[boot]\nsystemd=true\n[interop]\nappendWindowsPath=false\n' > /etc/wsl.conf
cat /etc/wsl.conf
