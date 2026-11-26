//! Keeps Konak's runtime pieces alive while the app runs (in the tray):
//! - a keepalive session so WSL never idles the distro out;
//! - Caddy on the LAN IP: `konak.local` (home page and root certificate) plus one name per app;
//! - the mDNS responder announcing all those names.
//!
//! Spike step 3 showed that checking only at logon is not enough (an external `wsl --terminate`
//! left everything down), so this loop checks every few seconds. Each tick re-reads the installed
//! apps from `state.json`, so a change made anywhere (UI, MCP, another process) is picked up; Caddy
//! and mDNS restart only when the app list, the LAN IP or the web ports change.
//!
//! Web ports: Caddy serves on 80/443 whenever port 80 on the LAN IP is free. While another program
//! holds it (IIS, XAMPP, Apache, http.sys, ...), Caddy serves on the first free pair of
//! `WebPorts::FALLBACKS` instead, and
//! it returns to 80/443 by itself once port 80 is free again (`PortChoice`). App settings that embed
//! the app's own address are then re-rendered for the new ports (`apps::rerender_for_ports`).

use crate::installer::State;
use crate::paths::Paths;
use crate::{catalog, shell};
use serde::{Deserialize, Serialize};
use std::net::{SocketAddr, UdpSocket};
use std::path::Path;
use std::process::Child;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_secs(10);
pub const HOME_NAME: &str = "konak";

/// The HTTP and HTTPS ports Caddy serves the `.local` names on.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct WebPorts {
    pub http: u16,
    pub https: u16,
}

impl Default for WebPorts {
    fn default() -> Self {
        WebPorts::STANDARD
    }
}

impl WebPorts {
    pub const STANDARD: WebPorts = WebPorts { http: 80, https: 443 };
    /// Pairs tried in order while another program holds port 80 on the LAN IP; the first pair whose
    /// two ports both bind on the LAN IP wins (`first_free_fallback`). The old 47080/47443 pair lies
    /// inside the TCP/UDP range 44620-48715 that WSL's mirrored networking reserves on Windows (the
    /// distro's `ip_local_port_range`), so it never binds there. These pairs are:
    /// - below 32768, where Linux's default ephemeral range starts, so outside it in NAT mode and
    ///   outside mirrored WSL's 44620-48715;
    /// - below the Windows dynamic range (49152+), where Hyper-V and WinNAT reserve blocks;
    /// - outside every range in `netsh int ipv4 show excludedportrange protocol=tcp` on the dev PC
    ///   (2026-11-26: 5357, 13508 and blocks from 50000 up);
    /// - not a common dev-server default (3000, 4200, 5000, 5173, 8000, 8080, 8443, 8888, 9000).
    pub const FALLBACKS: [WebPorts; 3] = [WebPorts { http: 18080, https: 18443 }, WebPorts { http: 28080, https: 28443 }, WebPorts { http: 15080, https: 15443 }];

    pub fn is_fallback(self) -> bool {
        self != WebPorts::STANDARD
    }

    /// `:<port>` for the scheme, or nothing when it is the scheme's default port.
    pub fn suffix(self, https: bool) -> String {
        match (https, self.http, self.https) {
            (false, 80, _) | (true, _, 443) => String::new(),
            (false, p, _) | (true, _, p) => format!(":{p}"),
        }
    }

    /// `http://<name>.local`, with the port when it is not the scheme's default.
    pub fn url(self, https: bool, name: &str) -> String {
        format!("{}://{name}.local{}", if https { "https" } else { "http" }, self.suffix(https))
    }
}

/// The web ports Caddy serves on now, from the supervisor heartbeat, so every process (UI, MCP)
/// reads the same. 80/443 before the first heartbeat.
pub fn active_ports(paths: &Paths) -> WebPorts {
    crate::diag::heartbeat(paths).map(|h| h.web_ports).unwrap_or_default()
}

/// The first fallback pair whose two ports are `free`, or `None` when no pair is.
pub fn first_free_fallback(free: impl Fn(u16) -> bool) -> Option<WebPorts> {
    WebPorts::FALLBACKS.into_iter().find(|p| free(p.http) && free(p.https))
}

/// The port choice, kept apart from sockets and clocks so it can be tested. Only port 80 decides
/// between 80/443 and a fallback pair; 443 follows it.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct PortChoice {
    /// `None` until the first test.
    pub ports: Option<WebPorts>,
    free_in_a_row: u32,
}

