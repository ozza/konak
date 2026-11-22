//! Installed apps and their operations, shared by the UI commands and the MCP server. Functions take
//! plain `&Paths` and never need Tauri. The supervisor follows changes on its own (it re-reads
//! `state.json` every tick); callers that run one can call `Supervisor::tick` to apply them at once.
//!
//! Errors that the UI can explain are short codes (`invalid_name`, `name_in_use`, ...; see the
//! `errors.*` locale keys). Other errors are technical text, already passed through the redactor.

use crate::catalog::{self, Manifest};
use crate::installer::{now, InstalledApp, State};
use crate::paths::Paths;
use crate::redact::Redactor;
use crate::services::{lan_ip, HOME_NAME};
use crate::shell::{self, tail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

const INSTALL_SH: &str = include_str!("../scripts/app-install.sh");
const REMOVE_SH: &str = include_str!("../scripts/app-remove.sh");
pub const MAX_LOG_LINES: u32 = 1000;

/// Serializes read-modify-write of `state.json` between app operations in this process.
static STATE_LOCK: Mutex<()> = Mutex::new(());

fn update_state(paths: &Paths, f: impl FnOnce(&mut State)) {
    let _guard = STATE_LOCK.lock().unwrap();
    let mut s = State::load(paths);
    f(&mut s);
    s.save(paths);
}

#[derive(Serialize, Clone, Debug)]
pub struct Container {
    pub name: String,
    /// Podman state: running, exited, created, paused, ...
    pub state: String,
    /// healthy, unhealthy or starting, when the image defines a health check.
    pub health: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct AppInfo {
    pub id: String,
    pub name: String,
    /// The `.local` URL; served on the LAN only once the app is claimed.
    pub url: String,
    /// `http://127.0.0.1:<port>`, reachable from this PC only. Used to finish an unclaimed app's
    /// first-run setup (a browser secure context, so Vaultwarden's web vault works there too).
    pub local_url: String,
    pub https: bool,
    pub port: u16,
    pub installed_unix: u64,
    pub claimed: bool,
    /// running | starting | unhealthy | stopped | missing (no containers) | unknown (distro unreadable)
    pub status: &'static str,
    pub containers: Vec<Container>,
}

/// The fields of `podman ps -a --format json` that Konak reads.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PsEntry {
    #[serde(default)]
    names: Vec<String>,
    #[serde(default)]
    state: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    labels: Option<HashMap<String, String>>,
}

/// Containers grouped by compose project (= app id), from `podman ps -a --format json` output.
pub fn parse_ps(json: &str) -> Result<HashMap<String, Vec<Container>>, String> {
    let entries: Vec<PsEntry> = serde_json::from_str(json.trim()).map_err(|e| format!("podman ps output unreadable: {e}"))?;
    let mut by_project: HashMap<String, Vec<Container>> = HashMap::new();
    for e in entries {
        let Some(project) = e.labels.as_ref().and_then(|l| l.get("com.docker.compose.project")) else { continue };
        let health = ["unhealthy", "healthy", "starting"].into_iter().find(|h| e.status.contains(&format!("({h})"))).map(String::from);
        by_project.entry(project.clone()).or_default().push(Container { name: e.names.join(","), state: e.state, health });
    }
    Ok(by_project)
}

pub fn status_of(containers: &[Container]) -> &'static str {
    let health = |h: &str| containers.iter().any(|c| c.health.as_deref() == Some(h));
    if containers.is_empty() {
        "missing"
    } else if health("unhealthy") {
        "unhealthy"
    } else if containers.iter().any(|c| c.state != "running") {
        "stopped"
    } else if health("starting") {
        "starting"
    } else {
        "running"
    }
}

fn podman_ps(paths: &Paths) -> Result<HashMap<String, Vec<Container>>, String> {
    let out = shell::run("wsl.exe", &["-d", &paths.distro, "-u", "root", "--", "podman", "ps", "-a", "--format", "json"]);
    if !out.ok {
        return Err(format!("podman ps failed: {}", tail(&out.text, 3)));
    }
    parse_ps(&out.text)
}

