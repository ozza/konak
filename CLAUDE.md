# Konak

Native Windows 11 desktop app that turns the user's PC into a self-hosted service hub. It runs its own hidden WSL2 distro for Linux containers. The original idea is in `idea.md`, which is outdated and still uses the old name "OmniPanel". Decisions and the spike plan are in `.audit/feasibility-review.md`.

## Decisions
- Name: Konak.
- Target: Windows 11 x64 only for the MVP.
- VM layer: WSL2 with our own imported distro, not `podman machine`.
- Container engine: Podman, decided by the spike step 1 rule on 2026-11-11. Both engines ran the upstream compose files unmodified; Podman won because it does not change the shared WSL `FORWARD` policy. See `.audit/spike-results.md`.
- Kept from idea.md, unchallenged: Tauri shell, Caddy and the mDNS responder as native Windows processes, compose-format app definitions, remote access through a guided Tailscale install.

## Proposed, not final (confirm with the user before UI work starts)
- UI: Svelte + Tailwind CSS.
- Tray agent: Rust, sharing code with the Tauri core.

## Localization
Multi-language UI: Turkish and English. Set up i18n from the start, and keep every user-facing string in locale files. Default language: follow the Windows display language (Turkish if it is Turkish, otherwise English).

## Testing
- Development and the "side by side with Docker Desktop" tests run on the dev machine, which already has WSL.
- Clean-install tests run on a separate Windows 11 machine or VM.
