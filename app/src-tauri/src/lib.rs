pub mod apps;
pub mod ca;
pub mod catalog;
pub mod confirm;
pub mod diag;
pub mod installer;
pub mod mcp;
pub mod paths;
pub mod pipe;
pub mod redact;
pub mod services;
pub mod shell;
pub mod uninstall;

use installer::{Flow, Preflight, State, STEPS};
use paths::Paths;
use serde::Serialize;
use services::Supervisor;
use std::collections::HashSet;
use std::io::Write;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, WindowEvent};

const LOCALE_EN: &str = include_str!("../../src/locales/en.json");
const LOCALE_TR: &str = include_str!("../../src/locales/tr.json");

pub fn log(paths: &Paths, msg: &str) {
    let _ = std::fs::create_dir_all(paths.log_dir());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(paths.install_log()) {
        let _ = writeln!(f, "{} {msg}", installer::now());
    }
}

#[derive(Default)]
struct Runtime {
    running: bool,
    current: Option<String>,
    failed: Option<String>,
    error: Option<String>,
    preflight: Option<Preflight>,
    /// Konak's firewall rules are missing (checked when the panel loads); the panel offers a repair.
    firewall_missing: bool,
    repairing: bool,
    repair_error: Option<String>,
}

struct Konak {
    paths: Paths,
    sup: Arc<Supervisor>,
    rt: Mutex<Runtime>,
    /// App ids with an operation in progress; one operation per app at a time.
    busy: Mutex<HashSet<String>>,
    /// The MCP write request waiting for the user's answer, if any.
    confirm: Mutex<confirm::Slot>,
}

#[derive(Serialize, Clone)]
struct StepStatus {
    id: &'static str,
    state: &'static str,
}

#[derive(Serialize, Clone)]
struct Summary {
    minutes: f64,
    uac_prompts: u32,
    reboots: u32,
    download_mb: f64,
}

#[derive(Serialize, Clone)]
struct Status {
    steps: Vec<StepStatus>,
    running: bool,
    error: Option<String>,
    reboot_needed: bool,
    done: bool,
    url: Option<String>,
    network_category: Option<String>,
    firewall_missing: bool,
    repairing: bool,
    repair_error: Option<String>,
    summary: Option<Summary>,
    /// `{http, https, port80_owner}`: 80/443, or 47080/47443 while another program holds port 80
    /// (then `port80_owner` names it when known, e.g. `httpd.exe (pid 1234)`).
    web_ports: services::WebStatus,
}

fn build_status(k: &Konak) -> Status {
    let (s, load_error) = match State::try_load(&k.paths) {
        Ok(s) => (s, None),
        Err(e) => (State::default(), Some(e)),
    };
    let web = k.sup.web_status();
    let rt = k.rt.lock().unwrap();
    let steps = STEPS
        .iter()
        .map(|&id| {
            let state = if s.completed.iter().any(|c| c == id) {
                "done"
            } else if s.skipped.iter().any(|c| c == id) {
                "skipped"
            } else if rt.current.as_deref() == Some(id) && rt.running {
                "running"
            } else if rt.failed.as_deref() == Some(id) {
                "failed"
            } else {
                "pending"
            };
            StepStatus { id, state }
        })
        .collect();
    Status {
        steps,
        running: rt.running,
        error: rt.error.clone().or(load_error),
        reboot_needed: s.reboot_pending && !rt.running,
        done: s.done,
        url: s.done.then(|| web.ports().url(false, services::HOME_NAME)),
        network_category: rt.preflight.as_ref().map(|p| p.network_category.clone()),
        firewall_missing: rt.firewall_missing,
        repairing: rt.repairing,
        repair_error: rt.repair_error.clone(),
        summary: s.done.then(|| Summary {
            minutes: (s.finished_unix.saturating_sub(s.started_unix)) as f64 / 60.0,
            uac_prompts: s.uac_prompts,
            reboots: s.reboots,
            download_mb: s.rx_total as f64 / 1_048_576.0,
        }),
        web_ports: web,
    }
}

