//! Local channel from `konak.exe --mcp` to the running Konak window, for the MCP write tools. One
//! JSON request line, one JSON reply line per connection.
//!
//! Security: the pipe's DACL allows only the current user's SID (`D:P(A;;GRGW;;;<sid>)`), remote
//! clients are rejected, and the first instance is created with `FILE_FLAG_FIRST_PIPE_INSTANCE`, so
//! a process that took the name first makes Konak run without the pipe instead of sharing it. The
//! client opens the pipe with identification-level impersonation only and checks, before writing,
//! that the server process runs this same konak.exe file.
//!
//! Synchronous Win32 calls (windows-sys) rather than tokio's named pipes: the GUI process has no
//! tokio runtime of its own to drive, the API takes raw security attributes directly, and one
//! thread per connection (at most `MAX_INSTANCES`) is plenty for one request at a time.

use crate::installer::State;
use crate::{apps, catalog, mcp};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetFileInformationByHandle, ReadFile, WriteFile, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, GetNamedPipeServerProcessId, PeekNamedPipe, WaitNamedPipeW, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Largest request line, newline excluded.
pub const MAX_REQUEST: usize = 4096;
const MAX_REPLY: usize = 16 * 1024;
const MAX_INSTANCES: u32 = 4;
/// A client gets this long to send its request line.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

// ---- Protocol -------------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Install,
    Remove,
    Start,
    Stop,
    Restart,
    Rename,
    Claim,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Install => "install",
            Op::Remove => "remove",
            Op::Start => "start",
            Op::Stop => "stop",
            Op::Restart => "restart",
            Op::Rename => "rename",
            Op::Claim => "claim",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub op: Op,
    /// Catalog id (install), or an installed app's id or `.local` name.
    pub app: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_data: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Done,
    Failed,
    Denied,
    Busy,
    Invalid,
    /// Still in progress when the wait cap was reached.
    Running,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Reply {
    pub fn new(outcome: Outcome, error: Option<String>) -> Self {
        Reply { outcome, error }
    }
}

/// A request that passed validation: what the dialog shows, and what runs after approval.
#[derive(Clone, Debug, PartialEq)]
pub struct Action {
    pub op: Op,
    /// Catalog id.
    pub id: String,
    pub name: Option<String>,
    pub delete_data: bool,
}

/// The pipe for a distro, so isolated dev runs (`KONAK_DISTRO`) do not collide. Characters a pipe
/// name cannot carry become `_`.
pub fn name(distro: &str) -> String {
    let safe: String = distro.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect();
    format!(r"\\.\pipe\konak-{safe}")
}

pub fn parse(line: &[u8]) -> Result<Request, String> {
    if line.len() > MAX_REQUEST {
        return Err("request too large".into());
    }
    serde_json::from_slice(line).map_err(|e| format!("bad request: {e}"))
}

/// Checks a request against the saved state: the app the op needs, the fields the op takes, and the
/// name rules for rename. Runs before any dialog, and again right before an approved op runs.
pub fn validate(state: &State, req: &Request) -> Result<Action, String> {
    if !state.done {
        return Err("setup_not_done: finish Konak's setup first".into());
    }
    if req.delete_data.is_some() && req.op != Op::Remove {
        return Err("delete_data is only for remove".into());
    }
    match (req.op, &req.name) {
        (Op::Rename, None) => return Err("rename needs a name".into()),
        (op, Some(_)) if op != Op::Rename => return Err("name is only for rename".into()),
        _ => {}
    }
    let id = if req.op == Op::Install {
        let app = req.app.trim();
        catalog::get(app).ok_or_else(|| format!("unknown_app: {app}"))?;
        if state.apps.iter().any(|a| a.id == app) {
            return Err(format!("already_installed: {app}"));
        }
        app.to_string()
    } else {
        mcp::resolve_app(state, &req.app)?
    };
    if let Some(name) = &req.name {
        apps::validate_name(state, &id, name)?;
    }
    Ok(Action { op: req.op, id, name: req.name.clone(), delete_data: req.delete_data.unwrap_or(false) })
}

// ---- Security descriptor ------------------------------------------------------------------

