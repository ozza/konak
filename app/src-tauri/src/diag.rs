//! Read-only facts and the spike checks behind the MCP tools (`mcp.rs`). Nothing here changes the
//! system or fixes anything. Any `wsl -d <distro>` starts a stopped distro, so every command inside
//! the distro is gated on `distro().running` (checked with `wsl -l`, which starts nothing).

use crate::installer::now;
use crate::paths::Paths;
use crate::services::WebPorts;
use crate::shell::{self, tail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

const NET_PS1: &str = include_str!("../scripts/net-check.ps1");
/// The supervisor writes its heartbeat every 10 s; older than this means it is not running.
pub const HEARTBEAT_STALE_S: u64 = 60;

/// wsl.exe output: UTF-16LE from some builds and messages even with `WSL_UTF8`, UTF-8 otherwise.
pub fn decode_wsl(bytes: &[u8]) -> String {
    let n16 = bytes.len() / 2;
    let zero_high = bytes.iter().skip(1).step_by(2).filter(|&&b| b == 0).count();
    let text = if bytes.len() >= 2 && bytes.len().is_multiple_of(2) && zero_high * 2 >= n16 {
        let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    };
    text.replace(['\u{feff}', '\0'], "")
}

/// `wsl -l -v` rows as (name, state, version). The header is the row whose last column is not a
/// number, so localized headers and state words do not matter.
pub fn parse_list_v(text: &str) -> Vec<(String, String, u8)> {
    text.lines()
        .filter_map(|l| {
            let cols: Vec<&str> = l.trim().trim_start_matches('*').split_whitespace().collect();
            let version = cols.last()?.parse().ok()?;
            (cols.len() >= 3).then(|| (cols[0].to_string(), cols[1..cols.len() - 1].join(" "), version))
        })
        .collect()
}

/// `wsl -l --running -q`: one distro name per line.
pub fn parse_running(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
}

#[derive(Serialize, Clone, Debug, PartialEq, Default)]
pub struct Distro {
    pub name: String,
    pub exists: bool,
    pub running: bool,
    /// State word as wsl prints it (may be localized).
    pub state: Option<String>,
    pub version: Option<u8>,
}

fn wsl_text(args: &[&str]) -> (bool, String) {
    match shell::hidden("wsl.exe").args(args).output() {
        Ok(o) => (o.status.success(), decode_wsl(&o.stdout)),
        Err(_) => (false, String::new()),
    }
}

/// Konak's distro state. "Running" comes from `--running -q`, which is not localized.
pub fn distro(paths: &Paths) -> Distro {
    let name = paths.distro.clone();
    let (_, list) = wsl_text(&["-l", "-v"]);
    let row = parse_list_v(&list).into_iter().find(|r| r.0.eq_ignore_ascii_case(&name));
    let (ok, running) = wsl_text(&["-l", "--running", "-q"]);
    let running = ok && parse_running(&running).iter().any(|n| n.eq_ignore_ascii_case(&name));
    Distro { exists: row.is_some(), running, state: row.as_ref().map(|r| r.1.clone()), version: row.map(|r| r.2), name }
}

pub fn not_running(d: &Distro) -> String {
    if d.exists {
        format!("distro not running: {} (left stopped; Konak's tray app starts it)", d.name)
    } else {
        format!("distro not running: {} does not exist", d.name)
    }
}

#[derive(Deserialize, Serialize, Default, Clone, Debug)]
pub struct Pids {
    pub keepalive: Option<u32>,
    pub caddy: Option<u32>,
    pub mdns: Option<u32>,
}

/// The supervisor heartbeat (`supervisor.json`, written by `services.rs`).
#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Heartbeat {
    pub last_tick_unix: u64,
    pub lan_ip: Option<String>,
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub pids: Pids,
    #[serde(default)]
    pub https_available: bool,
    #[serde(default)]
    pub caddy_failures: u32,
    /// Ports Caddy serves on; heartbeats from before the port fallback were always 80/443.
    #[serde(default)]
    pub web_ports: WebPorts,
    #[serde(default)]
    pub port80_owner: Option<String>,
}

impl Heartbeat {
    pub fn age(&self, now: u64) -> u64 {
        now.saturating_sub(self.last_tick_unix)
    }
    pub fn stale(&self, now: u64) -> bool {
        self.age(now) > HEARTBEAT_STALE_S
    }
}

pub fn heartbeat(paths: &Paths) -> Option<Heartbeat> {
    serde_json::from_str(&std::fs::read_to_string(paths.supervisor_file()).ok()?).ok()
}

#[repr(C)]
struct MemoryStatus {
    length: u32,
    load: u32,
    total_phys: u64,
    avail_phys: u64,
    total_page: u64,
    avail_page: u64,
    total_virtual: u64,
    avail_virtual: u64,
    avail_ext_virtual: u64,
}

extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
    fn GetExitCodeProcess(process: isize, code: *mut u32) -> i32;
    fn QueryFullProcessImageNameW(process: isize, flags: u32, name: *mut u16, size: *mut u32) -> i32;
    fn CloseHandle(handle: isize) -> i32;
    fn GetDiskFreeSpaceExW(dir: *const u16, avail: *mut u64, total: *mut u64, free: *mut u64) -> i32;
    fn GlobalMemoryStatusEx(status: *mut MemoryStatus) -> i32;
}