pub fn url(m: &Manifest, name: &str) -> String {
    format!("{}://{name}.local", if m.https { "https" } else { "http" })
}

/// Installed apps with live container status. When the distro cannot be read, status is `unknown`.
pub fn list(paths: &Paths) -> Vec<AppInfo> {
    let state = State::load(paths);
    let ps = podman_ps(paths).map_err(|e| crate::log(paths, &e)).ok();
    state
        .apps
        .iter()
        .filter_map(|a| {
            let m = catalog::get(&a.id)?;
            let containers = ps.as_ref().map(|p| p.get(&a.id).cloned().unwrap_or_default());
            Some(AppInfo {
                id: a.id.clone(),
                name: a.name.clone(),
                url: url(m, &a.name),
                local_url: format!("http://127.0.0.1:{}", m.port),
                https: m.https,
                port: m.port,
                installed_unix: a.installed_unix,
                claimed: a.claimed,
                status: containers.as_deref().map(status_of).unwrap_or("unknown"),
                containers: containers.unwrap_or_default(),
            })
        })
        .collect()
}

/// Local checks for a new name of app `id`: syntax, reserved, unique among installed apps.
pub fn validate_name(state: &State, id: &str, name: &str) -> Result<(), String> {
    if !catalog::valid_label(name) {
        return Err("invalid_name".into());
    }
    if name == HOME_NAME {
        return Err("name_reserved".into());
    }
    if state.apps.iter().any(|a| a.id != id && a.name == name) {
        return Err("name_in_use".into());
    }
    Ok(())
}

/// Asks the LAN whether another device already answers to `<name>.local`.
pub fn name_free_on_lan(paths: &Paths, name: &str) -> Result<bool, String> {
    let ip = lan_ip().ok_or("no_lan_ip")?;
    let out = shell::run(&paths.mdns().to_string_lossy(), &[&ip, "1", name, "--check-only"]);
    crate::log(paths, &format!("name check {name}: {}", tail(&out.text, 1)));
    Ok(out.ok)
}

/// First free name from `base`, `base-2`, ... `base-5`.
fn pick_name(paths: &Paths, state: &State, id: &str, base: &str) -> Result<String, String> {
    for i in 1..=5 {
        let name = if i == 1 { base.to_string() } else { format!("{base}-{i}") };
        if validate_name(state, id, &name).is_ok() && name_free_on_lan(paths, &name)? {
            return Ok(name);
        }
    }
    Err(format!("names {base} to {base}-5 are all taken on this network"))
}

fn b64(s: &str) -> String {
    if s.is_empty() {
        "-".into()
    } else {
        base64::engine::general_purpose::STANDARD.encode(s)
    }
}

/// Where Caddy's requests come from, as the apps see them: Caddy connects to 127.0.0.1:<port> on
/// Windows, WSL relays that to the distro's loopback, and Podman's port forward then masquerades it
/// to the gateway of the app's compose network. Compose networks get /24s from Podman's first default
/// subnet pool, and the WSL VM's network namespace is shared with other distros, so the exact /24
/// varies (10.89.4.1 was measured); the pool is the narrowest stable choice. Apps trust
/// X-Forwarded-For only from here, and Caddy overwrites that header with the real client address.
pub const PROXY_NET: &str = "10.89.0.0/16";

/// Values written to the app's `.env` by Konak (secrets and mounts are added by the script).
/// `claimed_env` is added only once the app is claimed.
pub fn env_lines(m: &Manifest, name: &str, claimed: bool) -> String {
    let (host, url) = (format!("{name}.local"), url(m, name));
    let mut lines = vec![format!("KONAK_HOST={host}"), format!("KONAK_URL={url}"), format!("KONAK_PORT={}", m.port), format!("KONAK_PROXY_NET={PROXY_NET}")];
    let claimed_env = m.claimed_env.iter().filter(|_| claimed);
    for (k, v) in m.env.iter().chain(claimed_env) {
        lines.push(format!("{k}={}", v.replace("${KONAK_HOST}", &host).replace("${KONAK_URL}", &url).replace("${KONAK_PROXY_NET}", PROXY_NET)));
    }
    lines.join("\n")
}