fn emit(app: &AppHandle) {
    let k = app.state::<Konak>();
    let _ = app.emit("status", build_status(&k));
}

fn run_all(app: AppHandle, set_private: bool) {
    let k = app.state::<Konak>();
    {
        let mut rt = k.rt.lock().unwrap();
        if rt.running {
            return;
        }
        rt.running = true;
        rt.failed = None;
        rt.error = None;
    }
    emit(&app);
    let paths = k.paths.clone();
    let mut state = match State::try_load(&paths) {
        Ok(s) => s,
        Err(e) => {
            let mut rt = k.rt.lock().unwrap();
            rt.running = false;
            rt.error = Some(e);
            drop(rt);
            emit(&app);
            return;
        }
    };
    if state.started_unix == 0 {
        state.started_unix = installer::now();
        log(&paths, "install started");
    }
    installer::register_autostart();

    let pre = match installer::preflight(&paths) {
        Ok(p) => p,
        Err(e) => {
            let mut rt = k.rt.lock().unwrap();
            rt.running = false;
            rt.failed = Some("preflight".into());
            rt.error = Some(e);
            drop(rt);
            emit(&app);
            return;
        }
    };
    if state.rx_baseline.is_none() {
        state.rx_baseline = Some(pre.rx_bytes);
    }
    k.rt.lock().unwrap().preflight = Some(pre.clone());
    state.save(&paths);

    for step in STEPS {
        if state.finished(step) {
            continue;
        }
        k.rt.lock().unwrap().current = Some(step.into());
        emit(&app);
        let t0 = std::time::Instant::now();
        let result = installer::run_step(step, &paths, &mut state, &pre, set_private, &k.sup);
        log(&paths, &format!("step {step}: {:.1}s", t0.elapsed().as_secs_f64()));
        state.save(&paths);
        // The admin step may have switched the network to Private; the panel's notice follows.
        if step == "admin" && result.is_ok() {
            if let Ok(p) = installer::preflight(&paths) {
                k.rt.lock().unwrap().preflight = Some(p);
            }
        }
        match result {
            Ok(Flow::Next) => emit(&app),
            Ok(Flow::Pause) => break,
            Err(e) => {
                log(&paths, &format!("step {step} failed: {e}"));
                let mut rt = k.rt.lock().unwrap();
                rt.failed = Some(step.into());
                rt.error = Some(e);
                break;
            }
        }
    }

    if STEPS.iter().all(|s| state.finished(s)) && !state.done {
        state.done = true;
        state.finished_unix = installer::now();
        if let (Some(base), Ok(p)) = (state.rx_baseline, installer::preflight(&paths)) {
            state.rx_total += p.rx_bytes - base;
        }
        state.rx_baseline = None;
        state.save(&paths);
        log(&paths, &format!("install done: uac={} reboots={} rx_bytes={}", state.uac_prompts, state.reboots, state.rx_total));
    }
    {
        let mut rt = k.rt.lock().unwrap();
        rt.running = false;
        rt.current = None;
    }
    emit(&app);
}

#[tauri::command]
fn get_status(k: tauri::State<Konak>) -> Status {
    build_status(&k)
}

#[tauri::command]
fn start_install(app: AppHandle, set_private: bool) {
    std::thread::spawn(move || run_all(app, set_private));
}

/// Refused unless the install is waiting for the reboot WSL needs, so nothing else that can invoke
/// commands in the webview can restart the PC.
#[tauri::command]
fn restart_pc(k: tauri::State<Konak>) -> Result<(), String> {
    State::load(&k.paths).may_restart_pc()?;
    shell::run("shutdown.exe", &["/r", "/t", "0"]);
    Ok(())
}

