# Konak app

Tauri 2 + SvelteKit (SPA) + Tailwind CSS. It started as the spike step 5 installer and may become the product skeleton.

What it does today:
- **Installs the platform from scratch:**
  - one UAC prompt (WSL when missing, firewall rules, an optional switch to the Private network profile);
  - a reboot when WSL needs one, after which it resumes on its own;
  - its own Debian WSL distro with systemd, Podman, pinned docker-compose and Konak's boot unit;
  - Caddy and the mDNS responder on the LAN IP, serving Konak's home page at `http://konak.local`.
- **Installs apps from the embedded catalog** (`src-tauri/catalog/<id>/`: `app.json` + upstream `compose.yml`, images pinned by digest). Each app gets a fixed host port in 47100–47199 bound to 127.0.0.1 in the distro and a `.local` name proxied by Caddy. Apps with `"https": true` are served with `tls internal` from Konak's own root CA, which is name-constrained to `.local`.
- **Then stays in the tray** and supervises the distro keepalive, Caddy and mDNS every 10 s, including when the LAN IP or the app list changes.

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
  - the generated `Caddyfile`, Caddy's storage (`caddy\`) and the `konak.local` page (`site\`);
  - the root CA (`ca\root.crt`, `ca\root.key`; the folder is readable by the current user only);
  - the supervisor heartbeat (`supervisor.json`);
  - the distro disk (`wsl\`).
- App data lives in the distro under `/var/lib/konak/apps/<id>/` (generated secrets in `.env`, mode 600).
- Media goes in `%USERPROFILE%\Konak\Media`, photos in `%USERPROFILE%\Konak\Photos`.
- Distro name: `konak`.
- Autostart: `HKCU\...\Run\Konak` = `"<exe>" --autostart`.

## Running on a development machine
These environment variables keep a dev run away from a real install:

| Variable | Effect |
|---|---|
| `KONAK_HOME` | Data folder instead of `%LOCALAPPDATA%\Konak` |
| `KONAK_DISTRO` | Distro name instead of `konak` |
| `KONAK_MEDIA` | Media folder |
| `KONAK_PHOTOS` | Photos folder |
| `KONAK_SKIP_ELEVATION=1` | Skips the UAC step and autostart registration. WSL must already be installed, and no firewall rules are added, so other LAN devices cannot reach the app. |
| `KONAK_DEV_AUTO_INSTALL=1` | Starts the install at launch, without a click |

Port 80 and 443 on the LAN IP must be free. Stop the spike's Caddy and its `konak-podman` distro first.

### Checking the local CA against Caddy
`cargo test write_root_and_caddyfile -- --ignored` with `KONAK_CA_CHECK_DIR` set writes a root and a
Caddyfile on 127.0.0.1:18080/18443 into that folder. Run `resources/bin/caddy.exe run --config <dir>/Caddyfile`,
then `openssl s_client -connect 127.0.0.1:18443 -servername test.local -CAfile <dir>/ca/root.crt -verify_hostname test.local`
must verify, and the same for `outside.example` must fail with "permitted subtree violation".

## MCP server
`konak.exe --mcp` runs a headless MCP server on stdin/stdout (official Rust SDK `rmcp`), so AI agents can inspect Konak instead of driving PowerShell or WSL themselves.
- **Local stdio only.** No network listener; the client starts the process.
- **Reads never change anything,** and none starts the Konak distro: when it is stopped, tools say "distro not running".
- **Every write waits for a person.** The write tools forward each request to the running Konak window, which
  asks the user to approve it. Nothing runs without that click. Without a running Konak they fail with
  "Konak is not running".
- **Redacted.** Every string in every reply passes through the same redactor as the UI.

Register it with an MCP client, for example Claude Code:
`claude mcp add konak -- "<install folder>\konak.exe" --mcp`. The per-user installer's default folder is `%LOCALAPPDATA%\Konak`.
The dev environment variables above (`KONAK_HOME`, `KONAK_DISTRO`) apply to it too.

| Tool | Returns |
|---|---|
| `konak_status` | Setup steps, distro state, installed apps with status, URL and containers, supervisor heartbeat age (stale after 60 s) |
| `app_logs {app, lines?}` | Last log lines of an app, by id or name (default 200, at most 1000) |
| `network_check` | LAN IP, its adapter and network category, Konak's firewall rules for `caddy.exe` and `mdns-responder.exe`, the owner of port 80 on the LAN IP |
| `mdns_check {name}` | Name syntax, use by Konak, and whether another LAN device answers to `<name>.local` |
| `resources` | Free space on the Konak drive, distro disk size, host RAM, memory and disk inside the distro when running |
| `diagnose` | ok/warn/fail, evidence and a suggested fix for: shared-namespace `FORWARD` policy, stale netavark rules, distro keepalive, port 80 owner, LAN adapter choice |

Write tools, each confirmed in the Konak window: `install_app {app}`, `remove_app {app, delete_data?}`,
`start_app {app}`, `stop_app {app}`, `restart_app {app}`, `rename_app {app, name}`, `claim_app {app}`.
A call waits for the answer (5 minutes) and then for the operation (up to 15 minutes) and returns
`done`, `failed`, `denied`, `busy`, `invalid` or `running`.

How the confirmation works (`src-tauri/src/pipe.rs`, `confirm.rs`, `src/lib/panel/McpConfirm.svelte`):
- `konak.exe --mcp` sends each request over a named pipe, `\\.\pipe\konak-<user hash>-<distro>`, to the
  running Konak. The pipe accepts only the current Windows user, local clients and medium integrity or
  higher. The client checks that the server is the same konak.exe file running as the same user.
- Konak validates the request first: an unknown app, an invalid name, an app busy with another
  operation or a request that would change nothing is refused without a dialog.
- The dialog names the requesting program (as it reports itself, unverified), the action and its
  consequence. Deny has the focus, Approve arms after 1.5 s, and Escape, closing the window or no answer
  in 5 minutes deny. An identical request denied in the last 2 minutes is refused without a dialog.
- Every request and its outcome is written to `install.log`.
- Limit: the click guards this channel only. A program running as the same user can still act outside
  it, for example by running `wsl` commands itself or by automating clicks; that is inherent to a
  same-user design.

## Uninstall
Run the uninstaller from Windows Settings. Before it removes any file, it asks one question: also
delete all Konak data? The default is No. Silent (`/S`) and passive runs take No without asking, and
updates (`/UPDATE`) skip the cleanup. Then it runs `konak.exe --uninstall [--purge]`
(`src-tauri/src/uninstall.rs`, hooked in through `src-tauri/windows/hooks.nsh`):

- `--uninstall` stops a running Konak and the caddy, mdns-responder and wsl keepalive it started.
  They are matched by exact executable path and command line, never by name. It then runs
  `wsl --terminate` on Konak's distro, removes `HKCU\...\Run\Konak` only if it points at this install,
  and removes the Konak root from the CurrentUser Root store if its thumbprint is there (Windows asks to
  confirm). It also removes the `Konak` firewall group with one UAC prompt, only if such rules exist.
- `--purge` also runs `wsl --unregister` on the distro and deletes the Konak home. The default home is
  also the install folder, so the installed files are left for the uninstaller. A path guard refuses
  any folder that holds the Media/Photos folders, the user profile, AppData or Temp, and any folder that
  is neither the default home nor a folder with Konak's `state.json`.
- The Media and Photos folders under the user profile are never deleted.
- Without `--purge` the data, the distro and the CA stay in `%LOCALAPPDATA%\Konak`. The uninstaller
  deletes only its own files there, so a reinstall picks up where it left off.
- Every action is logged to `%TEMP%\konak-uninstall-<unix time>.log`, and a summary is printed on stdout.

Dev runs use the same overrides as above (`KONAK_HOME`, `KONAK_DISTRO`, `KONAK_MEDIA`, `KONAK_PHOTOS`),
but only together with `KONAK_SKIP_ELEVATION=1`; otherwise `--uninstall` ignores them and works on
the default home and the `konak` distro.
With `KONAK_SKIP_ELEVATION=1`, the Run value, the certificate store and the firewall steps are
skipped and logged as skipped.