/// Image path of a live process; `None` when it has exited or cannot be queried.
pub fn process_image(pid: u32) -> Option<String> {
    const QUERY_LIMITED: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;
    // SAFETY: plain Win32 calls on a handle we own and close; the buffer size is passed in.
    unsafe {
        let h = OpenProcess(QUERY_LIMITED, 0, pid);
        if h == 0 {
            return None;
        }
        let mut code = 0;
        let alive = GetExitCodeProcess(h, &mut code) != 0 && code == STILL_ACTIVE;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let named = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) != 0;
        CloseHandle(h);
        (alive && named).then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

const MB: u64 = 1_048_576;

/// (free, total) bytes of the drive holding `dir` (or its nearest existing parent).
pub fn disk_space_bytes(dir: &Path) -> Option<(u64, u64)> {
    let dir = dir.ancestors().find(|p| p.exists())?;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let (mut avail, mut total, mut free) = (0, 0, 0);
    // SAFETY: NUL-terminated path and valid out pointers.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free) } != 0;
    ok.then_some((avail, total))
}

/// (free, total) MB of the drive holding `dir` (or its nearest existing parent).
pub fn disk_space(dir: &Path) -> Option<(u64, u64)> {
    disk_space_bytes(dir).map(|(free, total)| (free / MB, total / MB))
}

/// (available, total) MB of host RAM.
pub fn host_ram() -> Option<(u64, u64)> {
    let mut m = MemoryStatus { length: std::mem::size_of::<MemoryStatus>() as u32, load: 0, total_phys: 0, avail_phys: 0, total_page: 0, avail_page: 0, total_virtual: 0, avail_virtual: 0, avail_ext_virtual: 0 };
    // SAFETY: `length` is set as the API requires.
    let ok = unsafe { GlobalMemoryStatusEx(&mut m) } != 0;
    ok.then_some((m.avail_phys / MB, m.total_phys / MB))
}

#[derive(Serialize, Debug, PartialEq)]
pub struct Usage {
    pub total_mb: u64,
    pub used_mb: u64,
    pub available_mb: u64,
}

/// The `Mem:` row of `free -m`.
pub fn parse_free(text: &str) -> Option<Usage> {
    let cols: Vec<u64> = text.lines().find(|l| l.starts_with("Mem:"))?.split_whitespace().skip(1).filter_map(|c| c.parse().ok()).collect();
    Some(Usage { total_mb: *cols.first()?, used_mb: *cols.get(1)?, available_mb: *cols.get(5).or(cols.get(2))? })
}

/// The first data row of `df -P -m`.
pub fn parse_df(text: &str) -> Option<Usage> {
    text.lines().find_map(|l| {
        let c: Vec<&str> = l.split_whitespace().collect();
        (c.len() >= 6).then_some(())?;
        Some(Usage { total_mb: c[1].parse().ok()?, used_mb: c[2].parse().ok()?, available_mb: c[3].parse().ok()? })
    })
}

#[derive(Deserialize, Serialize, Clone, Default, Debug)]
pub struct Adapter {
    pub alias: String,
    #[serde(default)]
    pub description: String,
    pub index: i64,
    #[serde(default)]
    pub category: String,
}

impl Adapter {
    /// Virtual adapters (Hyper-V/WSL vEthernet, VMware, VirtualBox, VPN taps) are never the LAN.
    pub fn is_virtual(&self) -> bool {
        let s = format!("{} {}", self.alias, self.description).to_lowercase();
        ["vethernet", "vmware", "virtualbox", "hyper-v", "virtual", "wsl", "tap-", "loopback"].iter().any(|w| s.contains(w))
    }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct FwRule {
    pub name: String,
    pub enabled: bool,
    pub action: String,
    pub profile: String,
    pub direction: String,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct FwProgram {
    pub program: String,
    #[serde(default)]
    pub rules: Vec<FwRule>,
}

impl FwProgram {
    /// An enabled inbound allow rule that covers the Private profile.
    pub fn private_allow(&self) -> bool {
        self.rules.iter().any(|r| r.enabled && r.action == "Allow" && r.direction == "Inbound" && (r.profile.contains("Private") || r.profile == "Any"))
    }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Listener {
    pub address: String,
    pub pid: u32,
    #[serde(default)]
    pub image: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Default, Debug)]
pub struct NetFacts {
    pub adapter: Option<Adapter>,
    #[serde(default)]
    pub firewall: Vec<FwProgram>,
    #[serde(default)]
    pub port80: Vec<Listener>,
    /// Listeners on the fallback HTTP port (`WebPorts::FALLBACK.http`).
    #[serde(default)]
    pub fallback_http: Vec<Listener>,
}

fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Adapter, firewall rules, and port 80 and fallback port listeners, through one read-only
/// PowerShell run.
pub fn net_facts(paths: &Paths, ip: Option<&str>) -> Result<NetFacts, String> {
    let progs = [paths.caddy(), paths.mdns()].map(|p| ps_quote(&p.to_string_lossy())).join(",");
    let cmd = format!("& {{\n{NET_PS1}\n}} -Ip {} -Programs {progs} -FallbackPort {}", ps_quote(ip.unwrap_or("")), WebPorts::FALLBACK.http);
    let out = shell::run("powershell.exe", &["-NoProfile", "-NonInteractive", "-Command", &cmd]);
    let json = out.text.lines().rev().find(|l| l.trim_start().starts_with('{')).unwrap_or("");
    let mut facts: NetFacts = serde_json::from_str(json).map_err(|e| format!("network facts unreadable: {e}: {}", tail(&out.text, 3)))?;
    for l in facts.port80.iter_mut().chain(facts.fallback_http.iter_mut()) {
        l.image = process_image(l.pid);
    }
    Ok(facts)
}

/// Whether a listener on `ip` (or on all addresses) is Konak's Caddy: the heartbeat's pid, or the
/// bundled caddy.exe.
fn konak_listener(l: &Listener, ip: &str, caddy_pid: Option<u32>, caddy: &str) -> bool {
    (l.address == ip || l.address == "0.0.0.0" || l.address == "::") && (Some(l.pid) == caddy_pid || l.image.as_deref().is_some_and(|i| i.eq_ignore_ascii_case(caddy)))
}

/// The other program holding port 80 on `ip`, as `httpd.exe (pid 1234)`. `None` when there is none
/// or its image cannot be read (pid 4 is always the System process: http.sys, e.g. IIS).
pub fn port80_owner(net: &NetFacts, ip: &str, caddy: &str) -> Option<String> {
    let other = net.port80.iter().find(|l| (l.address == ip || l.address == "0.0.0.0" || l.address == "::") && !konak_listener(l, ip, None, caddy))?;
    let image = match (&other.image, other.pid) {
        (Some(path), _) => path.rsplit(['\\', '/']).next().unwrap_or(path).to_string(),
        (None, 4) => "System".to_string(),
        (None, _) => return None,
    };
    Some(format!("{image} (pid {})", other.pid))
}

/// Whether Konak's Caddy (the heartbeat's pid, or the bundled caddy.exe) listens on port 80 of the
/// LAN IP or on all addresses.
pub fn konak_owns_port80(net: &NetFacts, ip: Option<&str>, caddy_pid: Option<u32>, caddy: &str) -> bool {
    ip.is_some_and(|ip| {
        net.port80.iter().any(|l| (l.address == ip || l.address == "0.0.0.0") && (Some(l.pid) == caddy_pid || l.image.as_deref().is_some_and(|i| i.eq_ignore_ascii_case(caddy))))
    })
}

/// LAN networking: the MCP `network_check` tool and the panel's host view.
pub struct Network {
    pub lan_ip: Option<String>,
    pub facts: NetFacts,
    pub konak_owns_port80: bool,
    /// The ports the supervisor last served on (its heartbeat), and the port 80 owner it found.
    pub web_ports: WebPorts,
    pub port80_owner: Option<String>,
}

pub fn network(paths: &Paths) -> Result<Network, String> {
    let lan_ip = crate::services::lan_ip();
    let facts = net_facts(paths, lan_ip.as_deref())?;
    let hb = heartbeat(paths);
    let caddy_pid = hb.as_ref().and_then(|h| h.pids.caddy);
    let konak_owns_port80 = konak_owns_port80(&facts, lan_ip.as_deref(), caddy_pid, &paths.caddy().to_string_lossy());
    let (web_ports, port80_owner) = hb.map(|h| (h.web_ports, h.port80_owner)).unwrap_or_default();
    Ok(Network { lan_ip, facts, konak_owns_port80, web_ports, port80_owner })
}

/// Output of a read-only script run as root in the distro, or `None` when it is not running.
pub fn in_distro(paths: &Paths, d: &Distro, script: &str) -> Option<String> {
    d.running.then(|| shell::distro_script(&paths.distro, script, &[]).text)
}

pub const FORWARD_SH: &str = "command -v nft >/dev/null && { echo '## nft'; nft list ruleset 2>&1 | grep -E 'hook forward' || true; }\n\
command -v iptables-legacy >/dev/null && { echo '## iptables-legacy'; iptables-legacy -S FORWARD 2>&1 | head -3; }\ntrue\n";

pub const NETAVARK_SH: &str = r#"if nft list table inet netavark >/dev/null 2>&1; then echo 'table present'; else echo 'table missing'; fi
podman ps --format '{{.Names}}|{{.Ports}}' 2>/dev/null | while IFS='|' read -r name ports; do
  echo "container $name"
  for hp in $(printf '%s' "$ports" | grep -oE ':[0-9]+->' | tr -d ':>-' | sort -u); do
    if timeout 3 bash -c "</dev/tcp/127.0.0.1/$hp" 2>/dev/null; then echo "probe $name $hp open"; else echo "probe $name $hp closed"; fi
  done
done
if ids=$(podman network ls --no-trunc --format '{{.ID}}' 2>/dev/null); then echo '## podman networks'; printf '%s\n' "$ids"; fi
if j=$(nft -j list table inet netavark 2>/dev/null) && [ "${j#\{}" != "$j" ]; then echo '## nft json'; printf '%s\n' "$j"
elif t=$(nft -a list table inet netavark 2>/dev/null); then echo '## nft text'; printf '%s\n' "$t"; fi
"#;

/// The lines of `NETAVARK_SH` output under `## <name>`, up to the next `## ` line.
fn section<'a>(out: &'a str, name: &str) -> Option<Vec<&'a str>> {
    let mut lines = out.lines().skip_while(|l| l.strip_prefix("## ") != Some(name));
    lines.next()?;
    Some(lines.take_while(|l| !l.starts_with("## ")).collect())
}

/// One rule of the `inet netavark` table, reduced to what the stale-rule check needs.
#[derive(Debug, PartialEq, Default)]
pub struct NftRule {
    pub chain: String,
    pub handle: Option<u64>,
    pub dports: Vec<u16>,
    pub jump: Option<String>,
}

#[derive(Debug, PartialEq, Default)]
pub struct NftTable {
    pub chains: Vec<String>,
    pub rules: Vec<NftRule>,
}

/// `nft [-a] list table inet netavark`. Handles are read when `-a` printed them (`# handle N`).
pub fn parse_nft_text(text: &str) -> NftTable {
    let mut t = NftTable::default();
    let mut chain: Option<String> = None;
    for line in text.lines() {
        let (body, handle) = match line.split_once("# handle ") {
            Some((b, h)) => (b.trim(), h.trim().parse().ok()),
            None => (line.trim(), None),
        };
        if let Some(name) = body.strip_prefix("chain ").and_then(|r| r.strip_suffix('{')) {
            t.chains.push(name.trim().to_string());
            chain = Some(name.trim().to_string());
            continue;
        }
        if body == "}" {
            chain = None;
            continue;
        }
        let Some(c) = &chain else { continue };
        if body.is_empty() || body.starts_with("type ") {
            continue;
        }
        let words: Vec<&str> = body.split_whitespace().collect();
        let dports = words.windows(2).filter(|w| w[0] == "dport").filter_map(|w| w[1].parse().ok()).collect();
        let jump = words.windows(2).find(|w| w[0] == "jump" || w[0] == "goto").map(|w| w[1].to_string());
        t.rules.push(NftRule { chain: c.clone(), handle, dports, jump });
    }
    t
}

/// `nft -j list table inet netavark` (libnftables JSON); `None` when it is not that.
pub fn parse_nft_json(text: &str) -> Option<NftTable> {
    let v: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    let mut t = NftTable::default();
    for item in v.get("nftables")?.as_array()? {
        if let Some(name) = item.get("chain").and_then(|c| c.get("name")).and_then(|n| n.as_str()) {
            t.chains.push(name.to_string());
        }
        let Some(r) = item.get("rule") else { continue };
        let mut rule = NftRule { chain: r.get("chain").and_then(|c| c.as_str()).unwrap_or_default().to_string(), handle: r.get("handle").and_then(|h| h.as_u64()), ..Default::default() };
        for e in r.get("expr").and_then(|e| e.as_array()).into_iter().flatten() {
            if let Some(m) = e.get("match").filter(|m| m.pointer("/left/payload/field").and_then(|f| f.as_str()) == Some("dport")) {
                rule.dports.extend(m.get("right").and_then(|p| p.as_u64()).and_then(|p| u16::try_from(p).ok()));
            }
            if let Some(target) = e.get("jump").or(e.get("goto")).and_then(|j| j.get("target")).and_then(|t| t.as_str()) {
                rule.jump = Some(target.to_string());
            }
        }
        t.rules.push(rule);
    }
    Some(t)
}

/// The network id prefix netavark puts in its chain names: `nv_<first 8 hex of the id>_<subnet>`.
fn nv_id(chain: &str) -> Option<&str> {
    let id = chain.strip_prefix("nv_")?.split('_').next()?;
    (id.len() == 8 && id.bytes().all(|b| b.is_ascii_hexdigit())).then_some(id)
}

/// A netavark network unknown to Konak's Podman that still forwards one of Konak's catalog ports.
#[derive(Debug, PartialEq)]
pub struct ForeignNetwork {
    pub id: String,
    pub ports: Vec<u16>,
    /// Rules outside the network's own chains that jump into them, as (chain, handle).
    pub jumps: Vec<(String, Option<u64>)>,
    pub chains: Vec<String>,
}

/// Spike finding N1: a removed distro's network keeps a DNAT jump for a Konak port in
/// `NETAVARK-HOSTPORT-DNAT`; when it sits before Konak's own jump, that port is black-holed.
pub fn foreign_konak_dnat(t: &NftTable, known: &[&str]) -> Vec<ForeignNetwork> {
    let mut found: Vec<ForeignNetwork> = vec![];
    for r in t.rules.iter().filter(|r| r.chain == "NETAVARK-HOSTPORT-DNAT") {
        let Some(id) = r.jump.as_deref().and_then(nv_id) else { continue };
        let ports: Vec<u16> = r.dports.iter().copied().filter(|p| crate::catalog::PORT_RANGE.contains(p)).collect();
        if ports.is_empty() || known.iter().any(|k| k.starts_with(id)) {
            continue;
        }
        match found.iter_mut().find(|n| n.id == id) {
            Some(n) => n.ports.extend(ports),
            None => found.push(ForeignNetwork { id: id.to_string(), ports, jumps: vec![], chains: vec![] }),
        }
    }
    for n in &mut found {
        let own = |c: &str| nv_id(c) == Some(n.id.as_str());
        n.chains = t.chains.iter().filter(|c| own(c)).cloned().collect();
        n.jumps = t.rules.iter().filter(|r| !own(&r.chain) && r.jump.as_deref().is_some_and(own)).map(|r| (r.chain.clone(), r.handle)).collect();
        n.ports.sort_unstable();
        n.ports.dedup();
    }
    found
}

/// Commands that delete only this network's jumps, then its chains.
fn foreign_fix(n: &ForeignNetwork) -> String {
    let mut cmds: Vec<String> = n
        .jumps
        .iter()
        .map(|(chain, handle)| match handle {
            Some(h) => format!("nft delete rule inet netavark {chain} handle {h}"),
            None => format!("nft delete rule inet netavark {chain} handle <N> (N: the handle of its `jump nv_{}_…` rule in `nft -a list chain inet netavark {chain}`)", n.id),
        })
        .collect();
    cmds.extend(n.chains.iter().map(|c| format!("nft delete chain inet netavark {c}")));
    cmds.join("; ")
}

pub const RESOURCES_SH: &str = "free -m\necho ---\ndf -P -m /\n";

/// Memory and root disk usage inside the distro; both `None` when it is not running.
pub fn distro_usage(paths: &Paths, d: &Distro) -> (Option<Usage>, Option<Usage>) {
    in_distro(paths, d, RESOURCES_SH).as_deref().map(|t| t.split_once("---").unwrap_or((t, ""))).map(|(m, df)| (parse_free(m), parse_df(df))).unwrap_or((None, None))
}

/// The panel's "This PC" view. `category` is the network category Konak last read (the panel's
/// `refresh_network`); when there is none, the LAN adapter's own category is used.
pub fn host_info(paths: &Paths, category: Option<String>) -> serde_json::Value {
    use serde_json::json;
    let d = distro(paths);
    let (mem, disk) = distro_usage(paths, &d);
    let used_total = |u: Option<Usage>| u.map(|u| json!({"used_bytes": u.used_mb * MB, "total_bytes": u.total_mb * MB}));
    let net = network(paths).map_err(|e| crate::log(paths, &format!("host info: {e}"))).ok();
    let adapter = net.as_ref().and_then(|n| n.facts.adapter.as_ref());
    let category = category.filter(|c| !c.is_empty()).or_else(|| adapter.map(|a| a.category.clone()).filter(|c| !c.is_empty()));
    json!({
        "distro": {"name": d.name, "running": d.running},
        "memory": used_total(mem),
        "disk": used_total(disk),
        "data_drive": disk_space_bytes(&paths.home).map(|(free, total)| json!({"free_bytes": free, "total_bytes": total})),
        "network": {
            "lan_ip": net.as_ref().map_or_else(crate::services::lan_ip, |n| n.lan_ip.clone()),
            "adapter": adapter.map(|a| a.alias.clone()),
            "category": category,
            // As the panel's firewall notice does: rules that cannot be read are not reported missing.
            "firewall_ok": net.as_ref().is_none_or(|n| !crate::installer::rules_missing(&n.facts.firewall)),
            "konak_owns_port80": net.as_ref().is_some_and(|n| n.konak_owns_port80),
        },
    })
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Check {
    pub check: &'static str,
    /// ok | warn | fail
    pub status: &'static str,
    pub evidence: String,
    pub fix: String,
    /// What the panel shows: `panel.diag_msg.<code>` and `panel.diag_fix.<code>` in its locale files,
    /// filled with `args`. `evidence` and `fix` stay the technical English text for MCP clients and logs.
    pub code: &'static str,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub args: BTreeMap<&'static str, String>,
}

impl Check {
    fn msg(mut self, code: &'static str, args: &[(&'static str, String)]) -> Self {
        self.code = code;
        self.args = args.iter().cloned().collect();
        self
    }
}

