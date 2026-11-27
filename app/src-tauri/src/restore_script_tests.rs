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

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect()).unwrap_or_default();
    v.sort();
    v
}

/// `restore_env` plus fakes of `podman` (volumes are the folders in `<base>/vols`, all labelled
/// with the app's project; `FAKE_FAIL` names one whose inspect fails) and `docker-compose` (logs its
/// arguments and `COMPOSE_FILE`); both log to `<base>/fake.log`.
fn fake_env(base: &Path, fail: &str) -> Vec<(&'static str, String)> {
    let shim = base.join("shim");
    std::fs::create_dir_all(base.join("vols")).unwrap();
    let mut env = restore_env(base);
    std::fs::write(
        shim.join("podman"),
        "#!/bin/bash\ncase \"$1 $2\" in\n  'volume ls') ls -1 \"$FAKE_VOLS\" ;;\n  'volume inspect') [ \"$3\" != \"$FAKE_FAIL\" ] || exit 1; echo \"$FAKE_VOLS/$3\" ;;\n  'volume rm') rm -rf \"$FAKE_VOLS/$4\"; echo \"volume rm $4\" >> \"$FAKE_LOG\" ;;\n  'volume exists') [ -d \"$FAKE_VOLS/$3\" ] ;;\nesac\n",
    )
    .unwrap();
    std::fs::write(shim.join("docker-compose"), "#!/bin/bash\necho \"docker-compose $* (COMPOSE_FILE=${COMPOSE_FILE-unset})\" >> \"$FAKE_LOG\"\n").unwrap();
    env.extend([("FAKE_VOLS", slash(&base.join("vols"))), ("FAKE_LOG", slash(&base.join("fake.log"))), ("FAKE_FAIL", fail.to_string()), ("COMPOSE_FILE", "compose.override.yml".to_string())]);
    env
}

fn write_tree(root: &Path, files: &[(&str, &str)]) {
    for (rel, body) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
}

/// Every file under `dir` with its content, `/`-separated.
fn tree(dir: &Path) -> Vec<(String, String)> {
    let mut out = vec![];
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push((slash(p.strip_prefix(dir).unwrap()), std::fs::read_to_string(&p).unwrap_or_default()));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn a_rollback_after_an_interrupted_aside_keeps_every_volume_it_had_not_moved() {
    let Some(bash) = git_bash() else {
        eprintln!("Git for Windows' bash not found: skipped");
        return;
    };
    let base = temp("aside-cut");
    let env = fake_env(&base, "nextcloud_b");
    let (vols, app) = (base.join("vols"), base.join("root").join("apps").join("nextcloud"));
    let before = [("nextcloud_a/a.txt", "a"), ("nextcloud_b/files/user.txt", "user files"), ("nextcloud_c/c.txt", "c")];
    write_tree(&vols, &before);
    write_tree(&app, &[("compose.yml", "services: {}"), (".env", "A=1"), ("compose.override.yml", "hostile")]);
    // `aside` stops at the second volume (its inspect fails), after it moved the first one.
    let (aside_code, aside_text) = run(&bash, &base, RESTORE_SH, &["aside", "nextcloud"], &env, &[]);
    let vaside = base.join("root").join("apps").join("nextcloud.before-restore-volumes");
    let record = std::fs::read_to_string(vaside.join(".volumes")).unwrap_or_default();
    let (code, text) = run(&bash, &base, RESTORE_SH, &["rollback", "nextcloud", "1"], &env, &[]);
    let after = tree(&vols);
    let app_back = names(&app);
    let log = std::fs::read_to_string(base.join("fake.log")).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&base);
    assert_ne!(aside_code, 0, "{aside_text}");
    assert_eq!(record, "nextcloud_a\nnextcloud_b\nnextcloud_c\n", "recorded before anything moved");
    assert_eq!(code, 0, "{text}");
    let want: Vec<(String, String)> = before.iter().map(|(p, b)| (p.to_string(), b.to_string())).collect();
    assert_eq!(after, want, "no volume removed, the moved one put back");
    assert_eq!(app_back, [".env", "compose.override.yml", "compose.yml"]);
    assert!(!log.contains("volume rm"), "{log}");
    // Only the catalog's file, whatever the folder or the environment holds.
    assert!(log.contains("docker-compose -f compose.yml -p nextcloud down") && log.contains("COMPOSE_FILE=unset") && !log.contains("COMPOSE_FILE=compose"), "{log}");
}