/// Re-reads the network category and, once set up, whether Konak's firewall rules exist. Both are
/// read-only; the panel calls this when it loads, and the result arrives as a `status` event.
#[tauri::command]
async fn refresh_network(app: AppHandle) -> Result<(), String> {
    let handle = app.clone();
    blocking(app, move |p| {
        let pre = installer::preflight(p).ok();
        let missing = State::load(p).done && installer::firewall_missing(p);
        let k = handle.state::<Konak>();
        let mut rt = k.rt.lock().unwrap();
        if pre.is_some() {
            rt.preflight = pre;
        }
        rt.firewall_missing = missing;
        drop(rt);
        emit(&handle);
    })
    .await
}

/// "Repair network access" in the panel: the setup's elevated step again, so one UAC prompt, only
/// ever on this click. Progress and result arrive as `status` events.
#[tauri::command]
fn repair_network(app: AppHandle) {
    let k = app.state::<Konak>();
    {
        let mut rt = k.rt.lock().unwrap();
        if rt.repairing {
            return;
        }
        rt.repairing = true;
        rt.repair_error = None;
    }
    emit(&app);
    std::thread::spawn(move || {
        let k = app.state::<Konak>();
        let result = installer::repair_network(&k.paths);
        let missing = installer::firewall_missing(&k.paths);
        let mut rt = k.rt.lock().unwrap();
        rt.repairing = false;
        rt.firewall_missing = missing;
        rt.repair_error = result.err();
        drop(rt);
        emit(&app);
    });
}

/// Progress of a long app operation, emitted as `app-progress`: `state` is running, done or failed.
#[derive(Serialize, Clone)]
struct AppProgress {
    id: String,
    op: &'static str,
    state: &'static str,
    error: Option<String>,
}

/// Runs an app operation on its own thread and reports through `app-progress` events. Returns
/// `busy` at once when another operation on the same app is still running.
fn spawn_op(app: AppHandle, id: String, op: &'static str, f: impl FnOnce(&Paths) -> Result<(), String> + Send + 'static) -> Result<(), String> {
    spawn_op_notify(app, id, op, None, f)
}

/// `spawn_op`, and `done` receives the result when the operation ends.
fn spawn_op_notify(
    app: AppHandle,
    id: String,
    op: &'static str,
    done: Option<mpsc::Sender<Result<(), String>>>,
    f: impl FnOnce(&Paths) -> Result<(), String> + Send + 'static,
) -> Result<(), String> {
    let k = app.state::<Konak>();
    if !k.busy.lock().unwrap().insert(id.clone()) {
        return Err("busy".into());
    }
    let _ = app.emit("app-progress", AppProgress { id: id.clone(), op, state: "running", error: None });
    let handle = app.clone();
    std::thread::spawn(move || {
        let k = handle.state::<Konak>();
        let result = f(&k.paths);
        k.busy.lock().unwrap().remove(&id);
        // Apply a changed app list to Caddy and mDNS now rather than at the next tick.
        k.sup.tick();
        let (state, error) = match result {
            Ok(()) => ("done", None),
            Err(e) => {
                log(&k.paths, &format!("{op} {id} failed: {e}"));
                ("failed", Some(e))
            }
        };
        if let Some(done) = done {
            let _ = done.send(error.clone().map_or(Ok(()), Err));
        }
        let _ = handle.emit("app-progress", AppProgress { id, op, state, error });
    });
    Ok(())
}

