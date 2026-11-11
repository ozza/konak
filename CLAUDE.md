# Konak

Native Windows 11 desktop app that turns the user's PC into a self-hosted service hub. It runs its own hidden WSL2 distro for Linux containers. The original idea is in `idea.md`, which is outdated and still uses the old name "OmniPanel". Decisions and the spike plan are in `.audit/feasibility-review.md`.

## Decisions
- Name: Konak.
- Target: Windows 11 x64 only for the MVP.
- VM layer: WSL2 with our own imported distro, not `podman machine`.
- Container engine: Podman, decided by the spike step 1 rule on 2026-11-11. Both engines ran the upstream compose files unmodified; Podman won because it does not change the shared WSL `FORWARD` policy. See `.audit/spike-results.md`.
- App naming, approved 2026-11-11, three levels:
  1. Default: mDNS `.local` names, editable by the user, with a conflict check before every announcement.
  2. Optional custom domain such as `home.arpa` or `ev.lan`. The user adds a wildcard DNS record pointing at the PC, Konak serves those names, and mDNS turns off.
  3. Fallback: IP address and port.
  No sub-path hosting (`konak.local/app`), because many apps break under it.
- LAN exposure: the installer adds Private-profile allow rules and Public-profile block rules for Konak's programs, so the Windows "allow access" popup never appears. Konak detects a Public home network and offers a one-click switch to Private.
- Kept from idea.md, unchallenged: Tauri shell, Caddy and the mDNS responder as native Windows processes, compose-format app definitions, remote access through a guided Tailscale install.

## Planned features
- **MCP server** (requested 2026-11-11). It lets AI agents do configuration, installs and troubleshooting through Konak, instead of driving PowerShell or WSL directly. The scope below is proposed, not yet confirmed:
  - **Placement:** it runs inside the tray agent (Rust), reusing the same code paths as the UI.
  - **Transport:** local stdio only by default. Nothing is reachable from the LAN.
  - **Read-only tools first:** app list and status, logs, network profile and firewall check, mDNS name check, disk and RAM usage, and a "diagnose" tool that runs the known spike checks:
    - shared-namespace `FORWARD` policy;
    - stale netavark rules;
    - distro keepalive;
    - port 80 owner;
    - LAN adapter choice.
  - **Write tools:** install, remove, restart, change settings, back up and restore. Each one needs explicit user confirmation in the Konak UI for destructive or outward-facing actions. Secrets are redacted from every tool output.

## Proposed, not final (confirm with the user before UI work starts)
- UI: Svelte + Tailwind CSS.
- Tray agent: Rust, sharing code with the Tauri core.

## Localization
Multi-language UI: Turkish and English. Set up i18n from the start, and keep every user-facing string in locale files. Default language: follow the Windows display language (Turkish if it is Turkish, otherwise English).

## Testing
- Development and the "side by side with Docker Desktop" tests run on the dev machine, which already has WSL.
- Clean-install tests run on a separate Windows 11 machine or VM.