#[test]
fn a_rollback_removes_only_volumes_the_restore_made_and_refuses_without_the_record() {
    let Some(bash) = git_bash() else {
        eprintln!("Git for Windows' bash not found: skipped");
        return;
    };
    let base = temp("aside-full");
    let env = fake_env(&base, "-");
    let vols = base.join("vols");
    write_tree(&vols, &[("nextcloud_db/pg.txt", "old db"), ("nextcloud_nextcloud/f.txt", "old files")]);
    let (code, text) = run(&bash, &base, RESTORE_SH, &["aside", "nextcloud"], &env, &[]);
    assert_eq!(code, 0, "{text}");
    // The restore imports into the recorded volumes and makes one the app did not have.
    write_tree(&vols, &[("nextcloud_db/new.txt", "restored db"), ("nextcloud_extra/x.txt", "restored")]);
    let (code, text) = run(&bash, &base, RESTORE_SH, &["rollback", "nextcloud", "1"], &env, &[]);
    let after = tree(&vols);
    let apps_left = names(&base.join("root").join("apps"));
    // A set-aside folder without the record (an older Konak's): nothing is changed.
    let vaside = base.join("root").join("apps").join("nextcloud.before-restore-volumes");
    write_tree(&vaside, &[("nextcloud_db/pg.txt", "old db")]);
    write_tree(&vols, &[("nextcloud_new/n.txt", "restored")]);
    let (code2, text2) = run(&bash, &base, RESTORE_SH, &["rollback", "nextcloud", "1"], &env, &[]);
    let after2 = tree(&vols);
    let kept = vaside.join("nextcloud_db").join("pg.txt").is_file();
    let _ = std::fs::remove_dir_all(&base);
    assert_eq!(code, 0, "{text}");
    let s = |v: &[(&str, &str)]| v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect::<Vec<_>>();
    assert_eq!(after, s(&[("nextcloud_db/pg.txt", "old db"), ("nextcloud_nextcloud/f.txt", "old files")]));
    assert!(apps_left.is_empty(), "{apps_left:?}");
    assert_eq!(code2, 7, "{text2}");
    assert!(text2.contains("no record of the app's volumes"), "{text2}");
    assert!(after2.iter().any(|(p, _)| p == "nextcloud_new/n.txt") && kept);
}

#[test]
fn a_rollback_of_a_half_moved_volume_never_empties_it() {
    let Some(bash) = git_bash() else {
        eprintln!("Git for Windows' bash not found: skipped");
        return;
    };
    // What a power cut in the middle of `aside`'s move leaves: part of the volume set aside, the
    // rest still in it, no `.complete`.
    let base = temp("aside-half");
    let env = fake_env(&base, "-");
    let vols = base.join("vols");
    let vaside = base.join("root").join("apps").join("nextcloud.before-restore-volumes");
    write_tree(&vols, &[("nextcloud_nextcloud/b.txt", "not moved yet")]);
    write_tree(&vaside, &[(".volumes", "nextcloud_nextcloud\n"), (".no-folder", ""), ("nextcloud_nextcloud/a.txt", "moved")]);
    let (code, text) = run(&bash, &base, RESTORE_SH, &["rollback", "nextcloud", "1"], &env, &[]);
    let after = tree(&vols);
    let _ = std::fs::remove_dir_all(&base);
    assert_eq!(code, 0, "{text}");
    assert_eq!(after, [("nextcloud_nextcloud/a.txt".to_string(), "moved".to_string()), ("nextcloud_nextcloud/b.txt".to_string(), "not moved yet".to_string())]);
}

/// Every docker-compose call Konak makes names compose.yml (`-f`), with `COMPOSE_FILE` unset:
/// without `-f`, docker-compose prefers a compose.yaml and merges a compose.override.yml.
#[test]
fn every_compose_call_names_the_rendered_compose_yml() {
    let scripts = [
        ("app-install.sh", include_str!("../scripts/app-install.sh").to_string()),
        ("app-remove.sh", include_str!("../scripts/app-remove.sh").to_string()),
        ("app-restore.sh", RESTORE_SH.to_string()),
        ("apps::compose", crate::apps::compose_script("logs")),
    ];
    for (name, text) in &scripts {
        let calls: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.starts_with('#') && l.contains("docker-compose ")).collect();
        assert!(!calls.is_empty(), "{name}");
        for c in &calls {
            assert!(c.contains("docker-compose -f compose.yml -p ") || c.starts_with("(cd / && docker-compose -p "), "{name}: {c}");
        }
        let unset = text.find("unset COMPOSE_FILE COMPOSE_PATH_SEPARATOR").unwrap_or(usize::MAX);
        assert!(unset < text.find(calls[0]).unwrap(), "{name}: COMPOSE_FILE unset first");
    }
}