fn check(check: &'static str, status: &'static str, evidence: impl Into<String>, fix: &str) -> Check {
    Check { check, status, evidence: evidence.into(), fix: fix.to_string(), code: "", args: BTreeMap::new() }
}

/// Everything `diagnose` looks at, gathered once.
pub struct Facts {
    pub now: u64,
    pub distro: Distro,
    pub heartbeat: Option<Heartbeat>,
    pub lan_ip: Option<String>,
    pub net: Result<NetFacts, String>,
    /// Image path of the heartbeat's keepalive pid, when that process is alive.
    pub keepalive_image: Option<String>,
    /// Konak's bundled caddy.exe path.
    pub caddy: String,
    /// Outputs of `FORWARD_SH` and `NETAVARK_SH`; `None` when the distro is not running.
    pub forward: Option<String>,
    pub netavark: Option<String>,
}

pub fn gather(paths: &Paths) -> Facts {
    let distro = distro(paths);
    let heartbeat = heartbeat(paths);
    let lan_ip = crate::services::lan_ip();
    let net = net_facts(paths, lan_ip.as_deref());
    let keepalive_image = heartbeat.as_ref().and_then(|h| h.pids.keepalive).and_then(process_image);
    let (forward, netavark) = (in_distro(paths, &distro, FORWARD_SH), in_distro(paths, &distro, NETAVARK_SH));
    Facts { now: now(), distro, heartbeat, lan_ip, net, keepalive_image, caddy: paths.caddy().to_string_lossy().to_string(), forward, netavark }
}