/// The supervisor's re-render for new web ports, run as app operations: every claimed app is busy
/// meanwhile (the panel shows it restarting, op `restart`, and refuses other operations on it), and
/// an operation already running on one of them postpones the whole re-render (the supervisor
/// retries a minute later), so two renders of one app never overlap.
fn rerender_for_ports(app: &AppHandle, ports: services::WebPorts) -> Result<(), String> {
    let k = app.try_state::<Konak>().ok_or("not ready")?;
    let state = State::try_load(&k.paths)?;
    if state.rendered_ports == ports {
        return Ok(());
    }
    let ids: Vec<String> = state.apps.iter().filter(|a| a.claimed).map(|a| a.id.clone()).collect();
    {
        let mut busy = k.busy.lock().unwrap();
        if let Some(id) = ids.iter().find(|id| busy.contains(*id)) {
            return Err(format!("{id} has an operation in progress"));
        }
        busy.extend(ids.iter().cloned());
    }
    let progress = |id: &str, state: &'static str, error: Option<String>| {
        let _ = app.emit("app-progress", AppProgress { id: id.to_string(), op: "restart", state, error });
    };
    for id in &ids {
        progress(id, "running", None);
    }
    let result = apps::rerender_for_ports(&k.paths, ports, &|id, r| {
        k.busy.lock().unwrap().remove(id);
        match r {
            Ok(()) => progress(id, "done", None),
            Err(e) => progress(id, "failed", Some(e.clone())),
        }
    });
    // Apps it skipped (left the catalog) are released too.
    let mut busy = k.busy.lock().unwrap();
    for id in &ids {
        if busy.remove(id) {
            progress(id, "done", None);
        }
    }
    result
}

/// Runs blocking work off the main thread for async commands.
async fn blocking<T: Send + 'static>(app: AppHandle, f: impl FnOnce(&Paths) -> T + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(move || f(&app.state::<Konak>().paths)).await.map_err(|e| e.to_string())
}

#[tauri::command]
fn list_catalog() -> Vec<catalog::Manifest> {
    catalog::all().to_vec()
}

#[tauri::command]
async fn list_apps(app: AppHandle) -> Result<Vec<apps::AppInfo>, String> {
    blocking(app, apps::list).await
}

/// CPU and memory per installed app; empty while the distro is not running (never starts it).
#[tauri::command]
async fn app_stats(app: AppHandle) -> Result<Vec<apps::AppStats>, String> {
    blocking(app, apps::stats).await?
}

#[tauri::command]
fn install_app(app: AppHandle, id: String) -> Result<(), String> {
    catalog::get(&id).ok_or("unknown_app")?;
    let id2 = id.clone();
    spawn_op(app, id, "install", move |p| apps::install(p, &id2).map(|_| ()))
}

#[tauri::command]
fn remove_app(app: AppHandle, id: String, delete_data: bool) -> Result<(), String> {
    let id2 = id.clone();
    spawn_op(app, id, "remove", move |p| apps::remove(p, &id2, delete_data))
}

/// Local name checks answer at once; the LAN check and the re-render run on the thread.
#[tauri::command]
fn rename_app(app: AppHandle, id: String, name: String) -> Result<(), String> {
    apps::validate_name(&State::load(&app.state::<Konak>().paths), &id, &name)?;
    let id2 = id.clone();
    spawn_op(app, id, "rename", move |p| apps::rename(p, &id2, &name))
}

/// The user has created the admin account on this PC: publish the app's `.local` name.
#[tauri::command]
fn claim_app(app: AppHandle, id: String) -> Result<(), String> {
    let id2 = id.clone();
    spawn_op(app, id, "claim", move |p| apps::claim(p, &id2))
}

#[tauri::command]
fn restart_app(app: AppHandle, id: String) -> Result<(), String> {
    let id2 = id.clone();
    spawn_op(app, id, "restart", move |p| apps::restart(p, &id2))
}

#[tauri::command]
fn stop_app(app: AppHandle, id: String) -> Result<(), String> {
    let id2 = id.clone();
    spawn_op(app, id, "stop", move |p| apps::stop(p, &id2))
}

#[tauri::command]
fn start_app(app: AppHandle, id: String) -> Result<(), String> {
    let id2 = id.clone();
    spawn_op(app, id, "start", move |p| apps::start(p, &id2))
}

// ---- MCP write requests over the pipe ------------------------------------------------------
//
// A pipe request reaches an app operation on one path only: `pipe_request` -> `wait_for_answer`
// returns true -> `execute`. `wait_for_answer` returns true only when the receiver from
// `confirm::Slot::open` yields `true`, and the only sender on it is `Slot::answer`, called by the
// `mcp_answer` command with the request's id when the user clicks Approve in the window.

