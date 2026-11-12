//! Keeps Konak's runtime pieces alive while the app runs (in the tray):
//! - a keepalive session so WSL never idles the distro out,
//! - Caddy on the LAN IP, proxying the app's name to its port,
//! - the mDNS responder announcing the name.
//! Spike step 3 showed that checking only at logon is not enough (an external `wsl --terminate`
//! left everything down), so this loop checks every few seconds and also follows LAN IP changes.

use crate::paths::Paths;
use crate::shell;
use std::net::UdpSocket;
use std::process::Child;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TICK: Duration = Duration::from_secs(10);
pub const APP_PORT: u16 = 8096;

#[derive(Default)]
struct Procs {
    keepalive: Option<Child>,
    caddy: Option<Child>,
    mdns: Option<Child>,
    ip: Option<String>,
}

pub struct Supervisor {
    paths: Paths,
    name: Mutex<Option<String>>,
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
        Arc::new(Supervisor { paths, name: Mutex::new(None), procs: Mutex::new(Procs::default()), started: Mutex::new(false) })
    }

    pub fn set_name(&self, name: &str) {
        *self.name.lock().unwrap() = Some(name.to_string());
    }

    /// Starts the background loop once; later calls do nothing.
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

    pub fn tick(&self) {
        let Some(name) = self.name.lock().unwrap().clone() else { return };
        let mut p = self.procs.lock().unwrap();
        let log_dir = self.paths.log_dir();

        if !alive(&mut p.keepalive) {
            p.keepalive = shell::spawn_hidden("wsl.exe", &["-d", &self.paths.distro, "-u", "root", "--", "sleep", "infinity"], None).ok();
            crate::log(&self.paths, "supervisor: keepalive (re)started");
        }

        let Some(ip) = lan_ip() else { return };
        if p.ip.as_deref() != Some(ip.as_str()) {
            kill(&mut p.caddy);
            kill(&mut p.mdns);
            let caddyfile = format!(
                "{{\n  admin off\n  auto_https off\n}}\nhttp://{name}.local:80 {{\n  bind {ip}\n  reverse_proxy localhost:{APP_PORT}\n}}\n"
            );
            let _ = std::fs::write(self.paths.caddyfile(), caddyfile);
            crate::log(&self.paths, &format!("supervisor: LAN IP is {ip}"));
            p.ip = Some(ip.clone());
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
            p.mdns = shell::spawn_hidden(&self.paths.mdns().to_string_lossy(), &[&ip, "86400", &name], Some(&log_dir.join("mdns.log"))).ok();
            crate::log(&self.paths, "supervisor: mdns (re)started");
        }
    }

    pub fn stop_mdns(&self) {
        kill(&mut self.procs.lock().unwrap().mdns);
    }

    pub fn stop_all(&self) {
        let mut p = self.procs.lock().unwrap();
        kill(&mut p.mdns);
        kill(&mut p.caddy);
        kill(&mut p.keepalive);
        p.ip = None;
    }
}
