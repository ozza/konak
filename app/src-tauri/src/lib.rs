pub mod apps;
pub mod ca;
pub mod catalog;
pub mod diag;
pub mod installer;
pub mod mcp;
pub mod paths;
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
use std::sync::{Arc, Mutex};
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
}

struct Konak {
    paths: Paths,
    sup: Arc<Supervisor>,
    rt: Mutex<Runtime>,
    /// App ids with an operation in progress; one operation per app at a time.
    busy: Mutex<HashSet<String>>,
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
    summary: Option<Summary>,
}

fn build_status(k: &Konak) -> Status {
    let s = State::load(&k.paths);
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
        error: rt.error.clone(),
        reboot_needed: s.reboot_pending && !rt.running,
        done: s.done,
        url: s.done.then(|| format!("http://{}.local", services::HOME_NAME)),
        network_category: rt.preflight.as_ref().map(|p| p.network_category.clone()),
        summary: s.done.then(|| Summary {
            minutes: (s.finished_unix.saturating_sub(s.started_unix)) as f64 / 60.0,
            uac_prompts: s.uac_prompts,
            reboots: s.reboots,
            download_mb: s.rx_total as f64 / 1_048_576.0,
        }),
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
    let mut state = State::load(&paths);
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

#[tauri::command]
fn restart_pc() {
    shell::run("shutdown.exe", &["/r", "/t", "0"]);
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
        let _ = handle.emit("app-progress", AppProgress { id, op, state, error });
    });
    Ok(())
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

#[tauri::command]
fn restart_app(app: AppHandle, id: String) -> Result<(), String> {
    let id2 = id.clone();
    spawn_op(app, id, "restart", move |p| apps::restart(p, &id2))
}

#[tauri::command]
async fn app_logs(app: AppHandle, id: String, lines: Option<u32>) -> Result<String, String> {
    blocking(app, move |p| apps::logs(p, &id, lines.unwrap_or(200))).await?
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
    if let Some(opts) = uninstall::parse(&std::env::args().collect::<Vec<_>>()) {
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
            let mut state = State::load(&paths);

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
                sup.start();
                // Apps from before the catalog layout are re-rendered once (new port, env).
                if !state.rerender.is_empty() {
                    let (paths, sup, ids) = (paths.clone(), Arc::clone(&sup), state.rerender.clone());
                    std::thread::spawn(move || {
                        for id in ids {
                            if let Err(e) = apps::install(&paths, &id) {
                                log(&paths, &format!("re-render of {id} failed: {e}"));
                            }
                        }
                        sup.tick();
                    });
                }
            }
            app.manage(Konak { paths: paths.clone(), sup, rt: Mutex::new(Runtime::default()), busy: Mutex::new(HashSet::new()) });

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
            restart_pc,
            list_catalog,
            list_apps,
            install_app,
            remove_app,
            rename_app,
            restart_app,
            app_logs,
            ca_info,
            trust_ca_on_this_pc
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
