//! Where Konak keeps its files. Environment overrides exist so the installer can be exercised on the
//! development machine without touching a real install (see `app/README.md`).

use std::path::PathBuf;

#[derive(Clone)]
pub struct Paths {
    /// Konak's data folder: state, logs, generated configs and the distro disk.
    pub home: PathBuf,
    pub distro: String,
    /// Folder with the bundled Windows binaries (caddy.exe, mdns-responder.exe).
    pub bin: PathBuf,
    /// Visible media folder the apps can read, under the user's profile.
    pub media: PathBuf,
    /// Visible photo library folder (Immich uploads), under the user's profile.
    pub photos: PathBuf,
}

impl Paths {
    pub fn new(resource_dir: PathBuf) -> Self {
        Self::from_env(resource_dir, &|k| std::env::var(k).ok())
    }

    /// For `--uninstall`: the dev overrides apply only with `KONAK_SKIP_ELEVATION=1`, so a stray
    /// `KONAK_DISTRO` or `KONAK_HOME` in the environment cannot point a real uninstall (which may
    /// unregister the distro and delete the home) at something else.
    pub fn for_uninstall(resource_dir: PathBuf) -> Self {
        Self::from_env(resource_dir, &uninstall_env(&|k| std::env::var(k).ok()))
    }

    fn from_env(resource_dir: PathBuf, var: &dyn Fn(&str) -> Option<String>) -> Self {
        let local = var("LOCALAPPDATA").unwrap_or_else(|| ".".into());
        let profile = var("USERPROFILE").unwrap_or_else(|| ".".into());
        let home = var("KONAK_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(&local).join("Konak"));
        let distro = var("KONAK_DISTRO").unwrap_or_else(|| "konak".into());
        let media = var("KONAK_MEDIA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(&profile).join("Konak").join("Media"));
        let photos = var("KONAK_PHOTOS").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(&profile).join("Konak").join("Photos"));
        Paths { home, distro, bin: resource_dir.join("bin"), media, photos }
    }
    pub fn state_file(&self) -> PathBuf { self.home.join("state.json") }
    pub fn log_dir(&self) -> PathBuf { self.home.join("logs") }
    pub fn install_log(&self) -> PathBuf { self.log_dir().join("install.log") }
    pub fn distro_dir(&self) -> PathBuf { self.home.join("wsl") }
    pub fn caddyfile(&self) -> PathBuf { self.home.join("Caddyfile") }
    /// Caddy's data (certificates it issued, its intermediate) stays under the Konak home.
    pub fn caddy_storage(&self) -> PathBuf { self.home.join("caddy") }
    /// Static files Caddy serves on konak.local: the home page and the root certificate.
    pub fn site_dir(&self) -> PathBuf { self.home.join("site") }
    pub fn ca_dir(&self) -> PathBuf { self.home.join("ca") }
    pub fn ca_cert(&self) -> PathBuf { self.ca_dir().join("root.crt") }
    pub fn ca_key(&self) -> PathBuf { self.ca_dir().join("root.key") }
    /// Supervisor heartbeat, read by the MCP diagnose tool.
    pub fn supervisor_file(&self) -> PathBuf { self.home.join("supervisor.json") }
    pub fn caddy(&self) -> PathBuf { self.bin.join("caddy.exe") }
    pub fn mdns(&self) -> PathBuf { self.bin.join("mdns-responder.exe") }
}

pub fn skip_elevation() -> bool {
    std::env::var("KONAK_SKIP_ELEVATION").is_ok_and(|v| v == "1")
}

const OVERRIDES: [&str; 4] = ["KONAK_HOME", "KONAK_DISTRO", "KONAK_MEDIA", "KONAK_PHOTOS"];

/// The environment as `--uninstall` sees it: without `KONAK_SKIP_ELEVATION=1`, the path overrides
/// read as unset.
fn uninstall_env<'a>(var: &'a dyn Fn(&str) -> Option<String>) -> impl Fn(&str) -> Option<String> + 'a {
    let dev = var("KONAK_SKIP_ELEVATION").as_deref() == Some("1");
    move |k| if !dev && OVERRIDES.contains(&k) { None } else { var(k) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uninstall_ignores_overrides_unless_dev() {
        let env = |skip: Option<&'static str>| {
            move |k: &str| match k {
                "LOCALAPPDATA" => Some(r"C:\Users\u\AppData\Local".to_string()),
                "USERPROFILE" => Some(r"C:\Users\u".to_string()),
                "KONAK_HOME" => Some(r"D:\elsewhere".to_string()),
                "KONAK_DISTRO" => Some("konak-podman".to_string()),
                "KONAK_MEDIA" => Some(r"D:\m".to_string()),
                "KONAK_PHOTOS" => Some(r"D:\p".to_string()),
                "KONAK_SKIP_ELEVATION" => skip.map(String::from),
                _ => None,
            }
        };
        let real = Paths::from_env(PathBuf::from("res"), &uninstall_env(&env(None)));
        assert_eq!(real.home, PathBuf::from(r"C:\Users\u\AppData\Local\Konak"));
        assert_eq!(real.distro, "konak");
        assert_eq!(real.media, PathBuf::from(r"C:\Users\u\Konak\Media"));
        assert_eq!(real.photos, PathBuf::from(r"C:\Users\u\Konak\Photos"));
        assert_eq!(Paths::from_env(PathBuf::from("res"), &uninstall_env(&env(Some("0")))).distro, "konak");

        let dev = Paths::from_env(PathBuf::from("res"), &uninstall_env(&env(Some("1"))));
        assert_eq!((dev.home, dev.distro), (PathBuf::from(r"D:\elsewhere"), "konak-podman".to_string()));
        // The app itself keeps honoring them.
        assert_eq!(Paths::from_env(PathBuf::from("res"), &env(None)).distro, "konak-podman");
    }
}
