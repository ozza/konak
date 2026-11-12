mod installer;
mod paths;
mod services;
mod shell;

use installer::{Flow, Preflight, State, STEPS};
use paths::Paths;
use serde::Serialize;
use services::Supervisor;
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
        url: s.app_name.as_ref().filter(|_| s.done).map(|n| format!("http://{n}.local")),
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

extern "system" {
    fn GetUserDefaultUILanguage() -> u16;
}

/// Tray labels follow the Windows display language, from the same locale files as the UI.
fn tray_label(key: &str) -> String {
    let turkish = unsafe { GetUserDefaultUILanguage() } & 0x3ff == 0x1f;
    let v: serde_json::Value = serde_json::from_str(if turkish { LOCALE_TR } else { LOCALE_EN }).unwrap_or_default();
    v["tray"][key].as_str().unwrap_or(key).to_string()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
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
                if let Some(name) = &state.app_name {
                    sup.set_name(name);
                    sup.start();
                }
            }
            app.manage(Konak { paths: paths.clone(), sup, rt: Mutex::new(Runtime::default()) });

            let open = MenuItem::with_id(app, "open", tray_label("open"), true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", tray_label("quit"), true, None::<&str>)?;
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
        .invoke_handler(tauri::generate_handler![get_status, start_install, restart_pc])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