/// How long an approved operation is waited for before the reply says `running`.
const OP_WAIT: Duration = Duration::from_secs(15 * 60);

/// Serves the MCP pipe for the life of the process; without it the MCP write tools report that
/// Konak is not running.
fn serve_pipe(app: AppHandle) {
    let paths = app.state::<Konak>().paths.clone();
    let mut listener = match pipe::Listener::bind(&pipe::name(&paths.distro)) {
        Ok(l) => l,
        Err(e) => return log(&paths, &format!("mcp pipe: {e}; the MCP write tools are off")),
    };
    loop {
        let conn = listener.accept();
        let handle = app.clone();
        std::thread::spawn(move || {
            let reply = pipe_request(&handle, &conn);
            if let Ok(line) = serde_json::to_vec(&reply) {
                let _ = conn.write_line(&line);
            }
            conn.linger(Duration::from_secs(5));
        });
    }
}

fn reply(outcome: pipe::Outcome, error: impl Into<String>) -> pipe::Reply {
    pipe::Reply::new(outcome, Some(error.into()))
}

/// One pipe request: read, validate, ask the user, and run it only if approved.
fn pipe_request(app: &AppHandle, conn: &pipe::Pipe) -> pipe::Reply {
    use pipe::Outcome::{Busy, Denied, Failed, Invalid};
    let k = app.state::<Konak>();
    let line = match conn.read_line(pipe::MAX_REQUEST, Some(Instant::now() + pipe::REQUEST_TIMEOUT)) {
        Ok(l) => l,
        Err(pipe::ReadError::TooLarge) => return reply(Invalid, "request too large"),
        Err(pipe::ReadError::Timeout) => return reply(Invalid, "no request received"),
        Err(pipe::ReadError::Closed) => return reply(Invalid, "connection closed"),
    };
    let req = match pipe::parse(&line) {
        Ok(r) => r,
        Err(e) => return reply(Invalid, e),
    };
    let action = match State::try_load(&k.paths).and_then(|s| pipe::validate(&s, &req)) {
        Ok(a) => a,
        Err(e) => return reply(Invalid, e),
    };
    if k.busy.lock().unwrap().contains(&action.id) {
        return reply(Busy, format!("{} has an operation in progress", action.id));
    }
    let requester = conn.client_pid().and_then(diag::process_image);
    let id = match confirm::new_id() {
        Ok(id) => id,
        Err(e) => return reply(Failed, e),
    };
    let now = installer::now();
    let payload = confirm::Payload {
        id: id.clone(),
        requester: requester.as_deref().and_then(|p| std::path::Path::new(p).file_name()).map(|f| f.to_string_lossy().into_owned()),
        op: action.op.as_str(),
        app: action.id.clone(),
        name: action.name.clone(),
        delete_data: action.delete_data,
        expires_unix: now + confirm::ANSWER_TIMEOUT_S,
    };
    let answer = match k.confirm.lock().unwrap().open(payload.clone(), now) {
        Ok(rx) => rx,
        Err(_) => return reply(Busy, "another request is waiting for an answer in the Konak window"),
    };
    log(&k.paths, &format!("mcp request {id}: {} {} (delete_data={}, name={:?}) from {}", payload.op, payload.app, payload.delete_data, payload.name, requester.as_deref().unwrap_or("unknown")));
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
    let _ = app.emit("mcp-confirm", &payload);

    let approved = wait_for_answer(&k, conn, &id, &answer);
    log(&k.paths, &format!("mcp request {id}: {}", if approved { "approved" } else { "denied" }));
    if !approved {
        let _ = app.emit("mcp-confirm-closed", serde_json::json!({ "id": id }));
        return reply(Denied, "not approved in the Konak window");
    }
    execute(app, &req, &action)
}

