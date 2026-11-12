# Konak app

Tauri 2 + SvelteKit (SPA) + Tailwind CSS. It started as the spike step 5 installer and may become the product skeleton.

What it does today:
- **Installs Jellyfin from scratch:**
  - one UAC prompt (WSL when missing, firewall rules, an optional switch to the Private network profile);
  - a reboot when WSL needs one, after which it resumes on its own;
  - its own Debian WSL distro with systemd, Podman, pinned docker-compose and Konak's boot unit;
  - Jellyfin, pinned by digest;
  - Caddy and the mDNS responder on the LAN IP.
- **Then stays in the tray** and supervises the distro keepalive, Caddy and mDNS every 10 s, including when the LAN IP changes.

## Build
1. `npm install`
2. `powershell -File scripts/prepare-bins.ps1`
   - Downloads Caddy and checks its checksum.
   - Builds `spike/mdns-responder`.
   - Puts both into `src-tauri/resources/bin/`, which is git-ignored.
3. `npm run tauri build`. The result is a per-user NSIS installer at `src-tauri/target/release/bundle/nsis/`.

## Where things live (installed)
- `%LOCALAPPDATA%\Konak`:
  - state (`state.json`);
  - logs (`logs/install.log`, `caddy.log`, `mdns.log`);
  - the generated `Caddyfile`;
  - the distro disk (`wsl\`).
- Media goes in `%USERPROFILE%\Konak\Media`.
- Distro name: `konak`.
- Autostart: `HKCU\...\Run\Konak` = `"<exe>" --autostart`.

## Running on a development machine
These environment variables keep a dev run away from a real install:

| Variable | Effect |
|---|---|
| `KONAK_HOME` | Data folder instead of `%LOCALAPPDATA%\Konak` |
| `KONAK_DISTRO` | Distro name instead of `konak` |
| `KONAK_MEDIA` | Media folder |
| `KONAK_SKIP_ELEVATION=1` | Skips the UAC step and autostart registration. WSL must already be installed, and no firewall rules are added, so other LAN devices cannot reach the app. |
| `KONAK_DEV_AUTO_INSTALL=1` | Starts the install at launch, without a click |

Port 80 on the LAN IP and port 8096 must be free. Stop the spike's Caddy and its `konak-podman` distro first.

## Uninstall (manual for now)
1. Quit from the tray.
2. Run the uninstaller from Windows Settings.
3. `wsl --unregister konak`
4. Delete `%LOCALAPPDATA%\Konak`.
5. Delete the `Konak` firewall rule group (admin).
6. Delete the `HKCU\...\Run\Konak` value.