impl PortChoice {
    /// Caddy is about to start and none of Konak's runs: the bind tests alone chose `ports`.
    pub fn fresh(ports: WebPorts) -> Self {
        PortChoice { ports: Some(ports), free_in_a_row: 0 }
    }

    /// A periodic re-test while on the fallback: back to 80/443 only once port 80 was free on two
    /// tests in a row, so a program that restarts does not make Konak flap. On 80/443 nothing changes
    /// (Konak's own Caddy holds port 80 there).
    pub fn retest(self, port80_free: bool) -> Self {
        if !self.ports.is_some_and(WebPorts::is_fallback) {
            return self;
        }
        match (port80_free, self.free_in_a_row + 1) {
            (false, _) => PortChoice { free_in_a_row: 0, ..self },
            (true, n) if n >= 2 => PortChoice::fresh(WebPorts::STANDARD),
            (true, n) => PortChoice { free_in_a_row: n, ..self },
        }
    }
}

/// Port 80 is re-tested at most this often while on the fallback.
const PORT_RETEST: Duration = Duration::from_secs(60);

/// Whether `<ip>:<port>` can be bound now. Bind only, never listen: a listening konak.exe on the LAN
/// IP would make Windows Firewall ask the user. The bind fails the same way Caddy's would when
/// another program holds the address.
pub fn can_bind(ip: &str, port: u16) -> bool {
    use socket2::{Domain, Socket, Type};
    let Ok(addr) = format!("{ip}:{port}").parse::<SocketAddr>() else { return false };
    Socket::new(Domain::for_address(addr), Type::STREAM, None).and_then(|s| s.bind(&addr.into())).is_ok()
}

/// The web ports as the panel's `Status` carries them. `port80_owner` is set only while on the
/// fallback: the program holding port 80 (`httpd.exe (pid 1234)`), or `None` when it is unknown.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct WebStatus {
    pub http: u16,
    pub https: u16,
    pub port80_owner: Option<String>,
}

impl Default for WebStatus {
    fn default() -> Self {
        let p = WebPorts::STANDARD;
        WebStatus { http: p.http, https: p.https, port80_owner: None }
    }
}

impl WebStatus {
    pub fn ports(&self) -> WebPorts {
        WebPorts { http: self.http, https: self.https }
    }
}

/// One proxied app name.
#[derive(Clone, PartialEq, Debug)]
pub struct Route {
    pub id: String,
    pub name: String,
    pub port: u16,
    pub https: bool,
}