/// True only when the user approved request `id` in the window. A denial, no answer within the
/// timeout, and the client hanging up all give false.
fn wait_for_answer(k: &Konak, conn: &pipe::Pipe, id: &str, answer: &mpsc::Receiver<bool>) -> bool {
    let deadline = Instant::now() + Duration::from_secs(confirm::ANSWER_TIMEOUT_S);
    loop {
        match answer.recv_timeout(Duration::from_millis(250)) {
            Ok(approve) => return approve,
            Err(mpsc::RecvTimeoutError::Disconnected) => return false,
            Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline && conn.connected() => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Give up, unless an answer took the request first: it was sent under this lock.
                let mut slot = k.confirm.lock().unwrap();
                if slot.withdraw(id) {
                    return false;
                }
                drop(slot);
                return answer.try_recv().unwrap_or(false);
            }
        }
    }
}

/// Runs an approved action through the panel's operation path and waits for its result. The
/// request is checked again first: the apps may have changed while the dialog was open.
fn execute(app: &AppHandle, req: &pipe::Request, action: &pipe::Action) -> pipe::Reply {
    use pipe::Outcome::{Busy, Done, Failed, Running};
    let k = app.state::<Konak>();
    match State::try_load(&k.paths).and_then(|s| pipe::validate(&s, req)) {
        Ok(current) if current == *action => {}
        Ok(_) => return reply(Failed, "the app changed while the request waited; nothing was done"),
        Err(e) => return reply(Failed, format!("{e}; nothing was done")),
    }
    let (tx, rx) = mpsc::channel();
    if let Err(e) = start_action(app.clone(), action, tx) {
        return if e == "busy" { reply(Busy, format!("{} has an operation in progress", action.id)) } else { reply(Failed, e) };
    }
    match rx.recv_timeout(OP_WAIT) {
        Ok(Ok(())) => pipe::Reply::new(Done, None),
        Ok(Err(e)) => {
            // Secret values are read only from a running distro, as the MCP server does.
            let r = if diag::distro(&k.paths).running { redact::Redactor::load(&k.paths) } else { redact::Redactor::new(vec![]) };
            reply(Failed, r.apply(&e))
        }
        Err(mpsc::RecvTimeoutError::Timeout) => pipe::Reply::new(Running, None),
        Err(mpsc::RecvTimeoutError::Disconnected) => reply(Failed, "the operation ended without a result"),
    }
}

/// Starts an approved action with the same `apps` calls the panel commands make.
fn start_action(app: AppHandle, a: &pipe::Action, done: mpsc::Sender<Result<(), String>>) -> Result<(), String> {
    let (id, id2, op, done) = (a.id.clone(), a.id.clone(), a.op.as_str(), Some(done));
    match a.op {
        pipe::Op::Install => spawn_op_notify(app, id, op, done, move |p| apps::install(p, &id2).map(|_| ())),
        pipe::Op::Remove => {
            let delete_data = a.delete_data;
            spawn_op_notify(app, id, op, done, move |p| apps::remove(p, &id2, delete_data))
        }
        pipe::Op::Rename => {
            let name = a.name.clone().ok_or("rename needs a name")?;
            spawn_op_notify(app, id, op, done, move |p| apps::rename(p, &id2, &name))
        }
        pipe::Op::Claim => spawn_op_notify(app, id, op, done, move |p| apps::claim(p, &id2)),
        pipe::Op::Restart => spawn_op_notify(app, id, op, done, move |p| apps::restart(p, &id2)),
        pipe::Op::Stop => spawn_op_notify(app, id, op, done, move |p| apps::stop(p, &id2)),
        pipe::Op::Start => spawn_op_notify(app, id, op, done, move |p| apps::start(p, &id2)),
    }
}

/// The MCP write request awaiting an answer, for a window that missed the `mcp-confirm` event.
#[tauri::command]
fn mcp_pending(k: tauri::State<Konak>) -> Option<confirm::Payload> {
    k.confirm.lock().unwrap().current(installer::now())
}