pub fn forward_policy(f: &Facts) -> Check {
    const FIX: &str = "Another WSL distro's Docker set FORWARD to DROP in the shared WSL network namespace; there, run `iptables -P FORWARD ACCEPT` (or stop that Docker), then restart Konak's apps.";
    let Some(out) = &f.forward else { return check("forward_policy", "warn", not_running(&f.distro), "Start Konak from the tray so the distro runs, then diagnose again.").msg("distro_down", &[]) };
    if !out.contains("## ") {
        return check("forward_policy", "warn", "neither nft nor iptables-legacy found in the distro", "Reinstall the container engine from Konak's setup.").msg("forward_policy.no_tool", &[]);
    }
    let drops: Vec<&str> = out.lines().filter(|l| l.contains("-P FORWARD DROP") || (l.contains("hook forward") && l.contains("policy drop"))).map(str::trim).collect();
    if drops.is_empty() {
        check("forward_policy", "ok", "no FORWARD chain with a DROP policy", "").msg("forward_policy.ok", &[])
    } else {
        check("forward_policy", "fail", drops.join("; "), FIX).msg("forward_policy.drop", &[])
    }
}

pub fn stale_netavark(f: &Facts) -> Check {
    // Never `nft delete table inet netavark`: every WSL distro shares one network namespace, so that
    // also wipes the port forwards of any other Podman distro on the PC.
    const FIX: &str = "In Konak's distro as root, run `podman network reload --all`: it rebuilds the port forwards of Konak's running containers and leaves other distros' rules alone. konak-boot.service runs the same reload at every boot of Konak's distro. Do not delete the shared `inet netavark` table.";
    let Some(all) = &f.netavark else { return check("stale_netavark", "warn", not_running(&f.distro), "Start Konak from the tray so the distro runs, then diagnose again.").msg("distro_down", &[]) };
    // The probe lines come before the first `## ` section (networks, rule dump).
    let probes: Vec<&str> = all.lines().take_while(|l| !l.starts_with("## ")).collect();
    let containers = probes.iter().filter(|l| l.starts_with("container ")).count();
    let closed: Vec<String> = probes.iter().filter_map(|l| l.strip_prefix("probe ")).filter(|l| l.ends_with(" closed")).map(|l| l.trim_end_matches(" closed").replace(' ', ":")).collect();
    if containers > 0 && probes.contains(&"table missing") {
        return check("stale_netavark", "fail", format!("{containers} containers running but the shared inet netavark table is missing (another Podman distro may have flushed it)"), FIX)
            .msg("stale_netavark.table_missing", &[("containers", containers.to_string())]);
    }
    // Only when Konak's Podman listed its networks: otherwise every network would look foreign.
    if let Some(known) = section(all, "podman networks") {
        let table = section(all, "nft json").and_then(|j| parse_nft_json(&j.join("\n"))).or_else(|| section(all, "nft text").map(|t| parse_nft_text(&t.join("\n"))));
        let foreign = table.map(|t| foreign_konak_dnat(&t, &known)).unwrap_or_default();
        if !foreign.is_empty() {
            let evidence = foreign.iter().map(|n| format!("network {} is not one of Konak's Podman networks (left by a removed distro?) but still forwards port {}", n.id, n.ports.iter().map(u16::to_string).collect::<Vec<_>>().join(", "))).collect::<Vec<_>>().join("; ");
            let fix = format!(
                "In Konak's distro as root, delete only that network's rules: {}. Then run `podman network reload --all`. Do not delete the shared `inet netavark` table.",
                foreign.iter().map(foreign_fix).collect::<Vec<_>>().join("; ")
            );
            let ports = foreign.iter().flat_map(|n| n.ports.iter().map(u16::to_string)).collect::<Vec<_>>().join(", ");
            return Check { check: "stale_netavark", status: "fail", evidence, fix, code: "", args: BTreeMap::new() }.msg("stale_netavark.foreign", &[("ports", ports)]);
        }
    }
    if !closed.is_empty() {
        return check("stale_netavark", "warn", format!("published ports not answering inside the distro: {} (container still starting, or stale DNAT rules)", closed.join(", ")), FIX)
            .msg("stale_netavark.closed", &[("ports", closed.join(", "))]);
    }
    let open = probes.iter().filter(|l| l.ends_with(" open")).count();
    check("stale_netavark", "ok", format!("{containers} running containers, {open} published ports answering"), "")
        .msg("stale_netavark.ok", &[("containers", containers.to_string()), ("open", open.to_string())])
}