fn redacted(paths: &Paths, text: &str) -> String {
    Redactor::load(paths).apply(text)
}

/// Renders and starts the app under `name`; with `hook`, runs it after the app is healthy.
fn run_install(paths: &Paths, m: &Manifest, name: &str, claimed: bool, hook: Option<&str>) -> Result<(), String> {
    let mut mounts = String::new();
    for (var, kind) in &m.mounts {
        let dir = if kind == "photos" { &paths.photos } else { &paths.media };
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        mounts += &format!("{var}\t{}\n", dir.display());
    }
    let port = m.port.to_string();
    let secrets = if m.secrets.is_empty() { "-".to_string() } else { m.secrets.join(" ") };
    let args = [&m.id, port.as_str(), &m.health_path, &b64(m.compose), &b64(&env_lines(m, name, claimed)), &secrets, &b64(&mounts), &b64(hook.unwrap_or(""))];
    let out = shell::distro_script(&paths.distro, INSTALL_SH, &args);
    crate::log(paths, &format!("install {} as {name}: {}", m.id, redacted(paths, &tail(&out.text, 2))));
    if !out.ok {
        return Err(redacted(paths, &format!("install of {} failed: {}", m.id, tail(&out.text, 6))));
    }
    Ok(())
}

fn manifest(id: &str) -> Result<&'static Manifest, String> {
    catalog::get(id).ok_or_else(|| "unknown_app".into())
}

fn installed(paths: &Paths, id: &str) -> Result<InstalledApp, String> {
    State::load(paths).apps.into_iter().find(|a| a.id == id).ok_or_else(|| "not_installed".into())
}

/// Installs an app, or re-renders and restarts an installed one under its current name (repair).
pub fn install(paths: &Paths, id: &str) -> Result<InstalledApp, String> {
    let m = manifest(id)?;
    let state = State::load(paths);
    let (name, claimed) = match state.apps.iter().find(|a| a.id == id) {
        Some(a) => (a.name.clone(), a.claimed),
        None => (pick_name(paths, &state, id, &m.default_name)?, false),
    };
    run_install(paths, m, &name, claimed, None)?;
    let mut result = None;
    update_state(paths, |s| {
        s.rerender.retain(|r| r != id);
        let app = match s.apps.iter().find(|a| a.id == id) {
            Some(a) => a.clone(),
            None => {
                // New apps stay off the LAN until the user has created the admin account (`claim`).
                let a = InstalledApp { id: id.into(), name: name.clone(), installed_unix: now(), claimed: false };
                s.apps.push(a.clone());
                a
            }
        };
        result = Some(app);
    });
    Ok(result.unwrap())
}

/// Removes the app's containers; `delete_data` also deletes its data in the distro. Windows folders
/// (media, photos) are never deleted.
pub fn remove(paths: &Paths, id: &str, delete_data: bool) -> Result<(), String> {
    manifest(id)?;
    let mode = if delete_data { "--delete-data" } else { "keep" };
    let out = shell::distro_script(&paths.distro, REMOVE_SH, &[id, mode]);
    crate::log(paths, &format!("remove {id} ({mode}): {}", tail(&out.text, 2)));
    if !out.ok {
        return Err(redacted(paths, &format!("remove of {id} failed: {}", tail(&out.text, 6))));
    }
    update_state(paths, |s| {
        s.apps.retain(|a| a.id != id);
        s.rerender.retain(|r| r != id);
    });
    Ok(())
}

/// Renames an installed app: local checks, LAN conflict check, then re-renders its env (and runs the
/// manifest's `post_rename` hook). On failure the old name is rendered back.
pub fn rename(paths: &Paths, id: &str, new_name: &str) -> Result<(), String> {
    let m = manifest(id)?;
    let app = installed(paths, id)?;
    validate_name(&State::load(paths), id, new_name)?;
    if app.name == new_name {
        return Ok(());
    }
    if !name_free_on_lan(paths, new_name)? {
        return Err("name_taken_on_lan".into());
    }
    if let Err(e) = run_install(paths, m, new_name, app.claimed, m.post_rename.as_deref()) {
        let _ = run_install(paths, m, &app.name, app.claimed, m.post_rename.as_deref());
        return Err(e);
    }
    update_state(paths, |s| {
        if let Some(a) = s.apps.iter_mut().find(|a| a.id == id) {
            a.name = new_name.to_string();
        }
    });
    Ok(())
}