/// The user's answer to MCP write request `id`. Only the current request's id is accepted.
#[tauri::command]
fn mcp_answer(k: tauri::State<Konak>, id: String, approve: bool) -> Result<(), String> {
    k.confirm.lock().unwrap().answer(&id, approve, installer::now())
}

#[tauri::command]
async fn app_logs(app: AppHandle, id: String, lines: Option<u32>) -> Result<String, String> {
    blocking(app, move |p| apps::logs(p, &id, lines.unwrap_or(200))).await?
}

/// Distro, memory, disk and network facts for the panel's "This PC" view. Read-only; never starts
/// the distro (memory and disk are null while it is stopped).
#[tauri::command]
async fn host_info(app: AppHandle) -> Result<serde_json::Value, String> {
    let category = app.state::<Konak>().rt.lock().unwrap().preflight.as_ref().map(|p| p.network_category.clone());
    blocking(app, move |p| diag::host_info(p, category)).await
}

/// The MCP `diagnose` checks, redacted the same way. Never fixes anything or starts the distro.
#[tauri::command]
async fn diagnose(app: AppHandle) -> Result<Vec<diag::Check>, String> {
    blocking(app, |p| {
        let facts = diag::gather(p);
        // Secret values are read only from a running distro, as the MCP server does.
        let r = if facts.distro.running { redact::Redactor::load(p) } else { redact::Redactor::new(vec![]) };
        diag::diagnose(&facts).into_iter().map(|c| diag::Check {
                evidence: r.apply(&c.evidence),
                fix: r.apply(&c.fix),
                args: c.args.iter().map(|(k, v)| (*k, r.apply(v))).collect(),
                ..c
            }).collect()
    })
    .await
}

#[derive(Serialize)]
struct CaInfo {
    path: String,
    fingerprint: String,
}

#[tauri::command]
async fn ca_info(app: AppHandle) -> Result<CaInfo, String> {
    blocking(app, |p| {
        ca::ensure(p)?;
        Ok(CaInfo { path: p.ca_cert().to_string_lossy().to_string(), fingerprint: ca::fingerprint(p)? })
    })
    .await?
}

/// Windows shows its own confirmation dialog for this; the call returns when the user answers.
#[tauri::command]
async fn trust_ca_on_this_pc(app: AppHandle) -> Result<(), String> {
    blocking(app, |p| ca::ensure(p).and_then(|_| ca::trust_on_this_pc(p))).await?
}

extern "system" {
    fn GetUserDefaultUILanguage() -> u16;
}

