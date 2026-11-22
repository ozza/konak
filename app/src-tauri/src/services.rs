//! Keeps Konak's runtime pieces alive while the app runs (in the tray):
//! - a keepalive session so WSL never idles the distro out;
//! - Caddy on the LAN IP: `konak.local` (home page and root certificate) plus one name per app;
//! - the mDNS responder announcing all those names.
//!
//! Spike step 3 showed that checking only at logon is not enough (an external `wsl --terminate`
//! left everything down), so this loop checks every few seconds. Each tick re-reads the installed
//! apps from `state.json`, so a change made anywhere (UI, MCP, another process) is picked up; Caddy
//! and mDNS restart only when the app list or the LAN IP changes.

use crate::installer::State;
use crate::paths::Paths;
use crate::{catalog, shell};
use serde::Serialize;
use std::net::UdpSocket;
use std::path::Path;
use std::process::Child;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_secs(10);
pub const HOME_NAME: &str = "konak";

/// One proxied app name.
#[derive(Clone, PartialEq, Debug)]
pub struct Route {
    pub id: String,
    pub name: String,
    pub port: u16,
    pub https: bool,
}

impl Route {
    pub fn url(&self) -> String {
        format!("{}://{}.local", if self.https { "https" } else { "http" }, self.name)
    }
}

/// Routes for the installed apps that are still in the catalog and claimed. An unclaimed app is
/// reachable only from this PC (`http://127.0.0.1:<port>`) until its admin account exists.
pub fn routes(state: &State) -> Vec<Route> {
    routes_checked(state).0
}

/// Routes, plus the ids of apps left out because their name is already taken (by an earlier app or
/// the home page). Caddy rejects a config with two sites of the same name, so one bad entry in
/// `state.json` must never reach it and take down every route.
pub fn routes_checked(state: &State) -> (Vec<Route>, Vec<String>) {
    let mut seen = vec![HOME_NAME.to_string()];
    let (mut routes, mut skipped) = (vec![], vec![]);
    for a in state.apps.iter().filter(|a| a.claimed) {
        let Some(m) = catalog::get(&a.id) else { continue };
        if seen.contains(&a.name) {
            skipped.push(a.id.clone());
            continue;
        }
        seen.push(a.name.clone());
        routes.push(Route { id: a.id.clone(), name: a.name.clone(), port: m.port, https: m.https });
    }
    (routes, skipped)
}

/// Files the generated Caddyfile points at.
pub struct CaddyFiles<'a> {
    pub storage: &'a Path,
    pub site: &'a Path,
    /// Root certificate and key; `None` when the CA is unavailable, which drops the HTTPS sites
    /// (never replaced by an unconstrained CA).
    pub ca: Option<(&'a Path, &'a Path)>,
}

fn q(p: &Path) -> String {
    format!("\"{}\"", p.to_string_lossy().replace('\\', "/"))
}