/// Read and write for exactly this SID; `P` blocks inherited entries. Nothing else gets access.
pub fn sddl(sid: &str) -> String {
    format!("D:P(A;;GRGW;;;{sid})")
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Reads a NUL-terminated wide string the system allocated, then frees it.
unsafe fn take_local_wide(p: *mut u16) -> String {
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
    LocalFree(p.cast());
    s
}

fn last_error() -> u32 {
    unsafe { GetLastError() }
}

/// The SID of the user this process runs as, from its token, e.g. `S-1-5-21-...`.
pub fn current_user_sid() -> Result<String, String> {
    // SAFETY: the token handle is ours and closed; the buffer is sized by the first call and
    // aligned for TOKEN_USER (u64 storage).
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(format!("OpenProcessToken failed ({})", last_error()));
        }
        let mut len = 0u32;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
        let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
        let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &mut len) != 0;
        let err = last_error();
        CloseHandle(token);
        if !ok {
            return Err(format!("GetTokenInformation failed ({err})"));
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut s) == 0 {
            return Err(format!("ConvertSidToStringSid failed ({})", last_error()));
        }
        Ok(take_local_wide(s))
    }
}

/// A security descriptor built from SDDL by the system; freed on drop.
pub struct Descriptor(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is immutable once built and only read by the system.
unsafe impl Send for Descriptor {}

impl Descriptor {
    pub fn from_sddl(sddl: &str) -> Result<Self, String> {
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: the string is NUL-terminated; the system allocates `sd`, freed in Drop.
        if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(wide(sddl).as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) } == 0 {
            return Err(format!("bad security descriptor ({})", last_error()));
        }
        Ok(Descriptor(sd))
    }

    /// The pipe's descriptor: the current user only.
    pub fn current_user_only() -> Result<Self, String> {
        Self::from_sddl(&sddl(&current_user_sid()?))
    }

    /// The DACL as SDDL, as the system reads it back.
    pub fn dacl_sddl(&self) -> Result<String, String> {
        dacl_to_sddl(self.0)
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

fn dacl_to_sddl(sd: PSECURITY_DESCRIPTOR) -> Result<String, String> {
    let mut s: *mut u16 = std::ptr::null_mut();
    // SAFETY: `sd` is a valid descriptor; the system allocates `s`, freed by take_local_wide.
    unsafe {
        if ConvertSecurityDescriptorToStringSecurityDescriptorW(sd, SDDL_REVISION_1, DACL_SECURITY_INFORMATION, &mut s, std::ptr::null_mut()) == 0 {
            return Err(format!("ConvertSecurityDescriptorToStringSecurityDescriptor failed ({})", last_error()));
        }
        Ok(take_local_wide(s))
    }
}

// ---- Pipe handles -------------------------------------------------------------------------

/// One end of a pipe connection; the handle closes on drop.
pub struct Pipe(HANDLE);

// SAFETY: a pipe handle may be used from any thread; each Pipe is used by one thread at a time.
unsafe impl Send for Pipe {}

impl Drop for Pipe {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

pub enum ReadError {
    Closed,
    TooLarge,
    Timeout,
}

impl Pipe {
    /// Bytes waiting to be read; `None` once the other end has closed.
    fn available(&self) -> Option<u32> {
        let mut avail = 0u32;
        // SAFETY: a valid handle; no buffer is passed.
        let ok = unsafe { PeekNamedPipe(self.0, std::ptr::null_mut(), 0, std::ptr::null_mut(), &mut avail, std::ptr::null_mut()) } != 0;
        ok.then_some(avail)
    }

    /// False once the other end has disconnected.
    pub fn connected(&self) -> bool {
        self.available().is_some()
    }

    fn read_some(&self, buf: &mut [u8]) -> Option<usize> {
        let mut n = 0u32;
        // SAFETY: the buffer length is passed in.
        let ok = unsafe { ReadFile(self.0, buf.as_mut_ptr(), buf.len() as u32, &mut n, std::ptr::null_mut()) } != 0;
        (ok && n > 0).then_some(n as usize)
    }

    /// One line (newline excluded) of at most `cap` bytes. With a deadline it never blocks past it:
    /// it reads only what is already waiting (the server side, against clients that send nothing).
    /// Without one it blocks until the line arrives or the other end closes (the client side).
    pub fn read_line(&self, cap: usize, deadline: Option<Instant>) -> Result<Vec<u8>, ReadError> {
        let mut line = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let want = match deadline {
                Some(d) => match self.available() {
                    None => return Err(ReadError::Closed),
                    Some(0) if Instant::now() >= d => return Err(ReadError::Timeout),
                    Some(0) => {
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    Some(n) => (n as usize).min(buf.len()),
                },
                None => buf.len(),
            };
            let n = self.read_some(&mut buf[..want]).ok_or(ReadError::Closed)?;
            line.extend_from_slice(&buf[..n]);
            if let Some(i) = line.iter().position(|&b| b == b'\n') {
                line.truncate(i);
                return if line.len() > cap { Err(ReadError::TooLarge) } else { Ok(line) };
            }
            if line.len() > cap {
                return Err(ReadError::TooLarge);
            }
        }
    }

    pub fn write_line(&self, line: &[u8]) -> Result<(), String> {
        let mut data = line.to_vec();
        data.push(b'\n');
        let mut done = 0;
        while done < data.len() {
            let mut n = 0u32;
            // SAFETY: the slice length is passed in.
            if unsafe { WriteFile(self.0, data[done..].as_ptr(), (data.len() - done) as u32, &mut n, std::ptr::null_mut()) } == 0 {
                return Err(format!("pipe write failed ({})", last_error()));
            }
            done += n as usize;
        }
        Ok(())
    }

    /// Server side: waits up to `timeout` for the client to close, so the reply is read before the
    /// handle goes.
    pub fn linger(&self, timeout: Duration) {
        let end = Instant::now() + timeout;
        while self.connected() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn client_pid(&self) -> Option<u32> {
        let mut pid = 0;
        (unsafe { GetNamedPipeClientProcessId(self.0, &mut pid) } != 0).then_some(pid)
    }

    pub fn server_pid(&self) -> Option<u32> {
        let mut pid = 0;
        (unsafe { GetNamedPipeServerProcessId(self.0, &mut pid) } != 0).then_some(pid)
    }

    /// The DACL on the pipe object itself, as SDDL.
    pub fn dacl_sddl(&self) -> Result<String, String> {
        use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: the system allocates `sd`; freed below.
        let err = unsafe {
            GetSecurityInfo(self.0, SE_KERNEL_OBJECT, DACL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), &mut sd)
        };
        if err != 0 {
            return Err(format!("GetSecurityInfo failed ({err})"));
        }
        let s = dacl_to_sddl(sd);
        unsafe { LocalFree(sd) };
        s
    }
}

// ---- Server -------------------------------------------------------------------------------

pub struct Listener {
    name: Vec<u16>,
    sd: Descriptor,
    /// An instance waiting for a client. One is created before a connected one is handed out, so
    /// the name never lapses while Konak runs (another process could otherwise take it).
    next: Option<Pipe>,
}

impl Listener {
    /// Creates the first instance. Fails when the name already exists (another process holds it).
    pub fn bind(name: &str) -> Result<Self, String> {
        let mut l = Listener { name: wide(name), sd: Descriptor::current_user_only()?, next: None };
        l.next = Some(l.create(true).map_err(|e| format!("cannot create {name} (error {e}); is another process holding it?"))?);
        Ok(l)
    }

    fn create(&self, first: bool) -> Result<Pipe, u32> {
        let sa = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: self.sd.0, bInheritHandle: 0 };
        let open_mode = PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
        let pipe_mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
        // SAFETY: NUL-terminated name; `sa` and the descriptor outlive the call.
        let h = unsafe { CreateNamedPipeW(self.name.as_ptr(), open_mode, pipe_mode, MAX_INSTANCES, MAX_REPLY as u32, MAX_REQUEST as u32 + 2, 0, &sa) };
        if h == INVALID_HANDLE_VALUE {
            return Err(last_error());
        }
        Ok(Pipe(h))
    }

    /// Blocks until a client connects.
    pub fn accept(&mut self) -> Pipe {
        loop {
            let pipe = match self.next.take() {
                Some(p) => p,
                None => match self.create(false) {
                    Ok(p) => p,
                    // Every instance is in use: wait for one to close.
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(500));
                        continue;
                    }
                },
            };
            // SAFETY: a valid, unconnected server instance; no overlapped I/O.
            let ok = unsafe { ConnectNamedPipe(pipe.0, std::ptr::null_mut()) } != 0 || last_error() == ERROR_PIPE_CONNECTED;
            self.next = self.create(false).ok();
            if ok {
                return pipe;
            }
        }
    }
}