/// Text outside the webview (tray, the konak.local page) follows the Windows display language, from
/// the same locale files as the UI. `key` is dotted, e.g. `tray.open`.
pub fn ui_text(key: &str) -> String {
    let turkish = unsafe { GetUserDefaultUILanguage() } & 0x3ff == 0x1f;
    let lookup = |text: &str| {
        let v: serde_json::Value = serde_json::from_str(text).unwrap_or_default();
        key.split('.').try_fold(&v, |v, part| v.get(part)).and_then(|v| v.as_str()).map(String::from)
    };
    lookup(if turkish { LOCALE_TR } else { LOCALE_EN }).or_else(|| lookup(LOCALE_EN)).unwrap_or_else(|| key.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Headless modes run before the Tauri builder, so the single-instance plugin cannot swallow them.
    let args: Vec<String> = std::env::args().collect();
    if let Some(target) = uninstall::parse_stop(&args) {
        std::process::exit(uninstall::run_stop(target));
    }
    if let Some(opts) = uninstall::parse(&args) {
        std::process::exit(uninstall::run(opts));
    }
    // Headless MCP server on stdio. Resources sit next to the exe, as Tauri's resource_dir has them on Windows.
    if std::env::args().any(|a| a == "--mcp") {
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Into::into)).unwrap_or_default();
        return mcp::serve(Paths::new(exe_dir));
    }
    let autostart = std::env::args().any(|a| a == "--autostart");
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .setup(move |app| {
            let paths = Paths::new(app.path().resource_dir()?);
            let sup = Supervisor::new(paths.clone());
            // Port switches update the open window; the apps' settings follow as app operations.
            let handle = app.handle().clone();
            sup.on_change(move || {
                if handle.try_state::<Konak>().is_some() {
                    emit(&handle);
                }
            });
            let handle = app.handle().clone();
            sup.set_rerender(move |_, ports| rerender_for_ports(&handle, ports));
            // A damaged state.json is left alone: nothing starts, and the window shows the error.
            let mut state = State::try_load(&paths).unwrap_or_default();

            // Back from the reboot that WSL needed: count it and continue the install on our own.
            let mut resume = false;
            if state.reboot_pending && installer::boot_time().abs_diff(state.boot_time) > 120 {
                state.reboot_pending = false;
                state.reboots += 1;
                state.completed.push("reboot".into());
                state.save(&paths);
                log(&paths, "resumed after reboot");
                resume = true;
            }
            if state.done {
                // An uninstall that kept the data removed the Run value; put it back (not in dev runs).
                installer::register_autostart();
                let (paths, sup) = (paths.clone(), Arc::clone(&sup));
                std::thread::spawn(move || {
                    // A Caddy left from an earlier run would keep port 80 and serve its old config.
                    match uninstall::stop_leftovers(&paths) {
                        Ok(s) if s == "nothing running" => {}
                        r => log(&paths, &format!("leftover processes: {r:?}")),
                    }
                    sup.start();
                });
            }
            app.manage(Konak {
                paths: paths.clone(),
                sup,
                rt: Mutex::new(Runtime::default()),
                busy: Mutex::new(HashSet::new()),
                confirm: Mutex::new(confirm::Slot::default()),
            });
            // MCP write requests; each one waits for the user's answer in the window.
            let handle = app.handle().clone();
            std::thread::spawn(move || serve_pipe(handle));
            // Apps from before the catalog layout are re-rendered once (new port, env), as ordinary
            // operations: the panel sees them as busy and cannot start another one on the same app.
            if state.done {
                for id in state.rerender.clone() {
                    let id2 = id.clone();
                    if let Err(e) = spawn_op(app.handle().clone(), id.clone(), "install", move |p| apps::install(p, &id2).map(|_| ())) {
                        log(&paths, &format!("re-render of {id} not started: {e}"));
                    }
                }
            }

            let open = MenuItem::with_id(app, "open", ui_text("tray.open"), true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", ui_text("tray.quit"), true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &quit])?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Konak")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "quit" => {
                        app.state::<Konak>().sup.stop_all();
                        app.exit(0);
                    }
                    _ => {}
                })
                .build(app)?;

            if !(autostart && state.done) {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                }
            }
            let handle = app.handle().clone();
            let dev_auto = std::env::var("KONAK_DEV_AUTO_INSTALL").is_ok_and(|v| v == "1");
            if resume || (dev_auto && !state.done) {
                std::thread::spawn(move || run_all(handle, false));
            } else if !state.done {
                // Network profile for the Public-network notice, before the user clicks install.
                std::thread::spawn(move || {
                    let k = handle.state::<Konak>();
                    if let Ok(p) = installer::preflight(&k.paths) {
                        k.rt.lock().unwrap().preflight = Some(p);
                    }
                    emit(&handle);
                });
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window keeps Konak running in the tray; "Quit" in the tray menu stops it.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            start_install,
            refresh_network,
            repair_network,
            restart_pc,
            list_catalog,
            list_apps,
            app_stats,
            install_app,
            remove_app,
            rename_app,
            claim_app,
            restart_app,
            stop_app,
            start_app,
            mcp_pending,
            mcp_answer,
            app_logs,
            host_info,
            diagnose,
            ca_info,
            trust_ca_on_this_pc
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