pub fn caddyfile(ip: &str, routes: &[Route], f: &CaddyFiles) -> String {
    // skip_install_trust: Caddy must never touch the Windows trust store itself (it would pop a
    // dialog); trusting the root is an explicit user action (ca::trust_on_this_pc). persist_config off:
    // no autosave.json in %APPDATA%\Caddy, the Caddyfile is the only config.
    let mut s = format!("{{\n\tadmin off\n\tpersist_config off\n\tauto_https disable_redirects\n\tskip_install_trust\n\tstorage file_system {}\n", q(f.storage));
    if let Some((cert, key)) = f.ca {
        s += &format!("\tpki {{\n\t\tca local {{\n\t\t\troot {{\n\t\t\t\tcert {}\n\t\t\t\tkey {}\n\t\t\t}}\n\t\t}}\n\t}}\n", q(cert), q(key));
    }
    s += "}\n";
    s += &format!("http://{HOME_NAME}.local:80 {{\n\tbind {ip}\n\troot * {}\n\tfile_server\n}}\n", q(f.site));
    // Caddy already replaces X-Forwarded-For with the client address; X-Real-IP it would pass through
    // as the client sent it, and some apps (Nextcloud's Apache, Vaultwarden's default) trust it.
    let proxy = |port: u16| format!("reverse_proxy 127.0.0.1:{port} {{\n\t\theader_up X-Real-IP {{remote_host}}\n\t}}");
    for r in routes {
        let host = format!("{}.local", r.name);
        if !r.https {
            s += &format!("http://{host}:80 {{\n\tbind {ip}\n\t{}\n}}\n", proxy(r.port));
        } else if f.ca.is_some() {
            s += &format!("https://{host}:443 {{\n\tbind {ip}\n\ttls internal\n\t{}\n}}\n", proxy(r.port));
            s += &format!("http://{host}:80 {{\n\tbind {ip}\n\tredir https://{host}{{uri}} permanent\n}}\n");
        }
    }
    // Any other name (or the bare IP) gets a 404 instead of Caddy's empty 200. Caddy matches the
    // named sites first. On 443 only when HTTPS sites exist, so the catch-all never brings up a
    // listener of its own; it issues no certificate, so an unknown SNI still fails the handshake
    // and only a request whose Host differs from its SNI reaches it.
    s += &format!("http://:80 {{\n\tbind {ip}\n\trespond 404\n}}\n");
    if f.ca.is_some() && routes.iter().any(|r| r.https) {
        s += &format!("https://:443 {{\n\tbind {ip}\n\ttls internal\n\trespond 404\n}}\n");
    }
    s
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The static `konak.local` page: installed apps with links, and the root certificate download. It is
/// served over plain HTTP, so it never shows the fingerprint (a LAN attacker could change both the
/// file and the number); it points the user at the Konak window on the PC instead.
pub fn home_page(routes: &[Route], text: &dyn Fn(&str) -> String) -> String {
    let items: String = if routes.is_empty() {
        format!("<li>{}</li>", esc(&text("home.no_apps")))
    } else {
        routes
            .iter()
            .map(|r| {
                let label = text(&format!("apps.{}.name", r.id));
                format!("<li><a href=\"{0}\">{1}</a> <span>{0}</span></li>", esc(&r.url()), esc(&label))
            })
            .collect()
    };
    format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>Konak</title><style>body{{font-family:system-ui,sans-serif;max-width:40rem;margin:2rem auto;padding:0 1rem;line-height:1.5}}\
li{{margin:.4rem 0}}span{{color:#666;font-family:monospace;margin-left:.5rem}}</style></head><body>\
<h1>Konak</h1><h2>{}</h2><ul>{}</ul><h2>{}</h2><p>{}</p><p><a href=\"/konak-root.crt\" download>{}</a></p><p>{}</p></body></html>\n",
        esc(&text("home.apps")),
        items,
        esc(&text("home.cert_title")),
        esc(&text("home.cert_body")),
        esc(&text("home.cert_link")),
        esc(&text("home.cert_check")),
    )
}

#[derive(Serialize, Default)]
struct Pids {
    keepalive: Option<u32>,
    caddy: Option<u32>,
    mdns: Option<u32>,
}

/// Heartbeat written every tick to `supervisor.json`, for the MCP diagnose tool.
#[derive(Serialize)]
struct Heartbeat<'a> {
    last_tick_unix: u64,
    lan_ip: Option<&'a str>,
    names: Vec<String>,
    pids: Pids,
    /// Whether the running config serves the HTTPS sites (the CA was ready when it was written).
    https_available: bool,
    /// Caddy exits in a row without surviving a tick (port 80 taken, bad config); 0 when it runs.
    caddy_failures: u32,
}

#[derive(Default)]
struct Procs {
    keepalive: Option<Child>,
    caddy: Option<Child>,
    mdns: Option<Child>,
    /// LAN IP, routes and HTTPS availability the running Caddy/mDNS were started with.
    config: Option<(String, Vec<Route>, bool)>,
    /// Whether the CA was ready at the last check, and when to retry setting it up when it was not.
    ca_ready: bool,
    ca_retry_at: Option<Instant>,
    caddy_failures: u32,
    caddy_retry_at: Option<Instant>,
}

const CA_RETRY: Duration = Duration::from_secs(60);

/// Wait before restarting a Caddy that exited `failures` times in a row: 10 s doubling to ~10 min.
pub fn caddy_backoff(failures: u32) -> Duration {
    TICK * 2u32.pow(failures.clamp(1, 7) - 1)
}

pub struct Supervisor {
    paths: Paths,
    procs: Mutex<Procs>,
    started: Mutex<bool>,
}

fn alive(c: &mut Option<Child>) -> bool {
    matches!(c.as_mut().map(|c| c.try_wait()), Some(Ok(None)))
}

fn kill(c: &mut Option<Child>) {
    if let Some(mut child) = c.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// IPv4 of the interface holding the default route. Connecting a UDP socket sends no packets.
pub fn lan_ip() -> Option<String> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then(|| ip.to_string())
}

impl Supervisor {
    pub fn new(paths: Paths) -> Arc<Self> {
        Arc::new(Supervisor { paths, procs: Mutex::new(Procs::default()), started: Mutex::new(false) })
    }

    /// Starts the background loop once; later calls do nothing. Ticks do nothing before this.
    pub fn start(self: &Arc<Self>) {
        let mut started = self.started.lock().unwrap();
        if *started {
            return;
        }
        *started = true;
        let me = Arc::clone(self);
        std::thread::spawn(move || loop {
            me.tick();
            std::thread::sleep(TICK);
        });
    }

    /// Whether HTTPS sites can be served. Once ready, only the CA files are checked (cheap); while
    /// not, setting the CA up is retried once a minute, and the config is rewritten when it works.
    fn ca_ready(&self, p: &mut Procs) -> bool {
        let paths = &self.paths;
        if p.ca_ready {
            p.ca_ready = paths.ca_cert().exists() && paths.ca_key().exists();
        } else if p.ca_retry_at.is_none_or(|t| Instant::now() >= t) {
            // Caddy keeps its intermediate CA key in its storage folder, so HTTPS also needs that
            // folder locked down to the current user.
            p.ca_ready = crate::ca::ensure(paths)
                .and_then(|_| crate::ca::prepare_caddy_storage(paths))
                .map_err(|e| crate::log(paths, &format!("supervisor: CA unavailable, HTTPS sites off, retry in {} s: {e}", CA_RETRY.as_secs())))
                .is_ok();
            p.ca_retry_at = (!p.ca_ready).then(|| Instant::now() + CA_RETRY);
        }
        p.ca_ready
    }

    /// Writes the home page, the downloadable root certificate and the Caddyfile.
    fn write_config(&self, ip: &str, routes: &[Route], https: bool) {
        let p = &self.paths;
        let site = p.site_dir();
        let _ = std::fs::create_dir_all(&site);
        let _ = std::fs::create_dir_all(p.caddy_storage());
        let _ = std::fs::write(site.join("index.html"), home_page(routes, &crate::ui_text));
        if https {
            let _ = std::fs::copy(p.ca_cert(), site.join("konak-root.crt"));
        }
        let (cert, key) = (p.ca_cert(), p.ca_key());
        let files = CaddyFiles { storage: &p.caddy_storage(), site: &site, ca: https.then_some((cert.as_path(), key.as_path())) };
        let _ = std::fs::write(p.caddyfile(), caddyfile(ip, routes, &files));
    }

    /// Starts Caddy unless it runs or is backing off. A Caddy that exited on its own (port 80
    /// taken, for example) is restarted with a growing delay, so it does not restart and log every
    /// tick forever.
    fn keep_caddy(&self, p: &mut Procs) {
        if alive(&mut p.caddy) {
            p.caddy_failures = 0;
            p.caddy_retry_at = None;
            return;
        }
        if let Some(mut old) = p.caddy.take() {
            let code = old.try_wait().ok().flatten().and_then(|s| s.code());
            p.caddy_failures += 1;
            let wait = caddy_backoff(p.caddy_failures);
            p.caddy_retry_at = Some(Instant::now() + wait);
            crate::log(&self.paths, &format!("supervisor: caddy exited ({code:?}), {} in a row, next start in {} s (see caddy.log)", p.caddy_failures, wait.as_secs()));
        }
        if p.caddy_retry_at.is_some_and(|t| Instant::now() < t) {
            return;
        }
        let cf = self.paths.caddyfile();
        let log = self.paths.log_dir().join("caddy.log");
        match shell::spawn_hidden(&self.paths.caddy().to_string_lossy(), &["run", "--config", &cf.to_string_lossy(), "--adapter", "caddyfile"], Some(&log)) {
            Ok(c) => {
                p.caddy = Some(c);
                crate::log(&self.paths, "supervisor: caddy started");
            }
            Err(e) => {
                p.caddy_failures += 1;
                p.caddy_retry_at = Some(Instant::now() + caddy_backoff(p.caddy_failures));
                crate::log(&self.paths, &format!("supervisor: caddy did not start: {e}"));
            }
        }
    }

    pub fn tick(&self) {
        if !*self.started.lock().unwrap() {
            return;
        }
        let mut p = self.procs.lock().unwrap();
        let log_dir = self.paths.log_dir();

        if !alive(&mut p.keepalive) {
            p.keepalive = shell::spawn_hidden("wsl.exe", &["-d", &self.paths.distro, "-u", "root", "--", "sleep", "infinity"], None).ok();
            crate::log(&self.paths, "supervisor: keepalive (re)started");
        }

        // A state.json that cannot be read keeps the running config rather than dropping every app.
        let (routes, skipped) = match State::try_load(&self.paths) {
            Ok(s) => routes_checked(&s),
            Err(_) => match &p.config {
                Some((_, r, _)) => (r.clone(), vec![]),
                None => (vec![], vec![]),
            },
        };
        let mut names = vec![HOME_NAME.to_string()];
        names.extend(routes.iter().map(|r| r.name.clone()));
        let ip = lan_ip();
        if let Some(ip) = &ip {
            let https = self.ca_ready(&mut p);
            let config = Some((ip.clone(), routes.clone(), https));
            if p.config != config {
                kill(&mut p.caddy);
                kill(&mut p.mdns);
                // A new config gets an immediate start, whatever the old one's failures were.
                p.caddy_failures = 0;
                p.caddy_retry_at = None;
                self.write_config(ip, &routes, https);
                crate::log(&self.paths, &format!("supervisor: LAN IP {ip}, names {}", names.join(",")));
                if !skipped.is_empty() {
                    crate::log(&self.paths, &format!("supervisor: no route for {}: name already in use", skipped.join(",")));
                }
                p.config = config;
            }
            self.keep_caddy(&mut p);
            if !alive(&mut p.mdns) {
                p.mdns = shell::spawn_hidden(&self.paths.mdns().to_string_lossy(), &[ip, "86400", &names.join(",")], Some(&log_dir.join("mdns.log"))).ok();
                crate::log(&self.paths, "supervisor: mdns (re)started");
            }
        }

        let pid = |c: &Option<Child>| c.as_ref().map(Child::id);
        let beat = Heartbeat {
            last_tick_unix: crate::installer::now(),
            lan_ip: ip.as_deref(),
            names,
            pids: Pids { keepalive: pid(&p.keepalive), caddy: pid(&p.caddy), mdns: pid(&p.mdns) },
            https_available: p.config.as_ref().is_some_and(|c| c.2),
            caddy_failures: p.caddy_failures,
        };
        let _ = std::fs::write(self.paths.supervisor_file(), serde_json::to_string_pretty(&beat).unwrap());
    }

    pub fn stop_all(&self) {
        let mut p = self.procs.lock().unwrap();
        kill(&mut p.mdns);
        kill(&mut p.caddy);
        kill(&mut p.keepalive);
        p.config = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn route(name: &str, port: u16, https: bool) -> Route {
        Route { id: name.into(), name: name.into(), port, https }
    }

    #[test]
    fn caddy_restarts_back_off() {
        let secs: Vec<u64> = (1..=9).map(|n| caddy_backoff(n).as_secs()).collect();
        assert_eq!(secs, [10, 20, 40, 80, 160, 320, 640, 640, 640]);
        assert_eq!(caddy_backoff(0).as_secs(), 10);
    }

    #[test]
    fn caddyfile_for_http_and_https_apps() {
        let (storage, site, cert, key) =
            (PathBuf::from(r"C:\K\caddy"), PathBuf::from(r"C:\K\site"), PathBuf::from(r"C:\K\ca\root.crt"), PathBuf::from(r"C:\K\ca\root.key"));
        let files = CaddyFiles { storage: &storage, site: &site, ca: Some((&cert, &key)) };
        let routes = [route("jellyfin", 47101, false), route("vault", 47103, true)];
        let c = caddyfile("192.168.1.5", &routes, &files);
        assert!(c.contains("skip_install_trust") && c.contains("persist_config off"));
        assert!(c.contains("storage file_system \"C:/K/caddy\""));
        assert!(c.contains("\t\t\troot {\n\t\t\t\tcert \"C:/K/ca/root.crt\"\n\t\t\t\tkey \"C:/K/ca/root.key\"\n"));
        assert!(c.contains("http://konak.local:80 {\n\tbind 192.168.1.5\n\troot * \"C:/K/site\"\n\tfile_server\n}"));
        let proxy = |port: u16| format!("\treverse_proxy 127.0.0.1:{port} {{\n\t\theader_up X-Real-IP {{remote_host}}\n\t}}\n}}");
        assert!(c.contains(&format!("http://jellyfin.local:80 {{\n\tbind 192.168.1.5\n{}", proxy(47101))));
        assert!(c.contains(&format!("https://vault.local:443 {{\n\tbind 192.168.1.5\n\ttls internal\n{}", proxy(47103))));
        assert!(c.contains("http://vault.local:80 {\n\tbind 192.168.1.5\n\tredir https://vault.local{uri} permanent\n}"));
        assert!(c.ends_with("http://:80 {\n\tbind 192.168.1.5\n\trespond 404\n}\nhttps://:443 {\n\tbind 192.168.1.5\n\ttls internal\n\trespond 404\n}\n"));
        assert_eq!(c.matches('{').count(), c.matches('}').count());

        // Without the CA, HTTPS apps are dropped rather than served by some other CA.
        let no_ca = caddyfile("192.168.1.5", &routes, &CaddyFiles { storage: &storage, site: &site, ca: None });
        assert!(!no_ca.contains("pki") && !no_ca.contains("vault.local") && no_ca.contains("jellyfin.local"));
        assert!(no_ca.contains("http://:80 {\n\tbind 192.168.1.5\n\trespond 404\n}") && !no_ca.contains(":443") && !no_ca.contains("tls internal"));
        // No HTTPS app: no 443 listener at all.
        let http_only = caddyfile("192.168.1.5", &routes[..1], &files);
        assert!(http_only.contains("http://:80 {") && !http_only.contains(":443"));
    }

    #[test]
    fn unclaimed_apps_get_no_route() {
        use crate::installer::InstalledApp;
        let app = |id: &str, name: &str, claimed| InstalledApp { id: id.into(), name: name.into(), installed_unix: 0, claimed };
        let mut s = State::default();
        s.apps = vec![app("jellyfin", "movies", true), app("forgejo", "git", false)];
        let r = routes(&s);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "movies");
    }

    #[test]
    fn a_duplicate_name_drops_only_the_later_route() {
        use crate::installer::InstalledApp;
        let app = |id: &str, name: &str| InstalledApp { id: id.into(), name: name.into(), installed_unix: 0, claimed: true };
        let mut s = State::default();
        s.apps = vec![app("jellyfin", "media"), app("immich", "media"), app("forgejo", "konak"), app("nextcloud", "cloud")];
        let (r, skipped) = routes_checked(&s);
        assert_eq!(r.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["jellyfin", "nextcloud"]);
        assert_eq!(skipped, ["immich", "forgejo"]);
        let files = CaddyFiles { storage: Path::new("s"), site: Path::new("w"), ca: None };
        assert_eq!(caddyfile("10.0.0.2", &r, &files).matches("http://media.local:80").count(), 1);
    }

    #[test]
    fn home_page_lists_apps_escaped() {
        let text = |k: &str| if k == "apps.jellyfin.name" { "Jelly<fin>".to_string() } else { k.to_string() };
        let page = home_page(&[route("jellyfin", 47101, false)], &text);
        assert!(page.contains("<a href=\"http://jellyfin.local\">Jelly&lt;fin&gt;</a>"));
        assert!(page.contains("/konak-root.crt"));
        assert!(home_page(&[], &text).contains("home.no_apps"));
    }

    #[test]
    fn home_page_has_no_fingerprint_and_points_at_the_pc() {
        // Plain HTTP: a fingerprint here would come from the same attacker as the file.
        let page = home_page(&[], &|k: &str| k.to_string());
        assert!(!page.contains("fingerprint") && !page.contains("<code>"));
        assert!(page.contains("<p>home.cert_check</p>"));
    }
}