impl Route {
    pub fn url(&self, ports: WebPorts) -> String {
        ports.url(self.https, &self.name)
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

pub fn caddyfile(ip: &str, routes: &[Route], f: &CaddyFiles, ports: WebPorts) -> String {
    let (http, https) = (ports.http, ports.https);
    // skip_install_trust: Caddy must never touch the Windows trust store itself (it would pop a
    // dialog); trusting the root is an explicit user action (ca::trust_on_this_pc). persist_config off:
    // no autosave.json in %APPDATA%\Caddy, the Caddyfile is the only config.
    let mut s = format!("{{\n\tadmin off\n\tpersist_config off\n\tauto_https disable_redirects\n\tskip_install_trust\n\tstorage file_system {}\n", q(f.storage));
    if let Some((cert, key)) = f.ca {
        s += &format!("\tpki {{\n\t\tca local {{\n\t\t\troot {{\n\t\t\t\tcert {}\n\t\t\t\tkey {}\n\t\t\t}}\n\t\t}}\n\t}}\n", q(cert), q(key));
    }
    s += "}\n";
    s += &format!("http://{HOME_NAME}.local:{http} {{\n\tbind {ip}\n\troot * {}\n\tfile_server\n}}\n", q(f.site));
    // Caddy already replaces X-Forwarded-For with the client address; X-Real-IP it would pass through
    // as the client sent it, and some apps (Nextcloud's Apache, Vaultwarden's default) trust it.
    let proxy = |port: u16| format!("reverse_proxy 127.0.0.1:{port} {{\n\t\theader_up X-Real-IP {{remote_host}}\n\t}}");
    for r in routes {
        let host = format!("{}.local", r.name);
        if !r.https {
            s += &format!("http://{host}:{http} {{\n\tbind {ip}\n\t{}\n}}\n", proxy(r.port));
        } else if f.ca.is_some() {
            s += &format!("https://{host}:{https} {{\n\tbind {ip}\n\ttls internal\n\t{}\n}}\n", proxy(r.port));
            s += &format!("http://{host}:{http} {{\n\tbind {ip}\n\tredir https://{host}{}{{uri}} permanent\n}}\n", ports.suffix(true));
        }
    }
    // Any other name (or the bare IP) gets a 404 instead of Caddy's empty 200. Caddy matches the
    // named sites first. On 443 only when HTTPS sites exist, so the catch-all never brings up a
    // listener of its own; it issues no certificate, so an unknown SNI still fails the handshake
    // and only a request whose Host differs from its SNI reaches it.
    s += &format!("http://:{http} {{\n\tbind {ip}\n\trespond 404\n}}\n");
    if f.ca.is_some() && routes.iter().any(|r| r.https) {
        s += &format!("https://:{https} {{\n\tbind {ip}\n\ttls internal\n\trespond 404\n}}\n");
    }
    s
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The static `konak.local` page: installed apps with links, and the root certificate download. It is
/// served over plain HTTP, so it never shows the fingerprint (a LAN attacker could change both the
/// file and the number); it points the user at the Konak window on the PC instead.
pub fn home_page(routes: &[Route], ports: WebPorts, text: &dyn Fn(&str) -> String) -> String {
    let items: String = if routes.is_empty() {
        format!("<li>{}</li>", esc(&text("home.no_apps")))
    } else {
        routes
            .iter()
            .map(|r| {
                let label = text(&format!("apps.{}.name", r.id));
                format!("<li><a href=\"{0}\">{1}</a> <span>{0}</span></li>", esc(&r.url(ports)), esc(&label))
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
    /// The ports Caddy serves on: 80/443, or the fallback while another program holds port 80.
    web_ports: WebPorts,
    /// The program holding port 80, while on the fallback and when it is known.
    port80_owner: Option<String>,
}

#[derive(Default)]
struct Procs {
    keepalive: Option<Child>,
    caddy: Option<Child>,
    mdns: Option<Child>,
    /// LAN IP, routes, HTTPS availability and web ports the running Caddy/mDNS were started with.
    config: Option<(String, Vec<Route>, bool, WebPorts)>,
    /// Whether the CA was ready at the last check, and when to retry setting it up when it was not.
    ca_ready: bool,
    ca_retry_at: Option<Instant>,
    caddy_failures: u32,
    caddy_retry_at: Option<Instant>,
    choice: PortChoice,
    last_port_test: Option<Instant>,
}

const CA_RETRY: Duration = Duration::from_secs(60);
/// Wait before re-rendering the apps again after a re-render failed or found an app busy.
const RERENDER_RETRY: Duration = Duration::from_secs(60);

/// Re-renders the claimed apps for new web ports; returns when it is done.
pub type Rerender = Box<dyn Fn(&Paths, WebPorts) -> Result<(), String> + Send + Sync>;

#[derive(Default)]
struct RerenderJob {
    running: bool,
    retry_at: Option<Instant>,
}

/// Wait before restarting a Caddy that exited `failures` times in a row: 10 s doubling to ~10 min.
pub fn caddy_backoff(failures: u32) -> Duration {
    TICK * 2u32.pow(failures.clamp(1, 7) - 1)
}

pub struct Supervisor {
    paths: Paths,
    procs: Mutex<Procs>,
    started: Mutex<bool>,
    /// What the panel shows; its own lock, so reading it never waits for a tick.
    web: Mutex<WebStatus>,
    job: Mutex<RerenderJob>,
    /// Called when the web ports or the port 80 owner change (the UI emits `status`).
    notify: OnceLock<Box<dyn Fn() + Send + Sync>>,
    /// Runs the re-render for new web ports; `apps::rerender_for_ports` when none is set.
    rerender: OnceLock<Rerender>,
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
        Arc::new(Supervisor {
            paths,
            procs: Mutex::new(Procs::default()),
            started: Mutex::new(false),
            web: Mutex::new(WebStatus::default()),
            job: Mutex::new(RerenderJob::default()),
            notify: OnceLock::new(),
            rerender: OnceLock::new(),
        })
    }

    /// Set once, before `start`: called (on the supervisor's threads) when the web ports or the
    /// port 80 owner change.
    pub fn on_change(&self, f: impl Fn() + Send + Sync + 'static) {
        let _ = self.notify.set(Box::new(f));
    }

    /// Set once, before `start`: how the claimed apps are re-rendered when the web ports change.
    pub fn set_rerender(&self, f: impl Fn(&Paths, WebPorts) -> Result<(), String> + Send + Sync + 'static) {
        let _ = self.rerender.set(Box::new(f));
    }

    /// The web ports Caddy serves on, for the panel.
    pub fn web_status(&self) -> WebStatus {
        self.web.lock().unwrap().clone()
    }

    fn changed(&self) {
        if let Some(f) = self.notify.get() {
            f();
        }
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
    fn write_config(&self, ip: &str, routes: &[Route], https: bool, ports: WebPorts) {
        let p = &self.paths;
        let site = p.site_dir();
        let _ = std::fs::create_dir_all(&site);
        let _ = std::fs::create_dir_all(p.caddy_storage());
        let _ = std::fs::write(site.join("index.html"), home_page(routes, ports, &crate::ui_text));
        if https {
            let _ = std::fs::copy(p.ca_cert(), site.join("konak-root.crt"));
        }
        let (cert, key) = (p.ca_cert(), p.ca_key());
        let files = CaddyFiles { storage: &p.caddy_storage(), site: &site, ca: https.then_some((cert.as_path(), key.as_path())) };
        let _ = std::fs::write(p.caddyfile(), caddyfile(ip, routes, &files, ports));
    }

    /// The web ports for this tick, and whether the program holding port 80 should be looked up.
    /// Port 80 is tested before Caddy starts on its own (first start, or after it exited), and once
    /// a minute while on the fallback. A restart for a new app list keeps the current choice: Konak's
    /// own Caddy held the port until a moment ago. The fallback pair is picked on a fresh start only,
    /// for the same reason.
    fn choose_ports(&self, p: &mut Procs, ip: &str) -> (WebPorts, bool) {
        let before = p.choice.ports;
        let starting = !alive(&mut p.caddy) && p.caddy_retry_at.is_none_or(|t| Instant::now() >= t);
        let mut owner = false;
        if starting || before.is_none() {
            let ports = if can_bind(ip, WebPorts::STANDARD.http) {
                if !can_bind(ip, WebPorts::STANDARD.https) {
                    crate::log(&self.paths, &format!("supervisor: port 443 on {ip} is held by another program; HTTPS sites fail until it is free"));
                }
                WebPorts::STANDARD
            } else {
                first_free_fallback(|port| can_bind(ip, port)).unwrap_or_else(|| {
                    // As with a single fallback pair: Caddy tries the first one, fails, and backs off.
                    let all = WebPorts::FALLBACKS.map(|f| format!("{}/{}", f.http, f.https)).join(", ");
                    crate::log(&self.paths, &format!("supervisor: port 80 and every fallback pair ({all}) on {ip} are held by other programs; trying the first pair anyway"));
                    WebPorts::FALLBACKS[0]
                })
            };
            p.choice = PortChoice::fresh(ports);
            p.last_port_test = Some(Instant::now());
            owner = ports.is_fallback();
        } else if before.is_some_and(WebPorts::is_fallback) && p.last_port_test.is_none_or(|t| t.elapsed() >= PORT_RETEST) {
            let free = can_bind(ip, WebPorts::STANDARD.http);
            p.choice = p.choice.retest(free);
            p.last_port_test = Some(Instant::now());
            // The holder may have changed (IIS stopped, XAMPP started).
            owner = !free;
        }
        let ports = p.choice.ports.unwrap_or_default();
        if before != p.choice.ports {
            let why = if ports.is_fallback() { "port 80 is held by another program" } else { "port 80 is free" };
            crate::log(&self.paths, &format!("supervisor: {why} on {ip}; serving on ports {}/{}", ports.http, ports.https));
        }
        (ports, owner)
    }

    /// Records the ports for the panel; true when they changed. The owner is cleared off the fallback.
    fn set_web(&self, ports: WebPorts) -> bool {
        let mut w = self.web.lock().unwrap();
        let owner = if ports.is_fallback() { w.port80_owner.clone() } else { None };
        let new = WebStatus { http: ports.http, https: ports.https, port80_owner: owner };
        let changed = *w != new;
        *w = new;
        changed
    }

    /// Finds the program holding port 80 (one read-only PowerShell run) on its own thread.
    fn look_up_port80_owner(self: &Arc<Self>, ip: String) {
        let me = Arc::clone(self);
        std::thread::spawn(move || {
            let caddy = me.paths.caddy().to_string_lossy().to_string();
            let owner = crate::diag::net_facts(&me.paths, Some(&ip)).ok().and_then(|n| crate::diag::port80_owner(&n, &ip, &caddy));
            let mut w = me.web.lock().unwrap();
            // Back on port 80 meanwhile: there is no owner to show.
            if w.http == WebPorts::STANDARD.http || w.port80_owner == owner {
                return;
            }
            crate::log(&me.paths, &format!("supervisor: port 80 is held by {}", owner.as_deref().unwrap_or("an unknown program")));
            w.port80_owner = owner;
            drop(w);
            me.changed();
        });
    }

    /// Re-renders the claimed apps for `ports` on its own thread, one run at a time, and again a
    /// minute after a failure. The run itself is idempotent (`apps::rerender_for_ports`).
    fn start_rerender(self: &Arc<Self>, ports: WebPorts) {
        {
            let mut job = self.job.lock().unwrap();
            if job.running || job.retry_at.is_some_and(|t| Instant::now() < t) {
                return;
            }
            job.running = true;
        }
        crate::log(&self.paths, &format!("supervisor: re-rendering the claimed apps for ports {}/{}", ports.http, ports.https));
        let me = Arc::clone(self);
        std::thread::spawn(move || {
            let result = match me.rerender.get() {
                Some(f) => f(&me.paths, ports),
                None => crate::apps::rerender_for_ports(&me.paths, ports, &|_, _| {}),
            };
            let mut job = me.job.lock().unwrap();
            job.running = false;
            job.retry_at = result.is_err().then(|| Instant::now() + RERENDER_RETRY);
            drop(job);
            match result {
                Ok(()) => crate::log(&me.paths, &format!("supervisor: apps re-rendered for ports {}/{}", ports.http, ports.https)),
                Err(e) => crate::log(&me.paths, &format!("supervisor: re-render for ports {}/{} not finished, retry in {} s: {e}", ports.http, ports.https, RERENDER_RETRY.as_secs())),
            }
        });
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

    pub fn tick(self: &Arc<Self>) {
        if !*self.started.lock().unwrap() {
            return;
        }
        let mut p = self.procs.lock().unwrap();
        // stop_all may have run while this tick waited for the lock.
        if !*self.started.lock().unwrap() {
            return;
        }
        let log_dir = self.paths.log_dir();

        if !alive(&mut p.keepalive) {
            p.keepalive = shell::spawn_hidden("wsl.exe", &["-d", &self.paths.distro, "-u", "root", "--", "sleep", "infinity"], None).ok();
            crate::log(&self.paths, "supervisor: keepalive (re)started");
        }

        // A state.json that cannot be read keeps the running config rather than dropping every app.
        let state = State::try_load(&self.paths);
        let (routes, skipped) = match &state {
            Ok(s) => routes_checked(s),
            Err(_) => match &p.config {
                Some((_, r, _, _)) => (r.clone(), vec![]),
                None => (vec![], vec![]),
            },
        };
        let mut names = vec![HOME_NAME.to_string()];
        names.extend(routes.iter().map(|r| r.name.clone()));
        let ip = lan_ip();
        let (mut web_changed, mut active) = (false, None);
        if let Some(ip) = &ip {
            let https = self.ca_ready(&mut p);
            let (ports, look_up_owner) = self.choose_ports(&mut p, ip);
            web_changed = self.set_web(ports);
            if look_up_owner {
                self.look_up_port80_owner(ip.clone());
            }
            active = Some(ports);
            let config = Some((ip.clone(), routes.clone(), https, ports));
            if p.config != config {
                kill(&mut p.caddy);
                kill(&mut p.mdns);
                // A new config gets an immediate start, whatever the old one's failures were.
                p.caddy_failures = 0;
                p.caddy_retry_at = None;
                self.write_config(ip, &routes, https, ports);
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
            web_ports: p.config.as_ref().map(|c| c.3).unwrap_or_default(),
            port80_owner: self.web.lock().unwrap().port80_owner.clone(),
        };
        // Written before any re-render starts, so an app operation reads the new ports from it.
        let _ = std::fs::write(self.paths.supervisor_file(), serde_json::to_string_pretty(&beat).unwrap());
        drop(p);

        if web_changed {
            self.changed();
        }
        // Apps whose settings still carry the other ports: re-render them, off the lock.
        if let (Some(ports), Ok(s)) = (active, &state) {
            if s.rendered_ports != ports {
                self.start_rerender(ports);
            }
        }
    }

    /// Stops the children for good: later ticks (the loop, an op thread) do nothing.
    pub fn stop_all(&self) {
        *self.started.lock().unwrap() = false;
        let mut p = self.procs.lock().unwrap();
        kill(&mut p.mdns);
        kill(&mut p.caddy);
        kill(&mut p.keepalive);
        p.config = None;
        p.choice = PortChoice::default();
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
        let c = caddyfile("192.168.1.5", &routes, &files, WebPorts::STANDARD);
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
        let no_ca = caddyfile("192.168.1.5", &routes, &CaddyFiles { storage: &storage, site: &site, ca: None }, WebPorts::STANDARD);
        assert!(!no_ca.contains("pki") && !no_ca.contains("vault.local") && no_ca.contains("jellyfin.local"));
        assert!(no_ca.contains("http://:80 {\n\tbind 192.168.1.5\n\trespond 404\n}") && !no_ca.contains(":443") && !no_ca.contains("tls internal"));
        // No HTTPS app: no 443 listener at all.
        let http_only = caddyfile("192.168.1.5", &routes[..1], &files, WebPorts::STANDARD);
        assert!(http_only.contains("http://:80 {") && !http_only.contains(":443"));
    }

    #[test]
    fn caddyfile_on_the_fallback_ports() {
        let (storage, site, cert, key) = (PathBuf::from("s"), PathBuf::from("w"), PathBuf::from("c"), PathBuf::from("k"));
        let files = CaddyFiles { storage: &storage, site: &site, ca: Some((&cert, &key)) };
        let routes = [route("jellyfin", 47101, false), route("vault", 47103, true)];
        let c = caddyfile("192.168.1.5", &routes, &files, WebPorts::FALLBACKS[0]);
        assert!(c.contains("http://konak.local:18080 {\n\tbind 192.168.1.5\n"));
        assert!(c.contains("http://jellyfin.local:18080 {\n\tbind 192.168.1.5\n\treverse_proxy 127.0.0.1:47101 "));
        assert!(c.contains("https://vault.local:18443 {\n\tbind 192.168.1.5\n\ttls internal\n"));
        // The redirect keeps the visitor on the fallback HTTPS port.
        assert!(c.contains("http://vault.local:18080 {\n\tbind 192.168.1.5\n\tredir https://vault.local:18443{uri} permanent\n}"));
        assert!(c.ends_with("http://:18080 {\n\tbind 192.168.1.5\n\trespond 404\n}\nhttps://:18443 {\n\tbind 192.168.1.5\n\ttls internal\n\trespond 404\n}\n"));
        assert!(!c.contains(":80 ") && !c.contains(":443 ") && !c.contains(":80{") && !c.contains(":443{"), "{c}");
        assert_eq!(c.matches('{').count(), c.matches('}').count());
    }

    #[test]
    fn urls_carry_the_port_only_on_the_fallback() {
        assert_eq!(route("movies", 47101, false).url(WebPorts::STANDARD), "http://movies.local");
        assert_eq!(route("vault", 47104, true).url(WebPorts::STANDARD), "https://vault.local");
        let fb = WebPorts::FALLBACKS[1];
        assert_eq!(route("movies", 47101, false).url(fb), "http://movies.local:28080");
        assert_eq!(route("vault", 47104, true).url(fb), "https://vault.local:28443");
        assert!(!WebPorts::STANDARD.is_fallback() && WebPorts::FALLBACKS.iter().all(|p| p.is_fallback()));
        assert_eq!(WebPorts::default(), WebPorts::STANDARD);
        let page = home_page(&[route("movies", 47101, false)], fb, &|k: &str| k.to_string());
        assert!(page.contains("<a href=\"http://movies.local:28080\">"));
    }

    #[test]
    fn fallback_pairs_stay_out_of_reserved_ranges_and_the_first_free_pair_wins() {
        for p in WebPorts::FALLBACKS {
            for port in [p.http, p.https] {
                // Mirrored WSL's reserved range, Linux's default ephemeral range, Windows' dynamic range.
                assert!(port < 32768 && !(44620..=48715).contains(&port), "{port}");
                assert!(!crate::catalog::PORT_RANGE.contains(&port), "{port}");
            }
        }
        assert_eq!(first_free_fallback(|_| true), Some(WebPorts::FALLBACKS[0]));
        // Both ports of a pair must be free: a taken HTTPS port skips the pair.
        assert_eq!(first_free_fallback(|p| p != WebPorts::FALLBACKS[0].https), Some(WebPorts::FALLBACKS[1]));
        assert_eq!(first_free_fallback(|p| p == WebPorts::FALLBACKS[2].http || p == WebPorts::FALLBACKS[2].https), Some(WebPorts::FALLBACKS[2]));
        assert_eq!(first_free_fallback(|_| false), None);
    }

    #[test]
    fn port_choice_returns_to_80_only_after_two_free_tests() {
        assert_eq!(PortChoice::fresh(WebPorts::STANDARD).ports, Some(WebPorts::STANDARD));
        // Any fallback pair, here the second, goes back to 80/443 the same way.
        let pair = WebPorts::FALLBACKS[1];
        let fallback = PortChoice::fresh(pair);
        assert_eq!(fallback.ports, Some(pair));
        let run = |tests: &[bool]| tests.iter().fold(fallback, |c, &free| c.retest(free)).ports.unwrap();
        assert_eq!(run(&[true]), pair, "one free test is not enough");
        assert_eq!(run(&[true, true]), WebPorts::STANDARD);
        assert_eq!(run(&[true, false, true]), pair, "a taken test resets the count");
        assert_eq!(run(&[true, false, true, true]), WebPorts::STANDARD);
        assert_eq!(run(&[false, false, false]), pair);
        // Back on 80/443 the count starts over, and a re-test there changes nothing.
        let back = fallback.retest(true).retest(true);
        assert_eq!(back, PortChoice::fresh(WebPorts::STANDARD));
        assert_eq!(back.retest(false), back);
        assert_eq!(PortChoice::default().retest(true), PortChoice::default());
    }

    #[test]
    fn bind_test_sees_a_taken_port() {
        // Loopback only: nothing on the LAN, no firewall prompt.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(!can_bind("127.0.0.1", port));
        drop(l);
        assert!(can_bind("127.0.0.1", port));
        assert!(!can_bind("not an ip", 80));
    }

    #[test]
    fn unclaimed_apps_get_no_route() {
        use crate::installer::InstalledApp;
        let app = |id: &str, name: &str, claimed| InstalledApp { id: id.into(), name: name.into(), installed_unix: 0, claimed, stopped: false };
        let mut s = State::default();
        s.apps = vec![app("jellyfin", "movies", true), app("forgejo", "git", false)];
        let r = routes(&s);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "movies");
    }

    #[test]
    fn a_duplicate_name_drops_only_the_later_route() {
        use crate::installer::InstalledApp;
        let app = |id: &str, name: &str| InstalledApp { id: id.into(), name: name.into(), installed_unix: 0, claimed: true, stopped: false };
        let mut s = State::default();
        s.apps = vec![app("jellyfin", "media"), app("immich", "media"), app("forgejo", "konak"), app("nextcloud", "cloud")];
        let (r, skipped) = routes_checked(&s);
        assert_eq!(r.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["jellyfin", "nextcloud"]);
        assert_eq!(skipped, ["immich", "forgejo"]);
        let files = CaddyFiles { storage: Path::new("s"), site: Path::new("w"), ca: None };
        assert_eq!(caddyfile("10.0.0.2", &r, &files, WebPorts::STANDARD).matches("http://media.local:80").count(), 1);
    }

    #[test]
    fn home_page_lists_apps_escaped() {
        let text = |k: &str| if k == "apps.jellyfin.name" { "Jelly<fin>".to_string() } else { k.to_string() };
        let page = home_page(&[route("jellyfin", 47101, false)], WebPorts::STANDARD, &text);
        assert!(page.contains("<a href=\"http://jellyfin.local\">Jelly&lt;fin&gt;</a>"));
        assert!(page.contains("/konak-root.crt"));
        assert!(home_page(&[], WebPorts::STANDARD, &text).contains("home.no_apps"));
    }

    #[test]
    fn home_page_has_no_fingerprint_and_points_at_the_pc() {
        // Plain HTTP: a fingerprint here would come from the same attacker as the file.
        let page = home_page(&[], WebPorts::STANDARD, &|k: &str| k.to_string());
        assert!(!page.contains("fingerprint") && !page.contains("<code>"));
        assert!(page.contains("<p>home.cert_check</p>"));
    }
}