// ---- Client -------------------------------------------------------------------------------

pub const NOT_RUNNING: &str = "Konak is not running; start Konak, then try again";

/// Opens the pipe as a client, with identification-level impersonation only.
fn connect(name: &str) -> Result<Pipe, String> {
    let w = wide(name);
    for attempt in 0..2 {
        // SAFETY: NUL-terminated name; no security attributes or template.
        let h = unsafe {
            CreateFileW(w.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, std::ptr::null(), OPEN_EXISTING, SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION, std::ptr::null_mut())
        };
        if h != INVALID_HANDLE_VALUE {
            return Ok(Pipe(h));
        }
        match last_error() {
            ERROR_FILE_NOT_FOUND => return Err(NOT_RUNNING.into()),
            ERROR_PIPE_BUSY if attempt == 0 => {
                unsafe { WaitNamedPipeW(w.as_ptr(), 3000) };
            }
            ERROR_PIPE_BUSY => return Err("busy: Konak is handling other requests; try again shortly".into()),
            e => return Err(format!("cannot reach Konak (error {e})")),
        }
    }
    unreachable!("the loop returns")
}

/// Volume serial and file index: the same file whatever path spelling (subst drives, 8.3 names).
fn file_id(p: &Path) -> Option<(u32, u32, u32)> {
    use std::os::windows::io::AsRawHandle;
    let f = std::fs::File::open(p).ok()?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is open for the duration of the call.
    (unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } != 0).then_some((info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow))
}

