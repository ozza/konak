//! The restore and install scripts run under Git for Windows' bash on a temp folder (no distro).
//! Git's bash cannot make symlinks, device nodes or setuid files, so those cases run only in
//! `extract_refuses_links_devices_and_setuid_in_a_distro`, in a throwaway distro.

use super::*;
use std::process::{Command, Stdio};

/// Git for Windows' bash, never System32's bash.exe (that one is WSL); `None` when Git is not
/// installed. `KONAK_TEST_BASH` overrides the path.
fn git_bash() -> Option<PathBuf> {
    let p = std::env::var_os("KONAK_TEST_BASH").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Program Files\Git\bin\bash.exe"));
    let low = p.to_string_lossy().to_lowercase();
    assert!(!low.contains("system32") && !low.contains("windowsapps"), "{low} is WSL's bash");
    p.is_file().then_some(p)
}

fn slash(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Runs `script` with `args`, `stdin` as its input and `env`; the exit code and the output.
fn run(bash: &Path, dir: &Path, script: &str, args: &[&str], env: &[(&str, String)], stdin: &[u8]) -> (i32, String) {
    let file = dir.join("script.sh");
    std::fs::write(&file, script.replace("\r\n", "\n")).unwrap();
    let mut cmd = Command::new(bash);
    cmd.arg(slash(&file)).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    let data = stdin.to_vec();
    let feeder = std::thread::spawn(move || {
        let _ = input.write_all(&data);
    });
    let out = child.wait_with_output().unwrap();
    feeder.join().unwrap();
    (out.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

enum M<'a> {
    Dir(&'a str),
    File(&'a str, &'a [u8]),
    Fifo(&'a str),
    Link(&'a str, &'a str),
}

fn archive(members: &[M]) -> Vec<u8> {
    let mut b = tar::Builder::new(vec![]);
    for m in members {
        let mut h = tar::Header::new_gnu();
        h.set_mode(0o755);
        let (path, data): (&str, &[u8]) = match m {
            M::Dir(p) => {
                h.set_entry_type(tar::EntryType::Directory);
                (p, b"")
            }
            M::File(p, d) => {
                h.set_entry_type(tar::EntryType::Regular);
                (p, d)
            }
            M::Fifo(p) => {
                h.set_entry_type(tar::EntryType::Fifo);
                (p, b"")
            }
            M::Link(p, target) => {
                h.set_entry_type(tar::EntryType::Link);
                h.set_link_name(target).unwrap();
                (p, b"")
            }
        };
        h.set_size(data.len() as u64);
        b.append_data(&mut h, path, data).unwrap();
    }
    b.into_inner().unwrap()
}

fn temp(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("konak-script-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

/// The environment for app-restore.sh in `base`: its root and run folders, and a `flock` that
/// always gets the lock (Git's bash has none; the lock itself is tested in the distro test).
fn restore_env(base: &Path) -> Vec<(&'static str, String)> {
    let shim = base.join("shim");
    std::fs::create_dir_all(&shim).unwrap();
    std::fs::write(shim.join("flock"), "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::create_dir_all(base.join("run")).unwrap();
    let path = format!("{};{}", shim.display(), std::env::var("PATH").unwrap_or_default());
    vec![("KONAK_ROOT", slash(&base.join("root"))), ("KONAK_RUN_DIR", slash(&base.join("run"))), ("PATH", path)]
}

#[test]
fn extract_keeps_files_and_folders_and_refuses_the_rest() {
    let Some(bash) = git_bash() else {
        eprintln!("Git for Windows' bash not found: skipped");
        return;
    };
    let good: &[M] = &[
        M::Dir("forgejo"),
        M::File("forgejo/compose.yml", b"services: {}\n"),
        M::File("forgejo/.env", b"A=1\n"),
        M::Dir("forgejo/forgejo"),
        M::File("forgejo/forgejo/a", b"data"),
        // A hard link between two files of the folder is fine.
        M::Link("forgejo/forgejo/b", "forgejo/forgejo/a"),
    ];
    let cases: [(&str, &[M], i32, &str); 5] = [
        ("good", good, 0, "extracted"),
        ("fifo", &[M::Dir("forgejo"), M::Fifo("forgejo/pipe")], 4, "something other than files and folders: forgejo/pipe"),
        ("compose-dir", &[M::Dir("forgejo"), M::Dir("forgejo/compose.yml")], 4, "compose.yml is not a regular file"),
        ("bind-file", &[M::Dir("forgejo"), M::File("forgejo/forgejo", b"x")], 4, "forgejo is not a folder"),
        ("two-folders", &[M::Dir("forgejo"), M::Dir("other")], 4, "does not hold just the folder forgejo/"),
    ];
    for (tag, members, want_code, want_text) in cases {
        let base = temp(&format!("extract-{tag}"));
        let root = base.join("root");
        let (code, text) = run(&bash, &base, RESTORE_SH, &["extract", "forgejo", "forgejo"], &restore_env(&base), &archive(members));
        let dir = root.join("apps").join("forgejo");
        let placed = dir.is_dir();
        let b = std::fs::read(dir.join("forgejo").join("b")).ok();
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(code, want_code, "{tag}: {text}");
        assert!(text.contains(want_text), "{tag}: {text}");
        assert_eq!(placed, want_code == 0, "{tag}: the folder is moved into place only when it passes");
        if want_code == 0 {
            assert_eq!(b.as_deref(), Some(&b"data"[..]));
        }
    }
    // Bind source names are checked like volume names.
    let base = temp("extract-badbind");
    let (code, text) = run(&bash, &base, RESTORE_SH, &["extract", "forgejo", "../x"], &restore_env(&base), &[]);
    let _ = std::fs::remove_dir_all(&base);
    assert_eq!(code, 2, "{text}");
}

#[test]
fn install_writes_never_go_through_what_sits_at_compose_yml_and_env() {
    let Some(bash) = git_bash() else {
        eprintln!("Git for Windows' bash not found: skipped");
        return;
    };
    use base64::Engine;
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
    let install_sh = include_str!("../scripts/app-install.sh");
    let base = temp("install-links");
    let apps_dir = base.join("apps");
    let dir = apps_dir.join("forgejo");
    std::fs::create_dir_all(&dir).unwrap();
    // compose.yml and .env are hard links to files outside the app folder (Git's bash cannot make
    // symlinks; a write through either kind of link would change the outside file).
    let (victim, victim_env) = (base.join("victim.conf"), base.join("victim.env"));
    std::fs::write(&victim, b"[interop]\nenabled=false\n").unwrap();
    std::fs::write(&victim_env, b"FORGEJO_SECRET_KEY=kept\n").unwrap();
    std::fs::hard_link(&victim, dir.join("compose.yml")).unwrap();
    std::fs::hard_link(&victim_env, dir.join(".env")).unwrap();
    std::fs::write(dir.join(".env.new"), b"left over").unwrap();
    let env = [("KONAK_APPS_DIR", slash(&apps_dir))];
    let (compose_b64, env_b64) = (b64("services: {}"), b64("KONAK_HOST=git.local"));
    let args = ["forgejo", "47101", "/", &compose_b64, &env_b64, "FORGEJO_SECRET_KEY", "-", "-", "render"];
    let (code, text) = run(&bash, &base, install_sh, &args, &env, &[]);
    let (victim_after, victim_env_after) = (std::fs::read_to_string(&victim).unwrap(), std::fs::read_to_string(&victim_env).unwrap());
    let (compose, dotenv) = (std::fs::read_to_string(dir.join("compose.yml")).unwrap(), std::fs::read_to_string(dir.join(".env")).unwrap());
    let mut names: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
    names.sort();
    // A folder named compose.yml: refused, nothing written into it, no temp file left.
    let dir2 = apps_dir.join("vaultwarden");
    std::fs::create_dir_all(dir2.join("compose.yml")).unwrap();
    let args2 = ["vaultwarden", "47104", "/", &compose_b64, "-", "-", "-", "-", "render"];
    let (code2, text2) = run(&bash, &base, install_sh, &args2, &env, &[]);
    let inside: Vec<_> = std::fs::read_dir(dir2.join("compose.yml")).unwrap().flatten().collect();
    let left2: Vec<String> = std::fs::read_dir(&dir2).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
    let _ = std::fs::remove_dir_all(&base);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("rendered"), "{text}");
    assert_eq!(victim_after, "[interop]\nenabled=false\n", "compose.yml was written through the link");
    assert_eq!(victim_env_after, "FORGEJO_SECRET_KEY=kept\n", ".env was written through the link");
    assert_eq!(compose, "services: {}\n");
    assert_eq!(dotenv, "KONAK_HOST=git.local\nFORGEJO_SECRET_KEY=kept\n");
    assert_eq!(names, [".env", "compose.yml"], "no temp files, no .env.new");
    assert_ne!(code2, 0, "{text2}");
    assert!(inside.is_empty(), "{inside:?}");
    assert_eq!(left2, ["compose.yml"], "{text2}");
}

/// The cases Git's bash cannot make, in a throwaway distro:
/// `KONAK_TEST_DISTRO=konak-<name> cargo test extract_refuses_links -- --ignored`. It works in a
/// temp folder of that distro (`KONAK_ROOT`), never in /var/lib/konak.
#[test]
#[ignore]
fn extract_refuses_links_devices_and_setuid_in_a_distro() {
    let distro = std::env::var("KONAK_TEST_DISTRO").expect("set KONAK_TEST_DISTRO");
    assert!(distro.starts_with("konak-") && distro != "konak", "only a throwaway Konak distro");
    let script = format!("export KONAK_ROOT=/tmp/konak-extract-test KONAK_RUN_DIR=/tmp\nrm -rf /tmp/konak-extract-test\n{RESTORE_SH}");
    let header = |kind: tar::EntryType, mode: u32, size: u64| {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(kind);
        h.set_size(size);
        h.set_mode(mode);
        h
    };
    let sym = |path: &str, target: &str| {
        let mut h = header(tar::EntryType::Symlink, 0o777, 0);
        h.set_link_name(target).unwrap();
        (h, path.to_string())
    };
    let file = |path: &str, mode: u32| (header(tar::EntryType::Regular, mode, 1), path.to_string());
    let dev = |path: &str| {
        let mut h = header(tar::EntryType::Char, 0o666, 0);
        h.set_device_major(1).unwrap();
        h.set_device_minor(3).unwrap();
        (h, path.to_string())
    };
    let top = (header(tar::EntryType::Directory, 0o755, 0), "vaultwarden".to_string());
    type Members = Vec<(tar::Header, String)>;
    let cases: Vec<(&str, Members, bool)> = vec![
        ("bind source to /", vec![top.clone(), sym("vaultwarden/vw-data", "/")], false),
        ("compose.yml to wsl.conf", vec![top.clone(), sym("vaultwarden/compose.yml", "/etc/wsl.conf")], false),
        ("relative link inside", vec![top.clone(), file("vaultwarden/a", 0o644), sym("vaultwarden/b", "a")], false),
        ("char device", vec![top.clone(), dev("vaultwarden/null")], false),
        ("setuid stripped", vec![top.clone(), file("vaultwarden/su", 0o4755)], true),
    ];
    for (what, members, ok) in cases {
        let mut b = tar::Builder::new(vec![]);
        for (mut h, p) in members {
            let body = vec![b'x'; h.size().unwrap() as usize];
            b.append_data(&mut h, p, body.as_slice()).unwrap();
        }
        let data = b.into_inner().unwrap();
        let mut w = shell::DistroWriter::start(&distro, &script, &["extract", "vaultwarden", "vw-data"]).unwrap();
        w.write_all(&data).unwrap();
        let r = w.finish();
        assert_eq!(r.is_ok(), ok, "{what}: {r:?}");
    }
    let out = shell::distro_script(&distro, "stat -c %a /tmp/konak-extract-test/apps/vaultwarden/su; rm -rf /tmp/konak-extract-test\n", &[]);
    assert_eq!(out.text.trim(), "755", "setuid bit stripped");

    // An extract cut off mid-stream on a header boundary: Konak kills wsl.exe, then `stop` ends
    // the Linux side, so nothing is moved into place behind the rollback's back.
    let mut b = tar::Builder::new(vec![]);
    let (mut h, p) = top.clone();
    b.append_data(&mut h, p, &[][..]).unwrap();
    let (mut h, p) = file("vaultwarden/a", 0o644);
    b.append_data(&mut h, p, &b"x"[..]).unwrap();
    let partial = b.into_inner().unwrap();
    let mut w = shell::DistroWriter::start(&distro, &script, &["extract", "vaultwarden", "vw-data"]).unwrap();
    w.write_all(&partial).unwrap();
    std::thread::sleep(std::time::Duration::from_secs(2));
    let _ = w.abort();
    let stop = shell::distro_script(&distro, &script, &["stop", "vaultwarden"]);
    assert!(stop.ok && stop.text.contains("stopped"), "{}", stop.text);
    std::thread::sleep(std::time::Duration::from_secs(2));
    let out = shell::distro_script(&distro, "test -e /tmp/konak-extract-test/apps/vaultwarden && echo placed || echo absent; rm -rf /tmp/konak-extract-test\n", &[]);
    assert_eq!(out.text.trim(), "absent");
}
