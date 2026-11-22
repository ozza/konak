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
use std::time::Duration;

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
    state
        .apps
        .iter()
        .filter(|a| a.claimed)
        .filter_map(|a| catalog::get(&a.id).map(|m| Route { id: a.id.clone(), name: a.name.clone(), port: m.port, https: m.https }))
        .collect()
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
    for r in routes {
        let host = format!("{}.local", r.name);
        if !r.https {
            s += &format!("http://{host}:80 {{\n\tbind {ip}\n\treverse_proxy 127.0.0.1:{}\n}}\n", r.port);
        } else if f.ca.is_some() {
            s += &format!("https://{host}:443 {{\n\tbind {ip}\n\ttls internal\n\treverse_proxy 127.0.0.1:{}\n}}\n", r.port);
            s += &format!("http://{host}:80 {{\n\tbind {ip}\n\tredir https://{host}{{uri}} permanent\n}}\n");
        }
    }
    s
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The static `konak.local` page: installed apps with links, and the root certificate download with
/// its SHA-256 fingerprint (`None` when the CA is unavailable).
pub fn home_page(routes: &[Route], fingerprint: Option<&str>, text: &dyn Fn(&str) -> String) -> String {
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
    let fp = fingerprint.map(|f| format!("<p>{}<br><code>{}</code></p>", esc(&text("home.cert_fingerprint")), esc(f))).unwrap_or_default();
    format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>Konak</title><style>body{{font-family:system-ui,sans-serif;max-width:40rem;margin:2rem auto;padding:0 1rem;line-height:1.5}}\
li{{margin:.4rem 0}}span{{color:#666;font-family:monospace;margin-left:.5rem}}code{{word-break:break-all}}</style></head><body>\
<h1>Konak</h1><h2>{}</h2><ul>{}</ul><h2>{}</h2><p>{}</p><p><a href=\"/konak-root.crt\" download>{}</a></p>{}</body></html>\n",
        esc(&text("home.apps")),
        items,
        esc(&text("home.cert_title")),
        esc(&text("home.cert_body")),
        esc(&text("home.cert_link")),
        fp,
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
    https_available: bool,
}

#[derive(Default)]
struct Procs {
    keepalive: Option<Child>,
    caddy: Option<Child>,
    mdns: Option<Child>,
    /// LAN IP and routes the running Caddy/mDNS were started with.
    config: Option<(String, Vec<Route>)>,
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

    /// Writes the home page, the downloadable root certificate and the Caddyfile.
    fn write_config(&self, ip: &str, routes: &[Route]) -> bool {
        let p = &self.paths;
        let https = crate::ca::ensure(p).map_err(|e| crate::log(p, &format!("supervisor: CA unavailable, HTTPS sites off: {e}"))).is_ok();
        let site = p.site_dir();
        let _ = std::fs::create_dir_all(&site);
        let _ = std::fs::create_dir_all(p.caddy_storage());
        let fp = if https { crate::ca::fingerprint(p).ok() } else { None };
        let _ = std::fs::write(site.join("index.html"), home_page(routes, fp.as_deref(), &crate::ui_text));
        if https {
            let _ = std::fs::copy(p.ca_cert(), site.join("konak-root.crt"));
        }
        let (cert, key) = (p.ca_cert(), p.ca_key());
        let files = CaddyFiles { storage: &p.caddy_storage(), site: &site, ca: https.then_some((cert.as_path(), key.as_path())) };
        let _ = std::fs::write(p.caddyfile(), caddyfile(ip, routes, &files));
        https
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

        let routes = routes(&State::load(&self.paths));
        let mut names = vec![HOME_NAME.to_string()];
        names.extend(routes.iter().map(|r| r.name.clone()));
        let ip = lan_ip();
        let mut https = self.paths.ca_key().exists();
        if let Some(ip) = &ip {
            let config = Some((ip.clone(), routes.clone()));
            if p.config != config {
                kill(&mut p.caddy);
                kill(&mut p.mdns);
                https = self.write_config(ip, &routes);
                crate::log(&self.paths, &format!("supervisor: LAN IP {ip}, names {}", names.join(",")));
                p.config = config;
            }
            if !alive(&mut p.caddy) {
                let cf = self.paths.caddyfile();
                p.caddy = shell::spawn_hidden(
                    &self.paths.caddy().to_string_lossy(),
                    &["run", "--config", &cf.to_string_lossy(), "--adapter", "caddyfile"],
                    Some(&log_dir.join("caddy.log")),
                )
                .ok();
                crate::log(&self.paths, "supervisor: caddy (re)started");
            }
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
            https_available: https,
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
        assert!(c.contains("http://jellyfin.local:80 {\n\tbind 192.168.1.5\n\treverse_proxy 127.0.0.1:47101\n}"));
        assert!(c.contains("https://vault.local:443 {\n\tbind 192.168.1.5\n\ttls internal\n\treverse_proxy 127.0.0.1:47103\n}"));
        assert!(c.contains("http://vault.local:80 {\n\tbind 192.168.1.5\n\tredir https://vault.local{uri} permanent\n}"));
        assert_eq!(c.matches('{').count(), c.matches('}').count());

        // Without the CA, HTTPS apps are dropped rather than served by some other CA.
        let no_ca = caddyfile("192.168.1.5", &routes, &CaddyFiles { storage: &storage, site: &site, ca: None });
        assert!(!no_ca.contains("pki") && !no_ca.contains("vault.local") && no_ca.contains("jellyfin.local"));
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
    fn home_page_lists_apps_escaped() {
        let text = |k: &str| if k == "apps.jellyfin.name" { "Jelly<fin>".to_string() } else { k.to_string() };
        let page = home_page(&[route("jellyfin", 47101, false)], None, &text);
        assert!(page.contains("<a href=\"http://jellyfin.local\">Jelly&lt;fin&gt;</a>"));
        assert!(page.contains("/konak-root.crt"));
        assert!(!page.contains("home.cert_fingerprint"));
        assert!(home_page(&[], None, &text).contains("home.no_apps"));
    }

    #[test]
    fn home_page_shows_the_fingerprint_escaped() {
        let page = home_page(&[], Some("AB:CD<x>"), &|k: &str| k.to_string());
        assert!(page.contains("<p>home.cert_fingerprint<br><code>AB:CD&lt;x&gt;</code></p>"));
    }
}
