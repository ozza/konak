//! `konak.exe --mcp`: a headless MCP server on stdio (official Rust SDK `rmcp`) for AI agents doing
//! troubleshooting and app management. Local stdio only; every string in every reply passes
//! through the redactor. Stdout carries protocol messages only: nothing else may print to it.
//!
//! The read-only tools work here directly. The write tools (install, remove, start, stop,
//! restart, rename, claim) never act here: they forward the request over the pipe to the running
//! Konak window (`pipe`), where a person approves or denies each one, and the call waits for that.

use crate::diag::{self, Distro};
use crate::installer::{now, State, STEPS};
use crate::paths::Paths;
use crate::pipe::{self, Op, Outcome, Request};
use crate::redact::Redactor;
use crate::services::{active_ports, lan_ip, HOME_NAME};
use crate::shell::{self, tail};
use crate::{apps, catalog};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use serde::Deserialize;
use serde_json::{json, Value};

pub const DEFAULT_LOG_LINES: u32 = 200;

#[derive(Deserialize, schemars::JsonSchema)]
pub struct LogsArgs {
    /// Installed app: its catalog id (e.g. `jellyfin`) or its `.local` name without the suffix.
    pub app: String,
    /// Lines from the end of the log. Default 200, at most 1000.
    pub lines: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct NameArgs {
    /// A `.local` name without the suffix, e.g. `movies`.
    pub name: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppArgs {
    /// Installed app: its catalog id (e.g. `jellyfin`) or its `.local` name without the suffix.
    pub app: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstallArgs {
    /// Catalog id of an app that is not installed yet: jellyfin, immich, nextcloud, vaultwarden or forgejo.
    pub app: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoveArgs {
    /// Installed app: its catalog id or its `.local` name without the suffix.
    pub app: String,
    /// Also delete the app's data (its database, settings and uploads inside Konak) for good.
    /// Default false: the data is kept and a later install reuses it. Windows media and photo folders
    /// are never deleted.
    pub delete_data: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenameArgs {
    /// Installed app: its catalog id or its current `.local` name without the suffix.
    pub app: String,
    /// New `.local` name without the suffix: lowercase letters, digits and inner hyphens, at most 63.
    pub name: String,
}

/// Forwards a write request to the running Konak and turns its reply into a tool result.
fn forward(paths: &Paths, req: Request) -> Result<Value, String> {
    let reply = pipe::call(&paths.distro, &req)?;
    let what = json!({"op": req.op.as_str(), "app": req.app});
    match reply.outcome {
        Outcome::Done => Ok(json!({"outcome": "done", "request": what})),
        Outcome::Running => Ok(json!({"outcome": "running", "request": what, "note": "approved and still running after 15 minutes; konak_status shows the result"})),
        // Konak says why: not approved, or declined moments ago (the cooldown).
        Outcome::Denied => Err(format!("denied: {}", reply.error.as_deref().unwrap_or("the person did not approve this in the Konak window; nothing was changed"))),
        o => Err(format!("{}: {}", format!("{o:?}").to_lowercase(), reply.error.unwrap_or_default())),
    }
}

pub fn log_lines(lines: Option<u32>) -> u32 {
    lines.unwrap_or(DEFAULT_LOG_LINES).clamp(1, apps::MAX_LOG_LINES)
}

/// The installed app's id for an id or a name.
pub fn resolve_app(state: &State, app: &str) -> Result<String, String> {
    let app = app.trim().trim_end_matches(".local");
    match state.apps.iter().find(|a| a.id == app || a.name == app) {
        Some(a) => Ok(a.id.clone()),
        None if catalog::get(app).is_some() => Err(format!("not_installed: {app}")),
        None => Err(format!("unknown_app: {app}")),
    }
}

/// Masks every string in `v`, keys included.
pub fn redact_value(r: &Redactor, v: Value) -> Value {
    match v {
        Value::String(s) => Value::String(r.apply(&s)),
        Value::Array(a) => Value::Array(a.into_iter().map(|v| redact_value(r, v)).collect()),
        Value::Object(o) => Value::Object(o.into_iter().map(|(k, v)| (r.apply(&k), redact_value(r, v))).collect()),
        other => other,
    }
}

fn heartbeat_json(paths: &Paths) -> Value {
    let now = now();
    match diag::heartbeat(paths) {
        Some(h) => json!({"age_s": h.age(now), "stale": h.stale(now), "lan_ip": h.lan_ip, "names": h.names, "pids": h.pids, "https_available": h.https_available, "web_ports": h.web_ports}),
        None => Value::Null,
    }
}

fn status(paths: &Paths, d: &Distro) -> Value {
    let s = State::load(paths);
    let steps: Vec<Value> = STEPS
        .iter()
        .map(|&id| {
            let state = if s.completed.iter().any(|c| c == id) { "done" } else if s.skipped.iter().any(|c| c == id) { "skipped" } else { "pending" };
            json!({"id": id, "state": state})
        })
        .collect();
    let ports = active_ports(paths);
    let apps = if d.running {
        serde_json::to_value(apps::list(paths)).unwrap_or_default()
    } else {
        s.apps
            .iter()
            .filter_map(|a| catalog::get(&a.id).map(|m| json!({"id": a.id, "name": a.name, "url": apps::url(m, &a.name, ports), "claimed": a.claimed, "status": "distro_not_running"})))
            .collect()
    };
    json!({"setup": {"done": s.done, "steps": steps}, "distro": d, "apps": apps, "heartbeat": heartbeat_json(paths)})
}

fn logs(paths: &Paths, d: &Distro, args: LogsArgs) -> Result<Value, String> {
    let id = resolve_app(&State::load(paths), &args.app)?;
    if !d.running {
        return Err(diag::not_running(d));
    }
    let n = log_lines(args.lines);
    Ok(json!({"app": id, "lines": n, "log": apps::logs(paths, &id, n)?}))
}

fn network(paths: &Paths) -> Result<Value, String> {
    let n = diag::network(paths)?;
    let firewall: Vec<Value> = n.facts.firewall.iter().map(|p| json!({"program": p.program, "private_allow": p.private_allow(), "rules": p.rules})).collect();
    Ok(json!({
        "lan_ip": n.lan_ip,
        "adapter": n.facts.adapter,
        "adapter_virtual": n.facts.adapter.as_ref().map(diag::Adapter::is_virtual),
        "firewall": firewall,
        "port80": {"listeners": n.facts.port80, "konak_caddy_owns": n.konak_owns_port80},
        // Konak moves to the fallback ports while another program holds port 80, and back by itself.
        "web_ports": {
            "http": n.web_ports.http,
            "https": n.web_ports.https,
            "fallback": n.web_ports.is_fallback(),
            "home_url": n.web_ports.url(false, HOME_NAME),
            "port80_owner": n.port80_owner,
            "fallback_http_listeners": n.facts.fallback_http,
        },
    }))
}

fn mdns(paths: &Paths, name: &str) -> Result<Value, String> {
    let name = name.trim().trim_end_matches(".local");
    if !catalog::valid_label(name) {
        return Err(format!("invalid_name: {name:?} (lowercase letters, digits and inner hyphens, at most 63)"));
    }
    let state = State::load(paths);
    let konak_use = if name == HOME_NAME { Some(HOME_NAME.to_string()) } else { state.apps.iter().find(|a| a.name == name).map(|a| a.id.clone()) };
    let announced = diag::heartbeat(paths).is_some_and(|h| !h.stale(now()) && h.names.iter().any(|n| n == name));
    let ip = lan_ip().ok_or("no_lan_ip")?;
    let exe = paths.mdns();
    if !exe.exists() {
        return Err(format!("mDNS responder not found at {}", exe.display()));
    }
    let out = shell::run(&exe.to_string_lossy(), &[&ip, "1", name, "--check-only"]);
    let lan = match out.code {
        Some(0) => "free",
        Some(1) if announced => "answered_by_konak",
        Some(1) => "taken",
        _ => "check_failed",
    };
    Ok(json!({"name": name, "valid": true, "konak_use": konak_use, "announced_by_konak": announced, "lan": lan, "evidence": tail(&out.text, 2)}))
}

fn resources(paths: &Paths, d: &Distro) -> Value {
    let mb = |v: Option<(u64, u64)>| v.map(|(free, total)| json!({"free_mb": free, "total_mb": total}));
    let vhdx = paths.distro_dir().join("ext4.vhdx");
    let vhdx_mb = std::fs::metadata(&vhdx).ok().map(|m| m.len() / 1_048_576);
    let (mem, disk) = diag::distro_usage(paths, d);
    json!({
        "home": {"path": paths.home, "drive": mb(diag::disk_space(&paths.home))},
        "vhdx": {"path": vhdx, "size_mb": vhdx_mb},
        "host_ram": diag::host_ram().map(|(avail, total)| json!({"available_mb": avail, "total_mb": total})),
        "distro": {"name": d.name, "running": d.running, "memory": mem, "root_disk": disk, "note": (!d.running).then(|| diag::not_running(d))},
    })
}

#[derive(Clone)]
pub struct Server {
    paths: Paths,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl Server {
    pub fn new(paths: Paths) -> Self {
        Server { paths, tool_router: Self::tool_router() }
    }

    /// Runs blocking work off the async runtime and redacts the reply. The distro state is read
    /// first so no tool starts a stopped distro (secret values are only read from a running one).
    async fn run(&self, f: impl FnOnce(&Paths, &Distro) -> Result<Value, String> + Send + 'static) -> Result<String, String> {
        let paths = self.paths.clone();
        tokio::task::spawn_blocking(move || {
            let d = diag::distro(&paths);
            let result = f(&paths, &d);
            let r = if d.running { Redactor::load(&paths) } else { Redactor::new(vec![]) };
            match result {
                Ok(v) => Ok(redact_value(&r, v).to_string()),
                Err(e) => Err(r.apply(&e)),
            }
        })
        .await
        .map_err(|e| e.to_string())?
    }

    #[tool(description = "Konak overview: setup steps and whether setup is done, the distro state, installed apps with live status, URL, containers and whether they are claimed (unclaimed apps are reachable only from this PC until their admin account exists, and have no .local name yet), and the supervisor heartbeat age (stale after 60 s).")]
    async fn konak_status(&self) -> Result<String, String> {
        self.run(|p, d| Ok(status(p, d))).await
    }

    #[tool(description = "Last log lines of an installed app's containers (default 200, at most 1000), with secrets redacted.")]
    async fn app_logs(&self, Parameters(args): Parameters<LogsArgs>) -> Result<String, String> {
        self.run(move |p, d| logs(p, d, args)).await
    }

    #[tool(description = "LAN networking: the LAN IP, its adapter and network category (Private/Public), whether Konak's firewall rules exist for caddy.exe and mdns-responder.exe, who owns port 80 on the LAN IP, and the web ports Konak serves on (80/443, or a fallback pair such as 18080/18443 while another program holds port 80).")]
    async fn network_check(&self) -> Result<String, String> {
        self.run(|p, _| network(p)).await
    }

    #[tool(description = "Checks a .local name: valid syntax, used by Konak itself, and whether another device on the LAN already answers to it (mDNS query, about 4 s).")]
    async fn mdns_check(&self, Parameters(args): Parameters<NameArgs>) -> Result<String, String> {
        self.run(move |p, _| mdns(p, &args.name)).await
    }

    #[tool(description = "Disk and memory: free space on the Konak data drive, the distro disk (VHDX) size, host RAM, and memory and root disk usage inside the distro when it is running.")]
    async fn resources(&self) -> Result<String, String> {
        self.run(|p, d| Ok(resources(p, d))).await
    }

    #[tool(description = "Runs the known failure checks and reports ok/warn/fail with evidence and a one-line suggested fix each: shared-namespace FORWARD policy, stale netavark rules, distro keepalive, port 80 owner, LAN adapter choice. Never fixes anything.")]
    async fn diagnose(&self) -> Result<String, String> {
        self.run(|p, _| Ok(json!({"checks": diag::diagnose(&diag::gather(p))}))).await
    }

    /// Sends a write request to the Konak window and waits for the person's answer and the result.
    async fn write(&self, op: Op, app: String, name: Option<String>, delete_data: Option<bool>) -> Result<String, String> {
        let req = Request { op, app, name, delete_data };
        self.run(move |p, _| forward(p, req)).await
    }

    #[tool(description = "Installs an app from Konak's catalog (jellyfin, immich, nextcloud, vaultwarden, forgejo). A person must approve this in the Konak window; the call waits for that answer (up to 5 minutes) and then for the install (up to 15 minutes). New apps stay reachable only from this PC until their admin account exists (see claim_app). Fails if Konak is not running.")]
    async fn install_app(&self, Parameters(args): Parameters<InstallArgs>) -> Result<String, String> {
        self.write(Op::Install, args.app, None, None).await
    }

    #[tool(description = "Removes an installed app's containers. With delete_data true its data inside Konak is deleted for good; by default it is kept. A person must approve this in the Konak window; the call waits for that answer (up to 5 minutes) and then for the removal. Fails if Konak is not running.")]
    async fn remove_app(&self, Parameters(args): Parameters<RemoveArgs>) -> Result<String, String> {
        self.write(Op::Remove, args.app, None, args.delete_data).await
    }

    #[tool(description = "Starts an installed app that was stopped. A person must approve this in the Konak window; the call waits for that answer (up to 5 minutes) and then for the start. Fails if Konak is not running.")]
    async fn start_app(&self, Parameters(args): Parameters<AppArgs>) -> Result<String, String> {
        self.write(Op::Start, args.app, None, None).await
    }

    #[tool(description = "Stops an installed app; its data is kept and it stays stopped until started again. A person must approve this in the Konak window; the call waits for that answer (up to 5 minutes) and then for the stop. Fails if Konak is not running.")]
    async fn stop_app(&self, Parameters(args): Parameters<AppArgs>) -> Result<String, String> {
        self.write(Op::Stop, args.app, None, None).await
    }

    #[tool(description = "Restarts an installed app's containers. A person must approve this in the Konak window; the call waits for that answer (up to 5 minutes) and then for the restart. Fails if Konak is not running.")]
    async fn restart_app(&self, Parameters(args): Parameters<AppArgs>) -> Result<String, String> {
        self.write(Op::Restart, args.app, None, None).await
    }

    #[tool(description = "Changes an installed app's .local name (checked for conflicts on the LAN first). A person must approve this in the Konak window; the call waits for that answer (up to 5 minutes) and then for the change. Fails if Konak is not running.")]
    async fn rename_app(&self, Parameters(args): Parameters<RenameArgs>) -> Result<String, String> {
        self.write(Op::Rename, args.app, Some(args.name), None).await
    }

    #[tool(description = "Marks an installed app as claimed: its admin account exists, so Konak publishes its .local name and makes it reachable from the LAN. Only do this after the admin account was created. A person must approve this in the Konak window; the call waits for that answer (up to 5 minutes) and then for the change. Fails if Konak is not running.")]
    async fn claim_app(&self, Parameters(args): Parameters<AppArgs>) -> Result<String, String> {
        self.write(Op::Claim, args.app, None, None).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("konak", env!("CARGO_PKG_VERSION")))
            .with_instructions("Tools for a Konak home server on this Windows PC. The read-only tools (konak_status, app_logs, network_check, mdns_check, resources, diagnose) never start the Konak distro; when it is stopped they say so. The write tools (install_app, remove_app, start_app, stop_app, restart_app, rename_app, claim_app) need the Konak app running: a person approves or denies each request in the Konak window, and the call waits for that answer (up to 5 minutes) and for the operation (up to 15 minutes). A denied request changes nothing; do not retry it without asking the person.")
    }
}

/// Serves MCP on stdin/stdout until the client closes stdin. Errors go to Konak's log file.
pub fn serve(paths: Paths) {
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => return crate::log(&paths, &format!("mcp: no async runtime: {e}")),
    };
    let server = Server::new(paths.clone());
    rt.block_on(async {
        match server.serve(rmcp::transport::stdio()).await {
            Ok(running) => {
                let _ = running.waiting().await;
            }
            Err(e) => crate::log(&paths, &format!("mcp: {e}")),
        }
    });
    // A pending stdin read cannot be cancelled; do not wait for it.
    rt.shutdown_background();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::installer::InstalledApp;

    #[test]
    fn argument_validation() {
        assert_eq!(log_lines(None), 200);
        assert_eq!(log_lines(Some(0)), 1);
        assert_eq!(log_lines(Some(5000)), 1000);
        let mut s = State::default();
        s.apps.push(InstalledApp { id: "jellyfin".into(), name: "movies".into(), installed_unix: 0, claimed: true, stopped: false });
        assert_eq!(resolve_app(&s, "jellyfin").unwrap(), "jellyfin");
        assert_eq!(resolve_app(&s, "movies.local").unwrap(), "jellyfin");
        assert!(resolve_app(&State::default(), "jellyfin").unwrap_err().starts_with("not_installed"));
        assert!(resolve_app(&s, "nope; rm -rf /").unwrap_err().starts_with("unknown_app"));
    }

    #[test]
    fn write_tools_say_a_person_confirms() {
        let tools = Server::tool_router().list_all();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        for w in ["install_app", "remove_app", "start_app", "stop_app", "restart_app", "rename_app", "claim_app"] {
            let t = tools.iter().find(|t| t.name == w).unwrap_or_else(|| panic!("{w} missing from {names:?}"));
            let d = t.description.as_deref().unwrap_or_default();
            assert!(d.contains("A person must approve this in the Konak window") && d.contains("waits"), "{w}: {d}");
        }
        assert_eq!(tools.len(), 13, "{names:?}");
    }

    /// rmcp deserializes tool arguments with serde before the tool runs, so an unknown argument
    /// fails the call before any pipe request; the schema says so too.
    #[test]
    fn write_tools_reject_unknown_arguments() {
        let parse = |v: Value| serde_json::from_value::<AppArgs>(v).map(|a| a.app);
        assert_eq!(parse(json!({"app": "movies"})).unwrap(), "movies");
        assert!(parse(json!({"app": "movies", "delete_data": true})).unwrap_err().to_string().contains("unknown field"));
        assert!(serde_json::from_value::<InstallArgs>(json!({"app": "immich", "force": true})).is_err());
        assert!(serde_json::from_value::<RemoveArgs>(json!({"app": "x", "delete_data": true, "name": "y"})).is_err());
        assert!(serde_json::from_value::<RenameArgs>(json!({"app": "x", "name": "y", "delete_data": false})).is_err());
        let tools = Server::tool_router().list_all();
        for w in ["install_app", "remove_app", "start_app", "stop_app", "restart_app", "rename_app", "claim_app"] {
            let schema = &tools.iter().find(|t| t.name == w).unwrap().input_schema;
            assert_eq!(schema.get("additionalProperties"), Some(&json!(false)), "{w}: {schema:?}");
            assert_eq!(schema.get("type"), Some(&json!("object")), "{w}");
            assert!(schema["properties"].get("app").is_some(), "{w}: {schema:?}");
        }
    }

    #[test]
    fn write_without_konak_running_says_so() {
        let home = std::env::temp_dir().join("konak-mcp-nopipe");
        let paths = Paths { home: home.clone(), distro: format!("konak-nopipe-{}", std::process::id()), bin: home.join("bin"), media: home.join("m"), photos: home.join("p") };
        let err = forward(&paths, Request { op: Op::Start, app: "jellyfin".into(), name: None, delete_data: None }).unwrap_err();
        assert_eq!(err, pipe::NOT_RUNNING);
    }

    #[test]
    fn replies_are_redacted() {
        let r = Redactor::new(vec!["0123456789abcdef".into()]);
        let v = json!({"log": "db up with 0123456789abcdef\npassword=hunter22", "n": 3, "list": ["token: abc", {"url": "postgres://u:pw@db/x"}]});
        let out = redact_value(&r, v).to_string();
        assert!(!out.contains("0123456789abcdef") && !out.contains("hunter22") && !out.contains("abc\"") && !out.contains(":pw@"), "{out}");
        assert!(out.contains("\"n\":3") && out.contains(crate::redact::MASK));
    }

    #[test]
    fn status_without_distro_does_not_list_containers() {
        let home = std::env::temp_dir().join(format!("konak-mcp-test-{}", std::process::id()));
        let paths = Paths { home: home.clone(), distro: "konak-nonexistent".into(), bin: home.join("bin"), media: home.join("m"), photos: home.join("p") };
        let mut s = State::default();
        s.done = true;
        s.completed = vec!["preflight".into(), "network".into()];
        s.apps.push(InstalledApp { id: "jellyfin".into(), name: "movies".into(), installed_unix: 1, claimed: true, stopped: false });
        s.apps.push(InstalledApp { id: "forgejo".into(), name: "git".into(), installed_unix: 2, claimed: false, stopped: false });
        s.save(&paths);
        let d = Distro { name: paths.distro.clone(), ..Default::default() };
        let v = status(&paths, &d);
        let logs_err = logs(&paths, &d, LogsArgs { app: "jellyfin".into(), lines: None }).unwrap_err();
        let _ = std::fs::remove_dir_all(&home);
        assert!(logs_err.starts_with("distro not running"), "{logs_err}");
        assert_eq!(v["setup"]["done"], true);
        assert_eq!(v["setup"]["steps"][0], json!({"id": "preflight", "state": "done"}));
        assert_eq!(v["setup"]["steps"][1], json!({"id": "admin", "state": "pending"}));
        assert_eq!(v["apps"][0], json!({"id": "jellyfin", "name": "movies", "url": "http://movies.local", "claimed": true, "status": "distro_not_running"}));
        assert_eq!(v["apps"][1]["claimed"], false);
        assert!(v["heartbeat"].is_null());
    }

    #[test]
    fn status_urls_follow_the_fallback_ports() {
        let home = std::env::temp_dir().join(format!("konak-mcp-ports-test-{}", std::process::id()));
        let paths = Paths { home: home.clone(), distro: "konak-nonexistent".into(), bin: home.join("bin"), media: home.join("m"), photos: home.join("p") };
        let mut s = State::default();
        s.apps.push(InstalledApp { id: "vaultwarden".into(), name: "vault".into(), installed_unix: 1, claimed: true, stopped: false });
        s.save(&paths);
        std::fs::write(paths.supervisor_file(), r#"{"last_tick_unix":1,"lan_ip":"10.0.0.2","web_ports":{"http":18080,"https":18443},"port80_owner":"httpd.exe (pid 7)"}"#).unwrap();
        let d = Distro { name: paths.distro.clone(), ..Default::default() };
        let v = status(&paths, &d);
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(v["apps"][0]["url"], "https://vault.local:18443");
        assert_eq!(v["heartbeat"]["web_ports"], json!({"http": 18080, "https": 18443}));
    }
}