pub fn keepalive(f: &Facts) -> Check {
    const START: &str = "Start Konak (or quit and restart it from the tray); its supervisor restarts the keepalive within 10 s.";
    let Some(hb) = &f.heartbeat else { return check("keepalive", "warn", "no supervisor heartbeat: Konak's tray app has not run yet", START).msg("keepalive.no_heartbeat", &[]) };
    if hb.stale(f.now) {
        return check("keepalive", "fail", format!("supervisor heartbeat is {} s old (stale after {HEARTBEAT_STALE_S} s)", hb.age(f.now)), START).msg("keepalive.stale", &[("age", hb.age(f.now).to_string())]);
    }
    let pid = hb.pids.keepalive.map(|p| p.to_string()).unwrap_or_else(|| "none".into());
    let wsl = f.keepalive_image.as_deref().is_some_and(|i| i.to_lowercase().ends_with("wsl.exe"));
    if !wsl {
        return check("keepalive", "fail", format!("keepalive pid {pid} is not a live wsl.exe"), START).msg("keepalive.not_running", &[]);
    }
    if !f.distro.running {
        return check("keepalive", "fail", format!("keepalive pid {pid} alive but {}", not_running(&f.distro)), START).msg("distro_down", &[]);
    }
    check("keepalive", "ok", format!("keepalive pid {pid} alive, distro {} running", f.distro.name), "").msg("keepalive.ok", &[])
}

pub fn port80(f: &Facts) -> Check {
    let net = match &f.net {
        Ok(n) => n,
        Err(e) => return check("port80_owner", "warn", e.clone(), "Run the check again; PowerShell's NetTCPIP module must be available.").msg("port80_owner.net_error", &[]),
    };
    let Some(ip) = &f.lan_ip else { return check("port80_owner", "warn", "no LAN IP", "Connect the PC to the home network.").msg("no_network", &[]) };
    let caddy_pid = f.heartbeat.as_ref().and_then(|h| h.pids.caddy);
    let relevant: Vec<&Listener> = net.port80.iter().filter(|l| l.address == *ip || l.address == "0.0.0.0" || l.address == "::").collect();
    let konak = |l: &Listener| Some(l.pid) == caddy_pid || l.image.as_deref().is_some_and(|i| i.eq_ignore_ascii_case(&f.caddy));
    // The first other program, as "httpd.exe (pid 4120)", for the panel's message.
    let owner = relevant.iter().find(|l| !konak(l)).map(|l| {
        let image = l.image.as_deref().map(|i| i.rsplit(['\\', '/']).next().unwrap_or(i).to_string()).unwrap_or_else(|| "?".into());
        format!("{image} (pid {})", l.pid)
    });
    let others: Vec<String> = relevant.iter().filter(|l| !konak(l)).map(|l| format!("{} pid {} {}", l.address, l.pid, l.image.as_deref().unwrap_or("(image unknown)"))).collect();
    // Konak's Caddy on the fallback port: apps stay reachable there, so a warning, not a failure.
    let fallback = WebPorts::FALLBACK;
    let on_fallback = f.heartbeat.as_ref().is_some_and(|h| !h.stale(f.now) && h.web_ports.is_fallback()) && net.fallback_http.iter().any(|l| konak_listener(l, ip, caddy_pid, &f.caddy));
    let serving = format!("Konak serves its apps on the fallback ports instead: {} (HTTPS port {})", fallback.url(false, "<name>"), fallback.https);
    if !others.is_empty() {
        let evidence = format!("port 80 on {ip} held by another program: {}", others.join("; "));
        if on_fallback {
            return check("port80_owner", "warn", format!("{evidence}; {serving}"), "Stopping or reconfiguring that program frees port 80; Konak then returns to port 80 by itself within about 2 minutes.")
                .msg("port80_owner.fallback", &[("owner", owner.clone().unwrap_or_default()), ("port", fallback.http.to_string())]);
        }
        return check("port80_owner", "fail", evidence, "Stop or reconfigure that program so port 80 on the LAN IP is free; Konak's Caddy then takes it within 10 s.")
            .msg("port80_owner.taken", &[("owner", owner.unwrap_or_default())]);
    }
    if on_fallback {
        return check("port80_owner", "warn", format!("port 80 on {ip} is free again; {serving} until port 80 tests free twice, a minute apart"), "Nothing to do: Konak returns to port 80 by itself within about 2 minutes.")
            .msg("port80_owner.returning", &[]);
    }
    match relevant.first() {
        Some(l) => check("port80_owner", "ok", format!("Konak's caddy (pid {}) listens on {}:80", l.pid, l.address), "").msg("port80_owner.ok", &[]),
        None => {
            let failures = f.heartbeat.as_ref().map_or(0, |h| h.caddy_failures);
            let why = if failures > 0 { format!("; Konak's caddy exited {failures} times in a row (see logs\\caddy.log)") } else { String::new() };
            check("port80_owner", "warn", format!("nothing listens on {ip}:80{why}"), "Start Konak from the tray; its supervisor starts Caddy.").msg("port80_owner.nothing", &[])
        }
    }
}

