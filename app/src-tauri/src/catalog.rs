//! The app catalog, embedded in the binary: `catalog/<id>/app.json` (the manifest) and
//! `catalog/<id>/compose.yml` (the upstream compose file, images pinned by digest). Display text
//! lives in the locale files under `apps.<id>.*`, not here.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::OnceLock;

macro_rules! entry {
    ($id:literal) => {
        ($id, include_str!(concat!("../catalog/", $id, "/app.json")), include_str!(concat!("../catalog/", $id, "/compose.yml")))
    };
}

/// Adding an app: create `catalog/<id>/` and add one line here.
const ENTRIES: &[(&str, &str, &str)] = &[entry!("jellyfin"), entry!("immich"), entry!("nextcloud"), entry!("vaultwarden"), entry!("forgejo")];

/// Host ports for apps come from this block: below the Windows dynamic range (49152+) and away from
/// ports that other software on the PC commonly holds (3000, 8080, 8096, ...).
pub const PORT_RANGE: std::ops::RangeInclusive<u16> = 47100..=47199;

/// Windows folders an app may mount. Keys are what manifests write in `mounts`.
pub const MOUNT_KINDS: [&str; 2] = ["media", "photos"];

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    pub default_name: String,
    pub port: u16,
    #[serde(default)]
    pub https: bool,
    pub health_path: String,
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Env var name -> Windows folder kind (one of `MOUNT_KINDS`).
    #[serde(default)]
    pub mounts: BTreeMap<String, String>,
    pub min_ram_mb: u32,
    /// Env var name -> value; `${KONAK_HOST}` (`name.local`), `${KONAK_HOSTPORT}` (with the web port
    /// when it is not the default), `${KONAK_URL}` and `${KONAK_PROXY_NET}` are filled in at install time.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Like `env`, but written only once the app is claimed (its admin account exists), e.g. to
    /// close open registration. Compose files read these with a `${NAME:-default}` fallback.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub claimed_env: BTreeMap<String, String>,
    /// Bash run in the app directory (with `.env` loaded) after a rename has recreated the containers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_rename: Option<String>,
    #[serde(skip)]
    pub compose: &'static str,
}

/// `.local` name rule, also used for app ids: a DNS label in lower case.
pub fn valid_label(s: &str) -> bool {
    let b = s.as_bytes();
    let edge = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    !b.is_empty() && b.len() <= 63 && edge(b[0]) && edge(b[b.len() - 1]) && b.iter().all(|&c| edge(c) || c == b'-')
}

fn valid_env_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty() && b[0].is_ascii_uppercase() && b.iter().all(|&c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
}

pub fn parse(id: &str, json: &str, compose: &'static str) -> Result<Manifest, String> {
    let mut m: Manifest = serde_json::from_str(json).map_err(|e| format!("{id}: {e}"))?;
    m.compose = compose;
    let err = |msg: &str| Err(format!("{id}: {msg}"));
    if m.id != id || !valid_label(&m.id) {
        return err("id must match its folder and be a DNS label");
    }
    if !valid_label(&m.default_name) || m.default_name == "konak" {
        return err("default_name must be a DNS label other than konak");
    }
    if !PORT_RANGE.contains(&m.port) {
        return err("port must be inside 47100-47199");
    }
    // It ends up in a curl URL inside the distro: no query strings, quotes or shell characters.
    let path_char = |c: char| c.is_ascii_alphanumeric() || "/._-".contains(c);
    if !m.health_path.starts_with('/') || !m.health_path.chars().all(path_char) {
        return err("health_path must be an absolute path of A-Z a-z 0-9 / . _ -");
    }
    // KONAK_* names are set by Konak itself.
    let mut names: Vec<&String> = m.secrets.iter().chain(m.mounts.keys()).chain(m.env.keys()).chain(m.claimed_env.keys()).collect();
    if let Some(bad) = names.iter().find(|n| !valid_env_name(n) || n.starts_with("KONAK_")) {
        return err(&format!("bad env var name {bad}"));
    }
    let total = names.len();
    names.sort();
    names.dedup();
    if names.len() != total {
        // Duplicates across secrets/mounts/env/claimed_env would silently override each other in .env.
        return err("an env var name is used twice");
    }
    if let Some((_, kind)) = m.mounts.iter().find(|(_, k)| !MOUNT_KINDS.contains(&k.as_str())) {
        return err(&format!("unknown mount kind {kind}"));
    }
    if m.env.values().chain(m.claimed_env.values()).any(|v| v.contains('\n') || v.contains('\r')) {
        return err("env values must be single-line");
    }
    for line in compose.lines().map(str::trim) {
        if let Some(image) = line.strip_prefix("image:") {
            if !image.contains("@sha256:") {
                return err(&format!("image not pinned by digest: {}", image.trim()));
            }
        }
        if line.contains("${KONAK_PORT}") && !line.contains("127.0.0.1:${KONAK_PORT}") {
            return err("the app port must bind 127.0.0.1");
        }
    }
    Ok(m)
}

pub fn all() -> &'static [Manifest] {
    static CATALOG: OnceLock<Vec<Manifest>> = OnceLock::new();
    // Embedded files are validated by the unit tests, so a failure here is a build mistake.
    CATALOG.get_or_init(|| ENTRIES.iter().map(|(id, json, compose)| parse(id, json, compose).expect("invalid catalog entry")).collect())
}