/// Marks an app as claimed (its admin account exists), which gives it its `.local` name and Caddy
/// route at the supervisor's next tick. The app is first re-rendered with its `claimed_env` (e.g.
/// registration closed) and is marked claimed only when that succeeds, so it never reaches the LAN
/// with its pre-claim settings.
pub fn claim(paths: &Paths, id: &str) -> Result<(), String> {
    let m = manifest(id)?;
    let app = installed(paths, id)?;
    run_install(paths, m, &app.name, true, None)?;
    update_state(paths, |s| {
        if let Some(a) = s.apps.iter_mut().find(|a| a.id == id) {
            a.claimed = true;
        }
    });
    Ok(())
}

/// Runs a docker-compose command for an installed app in its directory.
fn compose(paths: &Paths, id: &str, args: &str) -> shell::Out {
    let script = format!("set -e\ncd /var/lib/konak/apps/\"$1\"\nexport DOCKER_HOST=unix:///run/podman/podman.sock\ndocker-compose -p \"$1\" {args}\n");
    shell::distro_script(&paths.distro, &script, &[id])
}

pub fn restart(paths: &Paths, id: &str) -> Result<(), String> {
    installed(paths, id)?;
    let out = compose(paths, id, "restart");
    if !out.ok {
        return Err(redacted(paths, &format!("restart of {id} failed: {}", tail(&out.text, 6))));
    }
    Ok(())
}