pub fn lan_adapter(f: &Facts) -> Check {
    let Some(ip) = &f.lan_ip else { return check("lan_adapter", "fail", "no IPv4 default route", "Connect the PC to the home network (Ethernet or Wi-Fi).").msg("no_network", &[]) };
    let adapter = f.net.as_ref().ok().and_then(|n| n.adapter.clone());
    let name = adapter.as_ref().map(|a| format!("{} ({})", a.alias, a.description)).unwrap_or_else(|| "unknown adapter".into());
    if adapter.as_ref().is_some_and(Adapter::is_virtual) {
        return check("lan_adapter", "fail", format!("LAN IP {ip} is on virtual adapter {name}"), "Disable the VPN or virtual switch that holds the default route, or give the physical adapter a lower interface metric.")
            .msg("lan_adapter.virtual", &[("adapter", adapter.as_ref().map(|a| a.alias.clone()).unwrap_or_default())]);
    }
    match f.heartbeat.as_ref().filter(|h| !h.stale(f.now)).and_then(|h| h.lan_ip.clone()) {
        Some(served) if served != *ip => check("lan_adapter", "warn", format!("supervisor serves {served}, current LAN IP is {ip} on {name}"), "Wait 10 s for the supervisor to follow the new IP; if it stays, restart Konak.")
            .msg("lan_adapter.ip_changed", &[]),
        _ => check("lan_adapter", "ok", format!("LAN IP {ip} on {name}"), "")
            .msg("lan_adapter.ok", &[("ip", ip.clone()), ("adapter", adapter.as_ref().map(|a| a.alias.clone()).unwrap_or_default())]),
    }
}

