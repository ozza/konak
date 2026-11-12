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
}

impl Paths {
    pub fn new(resource_dir: PathBuf) -> Self {
        let local = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
        let profile = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
        let home = std::env::var("KONAK_HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(&local).join("Konak"));
        let distro = std::env::var("KONAK_DISTRO").unwrap_or_else(|_| "konak".into());
        let media = std::env::var("KONAK_MEDIA").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(&profile).join("Konak").join("Media"));
        Paths { home, distro, bin: resource_dir.join("bin"), media }
    }
    pub fn state_file(&self) -> PathBuf { self.home.join("state.json") }
    pub fn log_dir(&self) -> PathBuf { self.home.join("logs") }
    pub fn install_log(&self) -> PathBuf { self.log_dir().join("install.log") }
    pub fn distro_dir(&self) -> PathBuf { self.home.join("wsl") }
    pub fn caddyfile(&self) -> PathBuf { self.home.join("Caddyfile") }
    pub fn caddy(&self) -> PathBuf { self.bin.join("caddy.exe") }
    pub fn mdns(&self) -> PathBuf { self.bin.join("mdns-responder.exe") }
}

pub fn skip_elevation() -> bool {
    std::env::var("KONAK_SKIP_ELEVATION").is_ok_and(|v| v == "1")
}
