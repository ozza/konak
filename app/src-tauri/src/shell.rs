//! Running Windows and WSL commands without flashing console windows.

use std::io::Write;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A command that never opens a console window. `WSL_UTF8` makes wsl.exe print UTF-8 instead of UTF-16.
pub fn hidden(program: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.creation_flags(CREATE_NO_WINDOW).env("WSL_UTF8", "1");
    cmd
}

pub struct Out {
    pub ok: bool,
    pub code: Option<i32>,
    pub text: String,
}

fn decode(bytes: &[u8]) -> String {
    // Some wsl.exe builds ignore WSL_UTF8 for their own messages and print UTF-16LE.
    String::from_utf8_lossy(bytes).replace('\0', "")
}

pub fn run(program: &str, args: &[&str]) -> Out {
    match hidden(program).args(args).output() {
        Ok(o) => Out {
            ok: o.status.success(),
            code: o.status.code(),
            text: format!("{}{}", decode(&o.stdout), decode(&o.stderr)),
        },
        Err(e) => Out { ok: false, code: None, text: format!("cannot start {program}: {e}") },
    }
}

/// Runs a bash script as root inside the distro, fed through stdin.
pub fn distro_script(distro: &str, script: &str, args: &[&str]) -> Out {
    let mut full = vec!["-d", distro, "-u", "root", "--", "bash", "-s", "--"];
    full.extend_from_slice(args);
    let child = hidden("wsl.exe")
        .args(&full)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return Out { ok: false, code: None, text: format!("cannot start wsl.exe: {e}") },
    };
    // Scripts are authored with LF endings; strip any CR that git or an editor may have added.
    let body = script.replace("\r\n", "\n");
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(body.as_bytes());
    }
    match child.wait_with_output() {
        Ok(o) => Out {
            ok: o.status.success(),
            code: o.status.code(),
            text: format!("{}{}", decode(&o.stdout), decode(&o.stderr)),
        },
        Err(e) => Out { ok: false, code: None, text: e.to_string() },
    }
}

pub fn spawn_hidden(program: &str, args: &[&str], log: Option<&std::path::Path>) -> std::io::Result<Child> {
    let mut cmd = hidden(program);
    cmd.args(args).stdin(Stdio::null());
    match log.and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok()) {
        Some(f) => {
            let f2 = f.try_clone()?;
            cmd.stdout(f).stderr(f2);
        }
        None => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    cmd.spawn()
}

/// Last few lines of command output, for error messages.
pub fn tail(text: &str, lines: usize) -> String {
    let v: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    v[v.len().saturating_sub(lines)..].join("\n")
}