/// The extract rules Git's bash cannot exercise (symlinks, devices, setuid, xattrs), pinned as text;
/// `extract_refuses_links_devices_and_setuid_in_a_distro` runs them.
#[test]
fn the_extract_rules_stay_in_the_script() {
    for rule in [
        "tar -C \"$staging\" --numeric-owner --xattrs --xattrs-include='user.*' --acls -xpf -",
        "bad=$(find \"$staging/$id\" \\( -type l -o ! \\( -type f -o -type d \\) \\) -print -quit </dev/null)",
        "bad=$(find \"$staging/$id\" -type f -links +1 -printf '%i %n\\n' </dev/null | sort | uniq -c | awk '$1 != $3 { print $2; exit }')",
        "find \"$staging/$id\" -type f -perm /6000 -exec chmod ug-s {} + </dev/null",
        "[ ! -e \"$dir\" ] || { echo \"$dir exists\" >&2; exit 3; }",
    ] {
        assert!(RESTORE_SH.contains(rule), "{rule}");
    }
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
        // What an interrupted install leaves: dropped with the archive's compose.yml.
        M::File("forgejo/.env.new", b"B=2\n"),
        M::File("forgejo/.compose.yml.Ab12Cd", b"services: {}\n"),
        M::File("forgejo/.env.Xy34Zw", b"C=3\n"),
        M::Dir("forgejo/forgejo"),
        M::File("forgejo/forgejo/a", b"data"),
        // A hard link between two files of the folder is fine.
        M::Link("forgejo/forgejo/b", "forgejo/forgejo/a"),
        // Below a bind source anything goes (the app's own data).
        M::File("forgejo/forgejo/compose.override.yml", b"x"),
    ];
    let hostile = |name: &'static str| -> Vec<M<'static>> { vec![M::Dir("forgejo"), M::File("forgejo/compose.yml", b"services: {}\n"), M::File(name, b"services: { app: { privileged: true } }\n")] };
    let (override_yml, compose_yaml, docker_compose, other) = (hostile("forgejo/compose.override.yml"), hostile("forgejo/compose.yaml"), hostile("forgejo/docker-compose.yml"), hostile("forgejo/notes.txt"));
    let cases: [(&str, &[M], i32, &str); 9] = [
        ("good", good, 0, "extracted"),
        ("fifo", &[M::Dir("forgejo"), M::Fifo("forgejo/pipe")], 4, "something other than files and folders: forgejo/pipe"),
        ("compose-dir", &[M::Dir("forgejo"), M::Dir("forgejo/compose.yml")], 4, "compose.yml is not a regular file"),
        ("bind-file", &[M::Dir("forgejo"), M::File("forgejo/forgejo", b"x")], 4, "forgejo is not a folder"),
        ("two-folders", &[M::Dir("forgejo"), M::Dir("other")], 4, "does not hold just the folder forgejo/"),
        // docker-compose would merge these on top of, or take them over, the catalog's compose.yml.
        ("override", &override_yml, 4, "holds compose.override.yml, which is not part of a Konak app folder"),
        ("compose-yaml", &compose_yaml, 4, "holds compose.yaml, which is not part of a Konak app folder"),
        ("docker-compose", &docker_compose, 4, "holds docker-compose.yml, which is not part of a Konak app folder"),
        ("other-name", &other, 4, "holds notes.txt, which is not part of a Konak app folder"),
    ];
    for (tag, members, want_code, want_text) in cases {
        let base = temp(&format!("extract-{tag}"));
        let root = base.join("root");
        let (code, text) = run(&bash, &base, RESTORE_SH, &["extract", "forgejo", "forgejo"], &restore_env(&base), &archive(members));
        let dir = root.join("apps").join("forgejo");
        let placed = dir.is_dir();
        let b = std::fs::read(dir.join("forgejo").join("b")).ok();
        let top = if placed { names(&dir) } else { vec![] };
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(code, want_code, "{tag}: {text}");
        assert!(text.contains(want_text), "{tag}: {text}");
        assert_eq!(placed, want_code == 0, "{tag}: the folder is moved into place only when it passes");
        if want_code == 0 {
            assert_eq!(b.as_deref(), Some(&b"data"[..]));
            assert_eq!(top, [".env", "forgejo"], "the archive's compose.yml and install leftovers are dropped");
        }
    }
    // A hard link to a file outside the folder is never placed (GNU tar refuses such targets).
    for (tag, target) in [("link-up", "forgejo/../../outside/victim"), ("link-abs", "/outside/victim")] {
        let base = temp(&format!("extract-{tag}"));
        std::fs::create_dir_all(base.join("outside")).unwrap();
        std::fs::write(base.join("outside").join("victim"), b"victim").unwrap();
        let members = [M::Dir("forgejo"), M::Dir("forgejo/forgejo"), M::Link("forgejo/forgejo/b", target)];
        let (code, text) = run(&bash, &base, RESTORE_SH, &["extract", "forgejo", "forgejo"], &restore_env(&base), &archive(&members));
        let placed = base.join("root").join("apps").join("forgejo").exists();
        let _ = std::fs::remove_dir_all(&base);
        assert_ne!(code, 0, "{tag}: {text}");
        assert!(!placed, "{tag}: {text}");
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
        // Inside a bind source: a file at the top of the folder would be refused for its name.
        ("setuid stripped", vec![top.clone(), (header(tar::EntryType::Directory, 0o755, 0), "vaultwarden/vw-data".to_string()), file("vaultwarden/vw-data/su", 0o4755)], true),
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
    let out = shell::distro_script(&distro, "stat -c %a /tmp/konak-extract-test/apps/vaultwarden/vw-data/su; rm -rf /tmp/konak-extract-test\n", &[]);
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