pub fn same_file(a: &Path, b: &Path) -> bool {
    matches!((file_id(a), file_id(b)), (Some(x), Some(y)) if x == y)
}

/// Refuses a server that is not this same konak.exe (a process that took the pipe name).
fn verify_server(pipe: &Pipe) -> Result<(), String> {
    let pid = pipe.server_pid().ok_or("cannot identify the Konak process")?;
    let image = crate::diag::process_image(pid).ok_or("cannot identify the Konak process")?;
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    if !same_file(Path::new(&image), &me) {
        return Err(format!("refused: the pipe is served by {image}, not by this Konak"));
    }
    Ok(())
}

fn exchange(pipe: &Pipe, req: &Request) -> Result<Reply, String> {
    pipe.write_line(&serde_json::to_vec(req).map_err(|e| e.to_string())?)?;
    let line = pipe.read_line(MAX_REPLY, None).map_err(|e| match e {
        ReadError::TooLarge => "reply too large".to_string(),
        _ => "Konak closed the connection without a reply".to_string(),
    })?;
    serde_json::from_slice(&line).map_err(|e| format!("bad reply: {e}"))
}

/// Sends one request to the running Konak and blocks until its reply: after the user's answer and,
/// when approved, the operation (or the server's wait cap).
pub fn call(distro: &str, req: &Request) -> Result<Reply, String> {
    let pipe = connect(&name(distro))?;
    verify_server(&pipe)?;
    exchange(&pipe, req)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::installer::InstalledApp;

    fn state() -> State {
        let mut s = State::default();
        s.done = true;
        s.apps.push(InstalledApp { id: "jellyfin".into(), name: "movies".into(), installed_unix: 0, claimed: true, stopped: false });
        s.apps.push(InstalledApp { id: "forgejo".into(), name: "git".into(), installed_unix: 0, claimed: false, stopped: false });
        s
    }

    fn req(json: &str) -> Result<Request, String> {
        parse(json.as_bytes())
    }

    #[test]
    fn parsing() {
        assert_eq!(req(r#"{"op":"start","app":"movies"}"#).unwrap(), Request { op: Op::Start, app: "movies".into(), name: None, delete_data: None });
        assert!(req(r#"{"op":"start","app":"x","force":true}"#).unwrap_err().contains("unknown field"));
        assert!(req(r#"{"op":"format_disk","app":"x"}"#).unwrap_err().contains("unknown variant"));
        assert!(req(r#"{"op":"start"}"#).unwrap_err().contains("missing field"));
        assert!(req(r#"{"op":"remove","app":"x","delete_data":"yes"}"#).is_err());
        assert!(req("not json").is_err());
        let big = format!(r#"{{"op":"start","app":"{}"}}"#, "a".repeat(MAX_REQUEST));
        assert_eq!(parse(big.as_bytes()).unwrap_err(), "request too large");
    }

    #[test]
    fn validation() {
        let s = state();
        let v = |j: &str| validate(&s, &req(j).unwrap());
        assert_eq!(v(r#"{"op":"remove","app":"movies.local","delete_data":true}"#).unwrap(), Action { op: Op::Remove, id: "jellyfin".into(), name: None, delete_data: true });
        assert!(!v(r#"{"op":"remove","app":"jellyfin"}"#).unwrap().delete_data);
        assert_eq!(v(r#"{"op":"stop","app":"jellyfin","delete_data":false}"#).unwrap_err(), "delete_data is only for remove");
        assert_eq!(v(r#"{"op":"install","app":"vaultwarden","delete_data":true}"#).unwrap_err(), "delete_data is only for remove");
        // Install takes a catalog id that is not installed yet.
        assert_eq!(v(r#"{"op":"install","app":"vaultwarden"}"#).unwrap().id, "vaultwarden");
        assert!(v(r#"{"op":"install","app":"jellyfin"}"#).unwrap_err().starts_with("already_installed"));
        assert!(v(r#"{"op":"install","app":"nope"}"#).unwrap_err().starts_with("unknown_app"));
        // Other ops need an installed app.
        assert!(v(r#"{"op":"start","app":"vaultwarden"}"#).unwrap_err().starts_with("not_installed"));
        assert!(v(r#"{"op":"claim","app":"../../etc"}"#).unwrap_err().starts_with("unknown_app"));
        // Rename: name rules, and the name field only on rename.
        assert_eq!(v(r#"{"op":"rename","app":"git","name":"code"}"#).unwrap().name.as_deref(), Some("code"));
        assert_eq!(v(r#"{"op":"rename","app":"git"}"#).unwrap_err(), "rename needs a name");
        assert_eq!(v(r#"{"op":"start","app":"git","name":"code"}"#).unwrap_err(), "name is only for rename");
        assert_eq!(v(r#"{"op":"rename","app":"git","name":"movies"}"#).unwrap_err(), "name_in_use");
        assert_eq!(v(r#"{"op":"rename","app":"git","name":"konak"}"#).unwrap_err(), "name_reserved");
        assert_eq!(v(r#"{"op":"rename","app":"git","name":"Bad Name"}"#).unwrap_err(), "invalid_name");
        assert_eq!(v(r#"{"op":"rename","app":"git","name":"-x"}"#).unwrap_err(), "invalid_name");
        // Nothing before setup is done.
        let mut early = state();
        early.done = false;
        assert!(validate(&early, &req(r#"{"op":"install","app":"vaultwarden"}"#).unwrap()).unwrap_err().starts_with("setup_not_done"));
    }

    #[test]
    fn reply_serialization() {
        assert_eq!(serde_json::to_string(&Reply::new(Outcome::Done, None)).unwrap(), r#"{"outcome":"done"}"#);
        assert_eq!(serde_json::to_string(&Reply::new(Outcome::Running, None)).unwrap(), r#"{"outcome":"running"}"#);
        assert_eq!(serde_json::to_string(&Reply::new(Outcome::Denied, Some("no".into()))).unwrap(), r#"{"outcome":"denied","error":"no"}"#);
        let r: Reply = serde_json::from_str(r#"{"outcome":"failed","error":"x"}"#).unwrap();
        assert_eq!(r, Reply::new(Outcome::Failed, Some("x".into())));
        assert!(serde_json::from_str::<Reply>(r#"{"outcome":"ok"}"#).is_err());
        assert_eq!(serde_json::to_string(&Request { op: Op::Rename, app: "git".into(), name: Some("code".into()), delete_data: None }).unwrap(), r#"{"op":"rename","app":"git","name":"code"}"#);
    }

    #[test]
    fn pipe_names() {
        assert_eq!(name("konak"), r"\\.\pipe\konak-konak");
        assert_eq!(name("konak-dev.1"), r"\\.\pipe\konak-konak-dev.1");
        assert_eq!(name(r"a\b c"), r"\\.\pipe\konak-a_b_c");
    }

    #[test]
    fn descriptor_grants_only_the_current_user() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-5-"), "{sid}");
        assert_eq!(sddl(&sid), format!("D:P(A;;GRGW;;;{sid})"));
        // The system writes the same rights back in its own order.
        let back = Descriptor::current_user_only().unwrap().dacl_sddl().unwrap();
        assert_eq!(back, format!("D:P(A;;GWGR;;;{sid})"));
        assert_eq!(back.matches("(A;").count(), 1, "{back}");
    }

    fn test_name(tag: &str) -> String {
        format!(r"\\.\pipe\konak-test-{tag}-{}-{}", std::process::id(), crate::confirm::new_id().unwrap())
    }

    /// The real pipe: its own DACL, first-instance exclusivity, framing, the size cap and the timeout.
    #[test]
    fn pipe_roundtrip_and_acl() {
        let name = test_name("rt");
        let mut l = Listener::bind(&name).unwrap();
        // Another first instance under the same name is refused.
        assert!(Listener::bind(&name).is_err());
        let sid = current_user_sid().unwrap();
        let n2 = name.clone();
        let client = std::thread::spawn(move || {
            let p = connect(&n2).unwrap();
            let pid = p.server_pid().unwrap();
            let r = exchange(&p, &Request { op: Op::Start, app: "movies".into(), name: None, delete_data: None });
            (pid, r)
        });
        let server = l.accept();
        let acl = server.dacl_sddl().unwrap();
        println!("pipe DACL: {acl}");
        assert!(acl.starts_with("D:P(") && acl.ends_with(&format!(";;;{sid})")), "{acl}");
        assert_eq!(acl.matches("(A;").count(), 1, "only the current user: {acl}");
        let line = server.read_line(MAX_REQUEST, Some(Instant::now() + REQUEST_TIMEOUT)).ok().unwrap();
        assert_eq!(parse(&line).unwrap().app, "movies");
        assert_eq!(server.client_pid(), Some(std::process::id()));
        server.write_line(&serde_json::to_vec(&Reply::new(Outcome::Denied, None)).unwrap()).unwrap();
        server.linger(Duration::from_secs(5));
        let (pid, r) = client.join().unwrap();
        assert_eq!(pid, std::process::id());
        assert_eq!(r.unwrap().outcome, Outcome::Denied);

        // Oversize request: refused without waiting for a newline.
        let n3 = name.clone();
        let client = std::thread::spawn(move || {
            let p = connect(&n3).unwrap();
            let _ = p.write_line(&vec![b'a'; MAX_REQUEST + 10]);
            p.linger(Duration::from_secs(5));
        });
        let server = l.accept();
        assert!(matches!(server.read_line(MAX_REQUEST, Some(Instant::now() + REQUEST_TIMEOUT)), Err(ReadError::TooLarge)));
        drop(server);
        client.join().unwrap();

        // A client that sends nothing hits the deadline.
        let n4 = name.clone();
        let client = std::thread::spawn(move || {
            let p = connect(&n4).unwrap();
            std::thread::sleep(Duration::from_millis(600));
            drop(p);
        });
        let server = l.accept();
        assert!(matches!(server.read_line(MAX_REQUEST, Some(Instant::now() + Duration::from_millis(200))), Err(ReadError::Timeout)));
        client.join().unwrap();
        server.linger(Duration::from_secs(2));
        assert!(!server.connected());
    }

    #[test]
    fn missing_pipe_means_not_running() {
        assert_eq!(connect(&test_name("none")).err().unwrap(), NOT_RUNNING);
    }

    #[test]
    fn same_file_by_identity() {
        let me = std::env::current_exe().unwrap();
        assert!(same_file(&me, &me));
        let upper = std::path::PathBuf::from(me.to_string_lossy().to_uppercase());
        assert!(same_file(&me, &upper));
        let cargo_toml = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert!(!same_file(&me, &cargo_toml));
        assert!(!same_file(&me, Path::new(r"C:\does\not\exist.exe")));
    }
}
