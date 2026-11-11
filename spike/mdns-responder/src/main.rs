//! Spike mDNS responder for Konak.
//!
//! Announces `<name>.local` host names (plus an `_http._tcp` service each) for one IPv4 address,
//! on the single network interface that owns that address. Before announcing, it asks the LAN
//! whether any of the names is already taken and refuses to start on a conflict.
//! It runs for a fixed number of seconds, then sends goodbye packets and exits.
//!
//! Usage: mdns-responder <ipv4> <seconds> <name>[,<name>...] [--check-only]

use mdns_sd::{HostnameResolutionEvent, IfKind, ServiceDaemon, ServiceInfo};
use std::net::IpAddr;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: mdns-responder <ipv4> <seconds> <name>[,<name>...] [--check-only]");
        std::process::exit(2);
    }
    let ip: IpAddr = args[1].parse().expect("invalid IP address");
    let seconds: u64 = args[2].parse().expect("invalid seconds");
    let names: Vec<&str> = args[3].split(',').collect();
    let check_only = args.iter().any(|a| a == "--check-only");

    let daemon = ServiceDaemon::new().expect("failed to start mDNS daemon");
    // Only the interface that owns `ip`; never VMware, WSL or other virtual adapters.
    daemon.disable_interface(IfKind::All).expect("disable interfaces");
    daemon.enable_interface(IfKind::Addr(ip)).expect("enable interface");

    // Conflict check: any existing answer for one of our names means someone else uses it.
    let mut conflict = false;
    for name in &names {
        let host = format!("{name}.local.");
        let rx = daemon.resolve_hostname(&host, Some(3000)).expect("resolve");
        let deadline = Instant::now() + Duration::from_secs(4);
        while let Ok(event) = rx.recv_deadline(deadline) {
            match event {
                HostnameResolutionEvent::AddressesFound(_, addrs) => {
                    println!("CONFLICT {host} already answers with {addrs:?}");
                    conflict = true;
                    break;
                }
                HostnameResolutionEvent::SearchTimeout(_) => {
                    println!("free     {host}");
                    break;
                }
                _ => {}
            }
        }
    }
    if conflict {
        eprintln!("refusing to announce: name conflict on the LAN");
        let _ = daemon.shutdown();
        std::process::exit(1);
    }
    if check_only {
        let _ = daemon.shutdown();
        return;
    }

    let mut fullnames = Vec::new();
    for name in &names {
        let host = format!("{name}.local.");
        let info = ServiceInfo::new("_http._tcp.local.", name, &host, ip, 80, None::<std::collections::HashMap<String, String>>)
            .expect("service info");
        fullnames.push(info.get_fullname().to_string());
        daemon.register(info).expect("register");
        println!("announcing {host} -> {ip}");
    }

    std::thread::sleep(Duration::from_secs(seconds));

    for fullname in &fullnames {
        if let Ok(rx) = daemon.unregister(fullname) {
            let _ = rx.recv_timeout(Duration::from_secs(2));
        }
    }
    if let Ok(rx) = daemon.shutdown() {
        let _ = rx.recv_timeout(Duration::from_secs(2));
    }
    println!("stopped, goodbye packets sent");
}