/// Last `lines` log lines of all the app's containers (at most `MAX_LOG_LINES`), redacted.
pub fn logs(paths: &Paths, id: &str, lines: u32) -> Result<String, String> {
    installed(paths, id)?;
    let n = lines.clamp(1, MAX_LOG_LINES);
    let out = compose(paths, id, &format!("logs --no-color --tail {n}"));
    let text = redacted(paths, &out.text);
    if !out.ok {
        return Err(format!("logs of {id} failed: {}", tail(&text, 6)));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(apps: &[(&str, &str)]) -> State {
        let mut s = State::default();
        s.apps = apps.iter().map(|(id, name)| InstalledApp { id: id.to_string(), name: name.to_string(), installed_unix: 0, claimed: true }).collect();
        s
    }

    #[test]
    fn name_validation() {
        let s = state_with(&[("jellyfin", "movies"), ("immich", "photos")]);
        assert!(validate_name(&s, "jellyfin", "films").is_ok());
        assert!(validate_name(&s, "jellyfin", "movies").is_ok(), "keeping its own name");
        assert_eq!(validate_name(&s, "jellyfin", "photos").unwrap_err(), "name_in_use");
        assert_eq!(validate_name(&s, "jellyfin", "konak").unwrap_err(), "name_reserved");
        for bad in ["", "Movies", "-a", "a-", "a.b", "a b", &"x".repeat(64)] {
            assert_eq!(validate_name(&s, "jellyfin", bad).unwrap_err(), "invalid_name", "{bad}");
        }
    }

    #[test]
    fn claim_is_not_recorded_when_the_rerender_fails() {
        let home = std::env::temp_dir().join(format!("konak-claim-test-{}", std::process::id()));
        let paths = Paths { home: home.clone(), distro: "konak-nonexistent".into(), bin: home.join("bin"), media: home.join("m"), photos: home.join("p") };
        let mut s = State::default();
        s.apps.push(InstalledApp { id: "forgejo".into(), name: "git".into(), installed_unix: 1, claimed: false });
        s.save(&paths);
        let not_installed = claim(&paths, "immich").unwrap_err();
        // The distro does not exist, so the re-render with claimed_env fails.
        let failed = claim(&paths, "forgejo");
        let after = State::load(&paths);
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(not_installed, "not_installed");
        assert!(failed.is_err());
        assert!(!after.apps[0].claimed, "claimed only after a successful re-render");
    }

    #[test]
    fn container_status() {
        let json = r#"[
          {"Names":["jellyfin"],"State":"running","Status":"Up 2 minutes (healthy)","Labels":{"com.docker.compose.project":"jellyfin"}},
          {"Names":["immich_server"],"State":"running","Status":"Up 1 minute (unhealthy)","Labels":{"com.docker.compose.project":"immich"}},
          {"Names":["immich_db"],"State":"running","Status":"Up 1 minute","Labels":{"com.docker.compose.project":"immich"}},
          {"Names":["vw"],"State":"exited","Status":"Exited (0) 1 minute ago","Labels":{"com.docker.compose.project":"vault"}},
          {"Names":["fj"],"State":"running","Status":"Up 3 seconds (starting)","Labels":{"com.docker.compose.project":"forgejo"}},
          {"Names":["other"],"State":"running","Status":"Up","Labels":null}
        ]"#;
        let p = parse_ps(json).unwrap();
        assert_eq!(p.len(), 4);
        assert_eq!(status_of(&p["jellyfin"]), "running");
        assert_eq!(status_of(&p["immich"]), "unhealthy");
        assert_eq!(status_of(&p["vault"]), "stopped");
        assert_eq!(status_of(&p["forgejo"]), "starting");
        assert_eq!(status_of(&[]), "missing");
        assert_eq!(p["jellyfin"][0].health.as_deref(), Some("healthy"));
        assert!(parse_ps("[]").unwrap().is_empty());
    }

    #[test]
    fn env_is_templated() {
        let mut m = catalog::get("jellyfin").unwrap().clone();
        m.env.insert("PUBLIC_URL".into(), "${KONAK_URL}/x".into());
        m.env.insert("HOST".into(), "${KONAK_HOST}".into());
        let env = env_lines(&m, "movies", true);
        assert!(env.contains("KONAK_HOST=movies.local\nKONAK_URL=http://movies.local\nKONAK_PORT=47101\nKONAK_PROXY_NET=10.89.0.0/16"));
        assert!(env.contains("PUBLIC_URL=http://movies.local/x"));
        assert!(env.contains("HOST=movies.local"));
    }

    #[test]
    fn apps_trust_only_the_proxy_net() {
        let env = |id: &str| env_lines(catalog::get(id).unwrap(), id, true);
        assert!(env("nextcloud").lines().any(|l| l == "TRUSTED_PROXIES=10.89.0.0/16"));
        assert!(env("nextcloud").lines().any(|l| l == "FORWARDED_FOR_HEADERS=HTTP_X_FORWARDED_FOR"));
        assert!(env("immich").lines().any(|l| l == "IMMICH_TRUSTED_PROXIES=10.89.0.0/16"));
        let compose = |id: &str| catalog::get(id).unwrap().compose;
        assert!(compose("forgejo").contains("FORGEJO__security__REVERSE_PROXY_TRUSTED_PROXIES=${KONAK_PROXY_NET}"));
        assert!(compose("forgejo").contains("FORGEJO__security__REVERSE_PROXY_LIMIT=1"));
        assert!(compose("vaultwarden").contains("IP_HEADER: X-Forwarded-For"));
        for m in catalog::all() {
            assert!(!m.compose.contains("0.0.0.0/0") && !m.env.values().any(|v| v.contains("0.0.0.0/0")), "{}", m.id);
        }
    }

    #[test]
    fn claimed_env_only_after_claim() {
        let m = catalog::get("forgejo").unwrap();
        assert_eq!(m.claimed_env["FORGEJO_DISABLE_REGISTRATION"], "true");
        assert!(!env_lines(m, "git", false).contains("FORGEJO_DISABLE_REGISTRATION"));
        assert!(env_lines(m, "git", true).lines().any(|l| l == "FORGEJO_DISABLE_REGISTRATION=true"));
        // The compose file falls back to open registration while the app is unclaimed.
        assert!(m.compose.contains("FORGEJO__service__DISABLE_REGISTRATION=${FORGEJO_DISABLE_REGISTRATION:-false}"));
    }
}