pub fn get(id: &str) -> Option<&'static Manifest> {
    all().iter().find(|m| m.id == id)
}

impl Manifest {
    /// The app-folder entries its containers bind-mount (`./forgejo:/data`, `DB_DATA_LOCATION:
    /// ./postgres`): the first path component of every `./...` bind source in compose.yml and of
    /// every `./...` env value.
    pub fn bind_sources(&self) -> Vec<String> {
        let from_compose = self.compose.lines().filter_map(|l| {
            let l = l.trim();
            l.strip_prefix("- ./").or_else(|| l.strip_prefix("source: ./")).map(|r| r.split([':', '/']).next().unwrap_or_default())
        });
        let from_env = self.env.values().filter_map(|v| v.strip_prefix("./").map(|r| r.split('/').next().unwrap_or_default()));
        let mut out: Vec<String> = from_compose.chain(from_env).filter(|s| !s.is_empty()).map(String::from).collect();
        out.sort();
        out.dedup();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"{"id":"demo","default_name":"demo","port":47150,"health_path":"/","min_ram_mb":256,
        "secrets":["DB_PASSWORD"],"mounts":{"MEDIA_DIR":"media"},"env":{"URL":"${KONAK_URL}"}}"#;
    const COMPOSE: &str = "services:\n  a:\n    image: x/y@sha256:abc\n    ports:\n      - 127.0.0.1:${KONAK_PORT}:80\n";

    #[test]
    fn parses_a_valid_manifest() {
        let m = parse("demo", GOOD, COMPOSE).unwrap();
        assert_eq!(m.port, 47150);
        assert!(!m.https);
        assert_eq!(m.mounts["MEDIA_DIR"], "media");
        assert!(m.claimed_env.is_empty());
        let closed = parse("demo", &GOOD.replace(r#""min_ram_mb""#, r#""claimed_env":{"CLOSED":"true"},"min_ram_mb""#), COMPOSE).unwrap();
        assert_eq!(closed.claimed_env["CLOSED"], "true");
    }

    #[test]
    fn rejects_bad_manifests() {
        let bad = |from: &str, to: &str| parse("demo", &GOOD.replace(from, to), COMPOSE).is_err();
        assert!(parse("other", GOOD, COMPOSE).is_err(), "id must match folder");
        assert!(bad("47150", "8096"), "port outside block");
        assert!(bad(r#""default_name":"demo""#, r#""default_name":"konak""#));
        assert!(bad(r#""health_path":"/""#, r#""health_path":"health""#));
        for path in ["/a b", "/x?y=1", "/x&y", "/$(id)", "/a;b", "/a'b", "/a\\\"b", "/ü"] {
            assert!(bad(r#""health_path":"/""#, &format!(r#""health_path":"{path}""#)), "{path}");
        }
        assert!(!bad(r#""health_path":"/""#, r#""health_path":"/api/v1.0/health_check-x""#));
        assert!(bad("\"media\"", "\"desktop\""), "unknown mount kind");
        assert!(bad("DB_PASSWORD", "KONAK_HOST"), "reserved env name");
        assert!(bad("DB_PASSWORD", "MEDIA_DIR"), "duplicate env name");
        assert!(bad("\"min_ram_mb\"", "\"typo\":1,\"min_ram_mb\""), "unknown field");
        let claimed = |entry: &str| bad(r#""min_ram_mb""#, &format!(r#""claimed_env":{{{entry}}},"min_ram_mb""#));
        assert!(claimed(r#""URL":"x""#), "claimed_env name already in env");
        assert!(claimed(r#""lower_case":"x""#), "claimed_env name not upper case");
        assert!(claimed(r#""KONAK_X":"x""#), "claimed_env name reserved");
        assert!(claimed(r#""CLOSED":"a\nb""#), "claimed_env value multi-line");
        assert!(parse("demo", GOOD, "image: x/y:latest\n").is_err(), "unpinned image");
        assert!(parse("demo", GOOD, "image: x@sha256:a\nports:\n  - ${KONAK_PORT}:80\n").is_err(), "port on all interfaces");
    }

    #[test]
    fn catalog_is_valid() {
        let all = all();
        assert!(!all.is_empty());
        let mut ports: Vec<u16> = all.iter().map(|m| m.port).collect();
        ports.sort();
        ports.dedup();
        assert_eq!(ports.len(), all.len(), "ports must be unique");
        for m in all {
            assert!(m.compose.contains("@sha256:"), "{} has no pinned image", m.id);
            assert!(PORT_RANGE.contains(&m.port));
        }
    }

    #[test]
    fn bind_sources_of_the_catalog() {
        let binds = |id: &str| get(id).unwrap().bind_sources();
        assert_eq!(binds("forgejo"), ["forgejo"]);
        assert_eq!(binds("jellyfin"), ["cache", "config"]);
        assert_eq!(binds("vaultwarden"), ["vw-data"]);
        assert_eq!(binds("immich"), ["postgres"]);
        assert!(binds("nextcloud").is_empty());
    }

    #[test]
    fn labels() {
        for ok in ["a", "jellyfin", "my-app2", &"a".repeat(63)] {
            assert!(valid_label(ok), "{ok}");
        }
        for bad in ["", "-a", "a-", "A", "a_b", "a.b", &"a".repeat(64), "ü"] {
            assert!(!valid_label(bad), "{bad}");
        }
    }
}