pub fn diagnose(f: &Facts) -> Vec<Check> {
    vec![forward_policy(f), stale_netavark(f), keepalive(f), port80(f), lan_adapter(f)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_utf16_and_utf8() {
        let s = "  NAME      STATE   VERSION\r\n* Ubuntu-24.04  Running 2\r\n";
        let utf16: Vec<u8> = [0xFF, 0xFE].into_iter().chain(s.encode_utf16().flat_map(u16::to_le_bytes)).collect();
        assert_eq!(decode_wsl(&utf16), s);
        assert_eq!(decode_wsl(s.as_bytes()), s);
        assert_eq!(decode_wsl("ab\0".as_bytes()), "ab");
    }

    #[test]
    fn parses_wsl_lists() {
        let text = "  NAME            STATE           VERSION\r\n* Ubuntu-24.04    Running         2\r\n  konak-podman    Stopped         2\r\n  docker-desktop  Durduruldu      2\r\n\r\n";
        let rows = parse_list_v(text);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], ("Ubuntu-24.04".into(), "Running".into(), 2));
        assert_eq!(rows[2], ("docker-desktop".into(), "Durduruldu".into(), 2));
        // A Turkish header is skipped too: its last column is not a number.
        assert!(parse_list_v("  AD   DURUM   SÜRÜM\r\n").is_empty());
        assert_eq!(parse_running("Ubuntu-24.04\r\nkonak-podman\r\n\r\n"), vec!["Ubuntu-24.04", "konak-podman"]);
    }

    #[test]
    fn parses_free_and_df() {
        let free = "               total        used        free      shared  buff/cache   available\nMem:           15911        1203       13850           3        1088       14707\nSwap:           4096           0        4096\n";
        assert_eq!(parse_free(free), Some(Usage { total_mb: 15911, used_mb: 1203, available_mb: 14707 }));
        let df = "Filesystem     1048576-blocks  Used Available Capacity Mounted on\n/dev/sdd              1031018  4210    974364       1% /\n";
        assert_eq!(parse_df(df), Some(Usage { total_mb: 1031018, used_mb: 4210, available_mb: 974364 }));
        assert_eq!(parse_free("nonsense"), None);
    }

    fn facts() -> Facts {
        Facts {
            now: 1000,
            distro: Distro { name: "konak".into(), exists: true, running: true, state: Some("Running".into()), version: Some(2) },
            heartbeat: Some(Heartbeat { last_tick_unix: 995, lan_ip: Some("192.168.1.5".into()), names: vec!["konak".into()], pids: Pids { keepalive: Some(11), caddy: Some(22), mdns: Some(33) }, https_available: true, caddy_failures: 0, web_ports: WebPorts::STANDARD, port80_owner: None }),
            lan_ip: Some("192.168.1.5".into()),
            net: Ok(NetFacts {
                adapter: Some(Adapter { alias: "Ethernet".into(), description: "Intel(R) Ethernet I225-V".into(), index: 7, category: "Private".into() }),
                firewall: vec![],
                port80: vec![Listener { address: "192.168.1.5".into(), pid: 22, image: Some(r"C:\Konak\bin\caddy.exe".into()) }],
                fallback_http: vec![],
            }),
            keepalive_image: Some(r"C:\Windows\System32\wsl.exe".into()),
            caddy: r"C:\Konak\bin\caddy.exe".into(),
            forward: Some("## nft\n\t\ttype filter hook forward priority filter; policy accept;\n".into()),
            netavark: Some("table present\ncontainer jellyfin\nprobe jellyfin 47101 open\n".into()),
        }
    }

    fn statuses(f: &Facts) -> Vec<&'static str> {
        diagnose(f).iter().map(|c| c.status).collect()
    }

    #[test]
    fn healthy_facts_pass_every_check() {
        let f = facts();
        let checks = diagnose(&f);
        assert_eq!(checks.iter().map(|c| c.check).collect::<Vec<_>>(), ["forward_policy", "stale_netavark", "keepalive", "port80_owner", "lan_adapter"]);
        assert!(checks.iter().all(|c| c.status == "ok" && c.fix.is_empty()), "{checks:?}");
        assert_eq!(checks.iter().map(|c| c.code).collect::<Vec<_>>(), ["forward_policy.ok", "stale_netavark.ok", "keepalive.ok", "port80_owner.ok", "lan_adapter.ok"]);
    }

    #[test]
    fn problems_are_reported_with_a_fix() {
        let mut f = facts();
        f.forward = Some("## nft\n\t\ttype filter hook forward priority filter; policy drop;\n## iptables-legacy\n-P FORWARD DROP\n".into());
        f.netavark = Some("table missing\ncontainer jellyfin\nprobe jellyfin 47101 closed\n".into());
        f.keepalive_image = None;
        f.net.as_mut().unwrap().port80 = vec![Listener { address: "0.0.0.0".into(), pid: 4, image: Some(r"C:\Windows\System32\httpd.exe".into()) }];
        f.net.as_mut().unwrap().adapter = Some(Adapter { alias: "vEthernet (WSL)".into(), description: "Hyper-V Virtual Ethernet Adapter".into(), index: 3, category: "Public".into() });
        let checks = diagnose(&f);
        assert!(checks.iter().all(|c| c.status == "fail" && !c.fix.is_empty()), "{checks:?}");
        assert!(checks[0].evidence.contains("-P FORWARD DROP"));
        assert!(checks[3].evidence.contains("httpd.exe"));
        assert!(checks[1].fix.contains("podman network reload --all") && !checks[1].fix.contains("nft delete"));
        assert_eq!(checks.iter().map(|c| c.code).collect::<Vec<_>>(), ["forward_policy.drop", "stale_netavark.table_missing", "keepalive.not_running", "port80_owner.taken", "lan_adapter.virtual"]);
        assert_eq!(checks[3].args["owner"], "httpd.exe (pid 4)");
        assert_eq!(checks[4].args["adapter"], "vEthernet (WSL)");

        // Only a closed port while the table exists: a warning (container may still be starting).
        let mut f = facts();
        f.netavark = Some("table present\ncontainer immich\nprobe immich 47102 closed\n".into());
        assert_eq!(stale_netavark(&f).status, "warn");
        assert!(stale_netavark(&f).evidence.contains("immich:47102"));
    }

    // Run 3 of the spike (finding N1): the dead konak-subnettest network 872ecb86 still forwards
    // 47105, in front of the forgejo network 655c363a of Konak's own Podman.
    const N1_BEFORE: &str = include_str!("testdata/nft-stale-before-forgejo.txt");
    const N1_AFTER: &str = include_str!("testdata/nft-after-stale-cleanup.txt");
    const FORGEJO_NET: &str = "655c363a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6";
    const DEFAULT_NET: &str = "2f259bab93aaaaa2542ba43ef33eb990d0999ee1b9924b557b7be53c0b7a1bb9";

    #[test]
    fn parses_the_n1_dump_and_finds_the_foreign_network() {
        let t = parse_nft_text(N1_BEFORE);
        assert!(t.chains.contains(&"nv_872ecb86_10_234_0_0_nm24_dnat".to_string()));
        let dnat: Vec<&NftRule> = t.rules.iter().filter(|r| r.chain == "NETAVARK-HOSTPORT-DNAT").collect();
        assert_eq!(dnat.len(), 3);
        assert_eq!(dnat[0], &NftRule { chain: "NETAVARK-HOSTPORT-DNAT".into(), handle: None, dports: vec![47105], jump: Some("nv_872ecb86_10_234_0_0_nm24_dnat".into()) });

        let found = foreign_konak_dnat(&t, &[FORGEJO_NET, DEFAULT_NET]);
        assert_eq!(
            found,
            vec![ForeignNetwork {
                id: "872ecb86".into(),
                ports: vec![47105],
                jumps: vec![("POSTROUTING".into(), None), ("NETAVARK-HOSTPORT-DNAT".into(), None)],
                chains: vec!["nv_872ecb86_10_234_0_0_nm24".into(), "nv_872ecb86_10_234_0_0_nm24_dnat".into()],
            }]
        );
        // The spike's 8000 forward (network ff1513d2, another distro) is outside Konak's ports.
        // Without forgejo's network known, its 47105 forward is foreign too.
        assert_eq!(foreign_konak_dnat(&t, &[DEFAULT_NET]).iter().map(|n| n.id.as_str()).collect::<Vec<_>>(), ["872ecb86", "655c363a"]);
        // After the manual cleanup nothing is left to report.
        assert!(foreign_konak_dnat(&parse_nft_text(N1_AFTER), &[DEFAULT_NET]).is_empty());
    }

    #[test]
    fn reads_handles_from_text_and_json() {
        let text = "table inet netavark { # handle 7\n\tchain NETAVARK-HOSTPORT-DNAT { # handle 4\n\t\tip daddr 127.0.0.1 tcp dport 47105 jump nv_872ecb86_10_234_0_0_nm24_dnat # handle 105\n\t}\n\tchain POSTROUTING { # handle 2\n\t\ttype nat hook postrouting priority srcnat; policy accept;\n\t\tip saddr 10.234.0.0/24 jump nv_872ecb86_10_234_0_0_nm24 # handle 103\n\t}\n\tchain nv_872ecb86_10_234_0_0_nm24 { # handle 50\n\t\tip daddr 10.234.0.0/24 accept # handle 51\n\t}\n\tchain nv_872ecb86_10_234_0_0_nm24_dnat { # handle 52\n\t\tip daddr 127.0.0.1 tcp dport 47105 dnat ip to 10.234.0.2:3000 # handle 53\n\t}\n}\n";
        let json = r#"{"nftables": [{"metainfo": {"version": "1.0.9", "json_schema_version": 1}},
            {"table": {"family": "inet", "name": "netavark", "handle": 7}},
            {"chain": {"family": "inet", "table": "netavark", "name": "NETAVARK-HOSTPORT-DNAT", "handle": 4}},
            {"chain": {"family": "inet", "table": "netavark", "name": "POSTROUTING", "handle": 2, "type": "nat", "hook": "postrouting", "prio": 100, "policy": "accept"}},
            {"chain": {"family": "inet", "table": "netavark", "name": "nv_872ecb86_10_234_0_0_nm24", "handle": 50}},
            {"chain": {"family": "inet", "table": "netavark", "name": "nv_872ecb86_10_234_0_0_nm24_dnat", "handle": 52}},
            {"rule": {"family": "inet", "table": "netavark", "chain": "NETAVARK-HOSTPORT-DNAT", "handle": 105, "expr": [
                {"match": {"op": "==", "left": {"payload": {"protocol": "ip", "field": "daddr"}}, "right": "127.0.0.1"}},
                {"match": {"op": "==", "left": {"payload": {"protocol": "tcp", "field": "dport"}}, "right": 47105}},
                {"jump": {"target": "nv_872ecb86_10_234_0_0_nm24_dnat"}}]}},
            {"rule": {"family": "inet", "table": "netavark", "chain": "POSTROUTING", "handle": 103, "expr": [
                {"match": {"op": "==", "left": {"payload": {"protocol": "ip", "field": "saddr"}}, "right": {"prefix": {"addr": "10.234.0.0", "len": 24}}}},
                {"jump": {"target": "nv_872ecb86_10_234_0_0_nm24"}}]}},
            {"rule": {"family": "inet", "table": "netavark", "chain": "nv_872ecb86_10_234_0_0_nm24", "handle": 51, "expr": [{"accept": null}]}},
            {"rule": {"family": "inet", "table": "netavark", "chain": "nv_872ecb86_10_234_0_0_nm24_dnat", "handle": 53, "expr": [
                {"match": {"op": "==", "left": {"payload": {"protocol": "tcp", "field": "dport"}}, "right": 47105}},
                {"dnat": {"family": "ip", "addr": "10.234.0.2", "port": 3000}}]}}]}"#;
        let from_json = parse_nft_json(json).unwrap();
        assert_eq!(parse_nft_text(text), from_json);
        assert_eq!(parse_nft_json("table inet netavark {"), None);
        let found = foreign_konak_dnat(&from_json, &[DEFAULT_NET]);
        assert_eq!(found[0].jumps, vec![("NETAVARK-HOSTPORT-DNAT".to_string(), Some(105)), ("POSTROUTING".to_string(), Some(103))]);
        assert_eq!(
            foreign_fix(&found[0]),
            "nft delete rule inet netavark NETAVARK-HOSTPORT-DNAT handle 105; nft delete rule inet netavark POSTROUTING handle 103; nft delete chain inet netavark nv_872ecb86_10_234_0_0_nm24; nft delete chain inet netavark nv_872ecb86_10_234_0_0_nm24_dnat"
        );
    }

    #[test]
    fn stale_netavark_reports_a_foreign_network_as_fail() {
        let mut f = facts();
        let probes = "table present\ncontainer forgejo\nprobe forgejo 47105 closed\n";
        f.netavark = Some(format!("{probes}## podman networks\n{FORGEJO_NET}\n{DEFAULT_NET}\n## nft text\n{N1_BEFORE}"));
        let c = stale_netavark(&f);
        assert_eq!(c.status, "fail");
        assert!(c.evidence.contains("network 872ecb86") && c.evidence.contains("47105") && !c.evidence.contains("655c363a"), "{c:?}");
        assert!(c.fix.contains("nft delete chain inet netavark nv_872ecb86_10_234_0_0_nm24_dnat") && !c.fix.contains("655c363a") && !c.fix.contains("delete table"), "{c:?}");
        // Without Konak's network list, nothing is called foreign: back to the closed-port warning.
        f.netavark = Some(format!("{probes}## nft text\n{N1_BEFORE}"));
        assert_eq!(stale_netavark(&f).status, "warn");
        // After the cleanup: ok.
        f.netavark = Some(format!("table present\ncontainer forgejo\nprobe forgejo 47105 open\n## podman networks\n{FORGEJO_NET}\n## nft text\n{N1_AFTER}"));
        assert_eq!(stale_netavark(&f).status, "ok");
    }

    #[test]
    fn port80_ownership_by_pid_or_image() {
        let caddy = r"C:\Konak\bin\caddy.exe";
        let net = |address: &str, pid: u32, image: Option<&str>| NetFacts { port80: vec![Listener { address: address.into(), pid, image: image.map(String::from) }], ..Default::default() };
        let ip = Some("192.168.1.5");
        assert!(konak_owns_port80(&net("192.168.1.5", 22, None), ip, Some(22), caddy));
        assert!(konak_owns_port80(&net("0.0.0.0", 9, Some(r"c:\konak\bin\CADDY.exe")), ip, None, caddy));
        assert!(!konak_owns_port80(&net("192.168.1.5", 4, Some(r"C:\Windows\System32\httpd.exe")), ip, Some(22), caddy));
        assert!(!konak_owns_port80(&net("10.0.0.2", 22, None), ip, Some(22), caddy), "another address");
        assert!(!konak_owns_port80(&net("192.168.1.5", 22, None), None, Some(22), caddy), "no LAN IP");
    }

    #[test]
    fn port80_taken_while_konak_serves_on_the_fallback_is_a_warning() {
        let mut f = facts();
        let httpd = Listener { address: "0.0.0.0".into(), pid: 4242, image: Some(r"C:\xampp\apache\bin\httpd.exe".into()) };
        let net = f.net.as_mut().unwrap();
        net.port80 = vec![httpd.clone()];
        net.fallback_http = vec![Listener { address: "192.168.1.5".into(), pid: 22, image: None }];
        f.heartbeat.as_mut().unwrap().web_ports = WebPorts::FALLBACK;
        let c = port80(&f);
        assert_eq!(c.status, "warn", "{c:?}");
        assert!(c.evidence.contains("httpd.exe") && c.evidence.contains("pid 4242") && c.evidence.contains(":47080"), "{c:?}");
        assert!(c.fix.contains("returns to port 80 by itself"), "{c:?}");
        assert_eq!(port80_owner(f.net.as_ref().unwrap(), "192.168.1.5", &f.caddy).as_deref(), Some("httpd.exe (pid 4242)"));

        // Caddy runs on neither port: still a failure.
        f.net.as_mut().unwrap().fallback_http.clear();
        assert_eq!(port80(&f).status, "fail");
        // A stale heartbeat does not count as serving on the fallback.
        let mut g = facts();
        g.net.as_mut().unwrap().port80 = vec![httpd];
        g.net.as_mut().unwrap().fallback_http = vec![Listener { address: "192.168.1.5".into(), pid: 22, image: None }];
        g.heartbeat.as_mut().unwrap().web_ports = WebPorts::FALLBACK;
        g.heartbeat.as_mut().unwrap().last_tick_unix = 0;
        assert_eq!(port80(&g).status, "fail");

        // Port 80 free again, switch-back pending: a warning that needs nothing.
        let mut h = facts();
        h.net.as_mut().unwrap().port80.clear();
        h.net.as_mut().unwrap().fallback_http = vec![Listener { address: "0.0.0.0".into(), pid: 9, image: Some(r"c:\konak\bin\CADDY.EXE".into()) }];
        h.heartbeat.as_mut().unwrap().web_ports = WebPorts::FALLBACK;
        let c = port80(&h);
        assert!(c.status == "warn" && c.evidence.contains("free again"), "{c:?}");
    }

    #[test]
    fn port80_owner_names_the_program() {
        let caddy = r"C:\Konak\bin\caddy.exe";
        let net = |l: Vec<Listener>| NetFacts { port80: l, ..Default::default() };
        let l = |address: &str, pid: u32, image: Option<&str>| Listener { address: address.into(), pid, image: image.map(String::from) };
        assert_eq!(port80_owner(&net(vec![l("0.0.0.0", 4, None)]), "10.0.0.2", caddy).as_deref(), Some("System (pid 4)"));
        assert_eq!(port80_owner(&net(vec![l("10.0.0.2", 77, None)]), "10.0.0.2", caddy), None, "image unknown");
        assert_eq!(port80_owner(&net(vec![l("10.0.0.9", 77, Some(r"C:\x\iis.exe"))]), "10.0.0.2", caddy), None, "another address");
        assert_eq!(port80_owner(&net(vec![l("10.0.0.2", 22, Some(caddy))]), "10.0.0.2", caddy), None, "Konak's own Caddy");
        assert_eq!(port80_owner(&net(vec![]), "10.0.0.2", caddy), None);
    }

    #[test]
    fn heartbeat_from_before_the_fallback_reads_as_80_and_443() {
        let hb: Heartbeat = serde_json::from_str(r#"{"last_tick_unix":5,"lan_ip":"10.0.0.2","names":["konak"],"pids":{"keepalive":1,"caddy":2,"mdns":3},"https_available":true,"caddy_failures":0}"#).unwrap();
        assert_eq!(hb.web_ports, WebPorts::STANDARD);
        assert!(hb.port80_owner.is_none());
    }

    #[test]
    fn stopped_distro_and_stale_heartbeat() {
        let mut f = facts();
        f.distro.running = false;
        f.forward = None;
        f.netavark = None;
        f.heartbeat.as_mut().unwrap().last_tick_unix = 900;
        assert_eq!(statuses(&f), ["warn", "warn", "fail", "ok", "ok"]);
        assert!(forward_policy(&f).evidence.starts_with("distro not running"));
        assert!(keepalive(&f).evidence.contains("100 s old"));

        let mut f = facts();
        f.heartbeat = None;
        f.lan_ip = Some("10.0.0.9".into());
        f.net.as_mut().unwrap().port80.clear();
        assert_eq!(statuses(&f), ["ok", "ok", "warn", "warn", "ok"]);

        // Caddy keeps exiting: the port check says so.
        let mut f = facts();
        f.net.as_mut().unwrap().port80.clear();
        f.heartbeat.as_mut().unwrap().caddy_failures = 4;
        assert!(port80(&f).evidence.contains("exited 4 times in a row"));

        // A fresh heartbeat on another IP: the supervisor has not followed yet.
        let mut f = facts();
        f.lan_ip = Some("10.0.0.9".into());
        assert_eq!(lan_adapter(&f).status, "warn");
    }
}
