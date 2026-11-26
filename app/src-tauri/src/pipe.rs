//! Local channel from `konak.exe --mcp` to the running Konak window, for the MCP write tools. One
//! JSON request line, one JSON reply line per connection.
//!
//! Security: the name carries a hash of the user's SID, so each Windows user has their own pipe.
//! The DACL allows only the current user's SID, with read and write-data rights but not
//! `FILE_CREATE_PIPE_INSTANCE`, and an Owner Rights entry takes away the owner's implicit
//! `WRITE_DAC`, so no new open of the pipe can add instances or loosen the DACL; Konak keeps no
//! handle with `WRITE_DAC` either (the first instance's is replaced once the DACL is final), so
//! duplicating Konak's handles does not give that right. A medium-integrity
//! label keeps low-integrity processes out. Remote clients are rejected, and the first instance is
//! created with `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a process that took the name first makes Konak
//! run without the pipe instead of sharing it. The client opens the pipe with identification-level
//! impersonation only and checks, before writing, that the server process runs this same
//! konak.exe file as this same user.
//!
//! Since nobody may create instances once the DACL is final (Konak included: Windows checks every
//! instance after the first against that DACL, for `FILE_CREATE_PIPE_INSTANCE` among others), all
//! instances are created at bind time and reused (disconnect, connect again). The name therefore
//! exists for as long as Konak runs, and a spare instance answers `busy` when every handler is in use.
//!
//! Synchronous Win32 calls (windows-sys) rather than tokio's named pipes: the GUI process has no
//! tokio runtime of its own to drive, the API takes raw security attributes directly, and one
//! thread per instance is plenty for one request at a time.

use crate::installer::State;
use crate::{apps, catalog, mcp};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, GetLastError, LocalFree, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, SetKernelObjectSecurity, TokenUser, DACL_SECURITY_INFORMATION, LABEL_SECURITY_INFORMATION, OBJECT_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetFileInformationByHandle, ReadFile, WriteFile, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, READ_CONTROL, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WRITE_DAC,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId, GetNamedPipeServerProcessId, PeekNamedPipe, WaitNamedPipeW,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION};

/// Largest request line, newline excluded.
pub const MAX_REQUEST: usize = 4096;
const MAX_REPLY: usize = 16 * 1024;
/// A client gets this long to send its request line.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How the client opens the pipe: identification-level impersonation only, so the server can see
/// who the client is but cannot act as the client.
const CLIENT_FLAGS: u32 = SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION;
/// Every instance: a blocking byte stream, local clients only.
const PIPE_MODE: u32 = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;

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

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Done => "done",
            Outcome::Failed => "failed",
            Outcome::Denied => "denied",
            Outcome::Busy => "busy",
            Outcome::Invalid => "invalid",
            Outcome::Running => "running",
        }
    }
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

/// The pipe for a user and a distro, `\\.\pipe\konak-<sid-hash8>-<distro>`, so other Windows users
/// and isolated dev runs (`KONAK_DISTRO`) do not collide. The hash is the first 8 hex digits of the
/// SHA-256 of the SID string. Characters a pipe name cannot carry become `_`.
pub fn name(sid: &str, distro: &str) -> String {
    let hash: String = Sha256::digest(sid.as_bytes())[..4].iter().map(|b| format!("{b:02x}")).collect();
    let safe: String = distro.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect();
    format!(r"\\.\pipe\konak-{hash}-{safe}")
}

/// `name` for the user this process runs as; the server and the client both derive it this way.
pub fn name_for(distro: &str) -> Result<String, String> {
    Ok(name(&current_user_sid()?, distro))
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
    // A request that would change nothing gets no dialog.
    if let Some(app) = state.apps.iter().find(|a| a.id == id) {
        match req.op {
            Op::Rename if req.name.as_deref() == Some(app.name.as_str()) => return Err(format!("same_name: {id} is already called {}", app.name)),
            Op::Start if !app.stopped => return Err(format!("not_stopped: {id} is not stopped (restart_app restarts it)")),
            Op::Stop if app.stopped => return Err(format!("already_stopped: {id}")),
            Op::Claim if app.claimed => return Err(format!("already_claimed: {id}")),
            _ => {}
        }
    }
    Ok(Action { op: req.op, id, name: req.name.clone(), delete_data: req.delete_data.unwrap_or(false) })
}

// ---- Security descriptor ------------------------------------------------------------------

/// What the user may do with the pipe: read, plus write data and attributes (what changing a
/// client's read mode needs). Not `FILE_APPEND_DATA`, which on a pipe is `FILE_CREATE_PIPE_INSTANCE`, and none of
/// the other write rights `GENERIC_WRITE` carries (`FILE_WRITE_EA`). 0x12018b.
pub const ACCESS: u32 = FILE_GENERIC_READ | FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES;

/// `ACCESS` for exactly this SID; `P` blocks inherited entries, so nothing else gets access. The
/// Owner Rights entry (`OW`) replaces the owner's implicit `READ_CONTROL | WRITE_DAC` with
/// `READ_CONTROL`: the owner is this same user, so without it any of the user's processes could
/// open the pipe with `WRITE_DAC` and loosen it. The mandatory label keeps processes below medium
/// integrity out entirely.
pub fn sddl(sid: &str) -> String {
    sddl_with(sid, ACCESS)
}

fn sddl_with(sid: &str, access: u32) -> String {
    format!("D:P(A;;{access:#x};;;{sid})(A;;{READ_CONTROL:#x};;;OW)S:(ML;;NWNRNX;;;ME)")
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
    // SAFETY: the pseudo handle of this process needs no closing.
    unsafe { token_user_sid(GetCurrentProcess()) }
}

/// The SID of the user another process runs as.
pub fn process_user_sid(pid: u32) -> Result<String, String> {
    // SAFETY: the process handle is ours and closed after use.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return Err(format!("OpenProcess failed ({})", last_error()));
        }
        let sid = token_user_sid(process);
        CloseHandle(process);
        sid
    }
}

/// The token user of `process` as a string SID.
///
/// # Safety
/// `process` must be a valid process handle with query access.
unsafe fn token_user_sid(process: HANDLE) -> Result<String, String> {
    // SAFETY: the token handle is ours and closed; the buffer is sized by the first call and
    // aligned for TOKEN_USER (u64 storage).
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
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

    /// The DACL and the integrity label as SDDL, as the system reads them back.
    pub fn sddl(&self) -> Result<String, String> {
        to_sddl(self.0)
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

const READ_BACK: OBJECT_SECURITY_INFORMATION = DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION;

fn to_sddl(sd: PSECURITY_DESCRIPTOR) -> Result<String, String> {
    let mut s: *mut u16 = std::ptr::null_mut();
    // SAFETY: `sd` is a valid descriptor; the system allocates `s`, freed by take_local_wide.
    unsafe {
        if ConvertSecurityDescriptorToStringSecurityDescriptorW(sd, SDDL_REVISION_1, READ_BACK, &mut s, std::ptr::null_mut()) == 0 {
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
    /// The other end closed before sending a single byte.
    NothingSent,
    /// The other end closed in the middle of a line.
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
            let closed = if line.is_empty() { ReadError::NothingSent } else { ReadError::Closed };
            let want = match deadline {
                Some(d) => match self.available() {
                    None => return Err(closed),
                    Some(0) if Instant::now() >= d => return Err(ReadError::Timeout),
                    Some(0) => {
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    Some(n) => (n as usize).min(buf.len()),
                },
                None => buf.len(),
            };
            let n = self.read_some(&mut buf[..want]).ok_or(closed)?;
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

    /// The DACL and integrity label on the pipe object itself, as SDDL.
    pub fn sddl(&self) -> Result<String, String> {
        use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: the system allocates `sd`; freed below.
        let err = unsafe {
            GetSecurityInfo(self.0, SE_KERNEL_OBJECT, READ_BACK, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), &mut sd)
        };
        if err != 0 {
            return Err(format!("GetSecurityInfo failed ({err})"));
        }
        let s = to_sddl(sd);
        unsafe { LocalFree(sd) };
        s
    }
}

// ---- Server -------------------------------------------------------------------------------

impl Pipe {
    /// Server side: blocks until a client connects to this instance. False when the connection
    /// failed (e.g. the client already left); `disconnect` before trying again.
    pub fn accept(&self) -> bool {
        // SAFETY: a valid server instance; no overlapped I/O.
        unsafe { ConnectNamedPipe(self.0, std::ptr::null_mut()) != 0 || GetLastError() == ERROR_PIPE_CONNECTED }
    }

    /// Server side: drops the client (and anything it has not read yet), so the instance can
    /// accept the next one.
    pub fn disconnect(&self) {
        // SAFETY: a valid server instance.
        unsafe { DisconnectNamedPipe(self.0) };
    }
}

/// Every instance of one pipe name, all created at bind time and reused for the life of Konak.
pub struct Listener {
    pub instances: Vec<Pipe>,
}

impl Listener {
    /// Creates `count` instances, then makes the DACL final (`sddl`): from then on nobody, Konak
    /// included, can create another instance. Fails when the name already exists (another process
    /// holds it).
    pub fn bind(name: &str, count: u32) -> Result<Self, String> {
        let sid = current_user_sid()?;
        let w = wide(name);
        // Only while the instances are created. Each further instance is access-checked against the
        // pipe's DACL for what a duplex server end asks (`GENERIC_READ | GENERIC_WRITE`, which on a
        // pipe includes `FILE_CREATE_PIPE_INSTANCE`), so the user gets that much for now.
        let setup = Descriptor::from_sddl(&sddl_with(&sid, FILE_GENERIC_READ | FILE_GENERIC_WRITE))?;
        let first = create(&w, &setup, FILE_FLAG_FIRST_PIPE_INSTANCE | WRITE_DAC).map_err(|e| format!("cannot create {name} (error {e}); is another process holding it?"))?;
        let mut instances = vec![first];
        for _ in 1..count {
            instances.push(create(&w, &setup, 0).map_err(|e| format!("cannot create an instance of {name} (error {e})"))?);
        }
        let last = Descriptor::current_user_only()?;
        // SAFETY: the first instance's handle was opened with WRITE_DAC; the descriptor is valid.
        // The DACL belongs to the pipe, so this applies to every instance.
        if unsafe { SetKernelObjectSecurity(instances[0].0, DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION, last.0) } == 0 {
            return Err(format!("cannot restrict {name} (error {})", last_error()));
        }
        // That was the only use of WRITE_DAC: keep the first instance through a handle without it.
        instances[0] = without_write_dac(&instances[0]).map_err(|e| format!("cannot reopen {name} (error {e})"))?;
        Ok(Listener { instances })
    }
}

/// Another handle to the same server instance with what serving needs (read and write) but not
/// `WRITE_DAC`. A handle's granted access never grows, so this one cannot be used to change the
/// DACL, by Konak or by a process that duplicates it out of Konak.
fn without_write_dac(p: &Pipe) -> Result<Pipe, u32> {
    let mut h: HANDLE = std::ptr::null_mut();
    // SAFETY: a valid handle of this process, duplicated into this process; a subset of its access.
    let ok = unsafe {
        let me = GetCurrentProcess();
        DuplicateHandle(me, p.0, me, &mut h, FILE_GENERIC_READ | FILE_GENERIC_WRITE, 0, 0)
    };
    if ok == 0 {
        return Err(last_error());
    }
    Ok(Pipe(h))
}

/// One instance. No instance limit: the final DACL is what stops anyone from adding instances.
fn create(name: &[u16], sd: &Descriptor, flags: u32) -> Result<Pipe, u32> {
    let sa = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd.0, bInheritHandle: 0 };
    // SAFETY: NUL-terminated name; `sa` and the descriptor outlive the call.
    let h = unsafe { CreateNamedPipeW(name.as_ptr(), PIPE_ACCESS_DUPLEX | flags, PIPE_MODE, PIPE_UNLIMITED_INSTANCES, MAX_REPLY as u32, MAX_REQUEST as u32 + 2, 0, &sa) };
    if h == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    Ok(Pipe(h))
}

/// Caps how many connections are handled at once; a connection beyond the cap gets `busy`.
pub struct Gate {
    active: AtomicUsize,
    cap: usize,
}

/// One handled connection; leaves the gate on drop.
pub struct Pass<'a>(&'a Gate);

impl Gate {
    pub const fn new(cap: usize) -> Self {
        Gate { active: AtomicUsize::new(0), cap }
    }

    pub fn enter(&self) -> Option<Pass<'_>> {
        if self.active.fetch_add(1, Ordering::SeqCst) < self.cap {
            return Some(Pass(self));
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
        None
    }
}

impl Drop for Pass<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The op and app a request line names, as sent, for the audit line. Read loosely, so a request
/// that `parse` or `validate` refuses is still described; a field that is missing stays `None`.
#[derive(Default, Debug, PartialEq)]
pub struct Sent {
    pub op: Option<String>,
    pub app: Option<String>,
}

pub fn sent(line: &[u8]) -> Sent {
    let v: serde_json::Value = serde_json::from_slice(line).unwrap_or_default();
    let field = |k: &str| v.get(k).map(|x| x.as_str().map_or_else(|| x.to_string(), String::from));
    Sent { op: field("op"), app: field("app") }
}

/// The `install.log` line for a finished pipe request: its confirmation id when it got one, who
/// sent it, the outcome, the op and app it named, and the reason. The op, app and reason are
/// debug-quoted, so a client cannot start a new log line.
pub fn audit_line(id: Option<&str>, requester: Option<&str>, sent: &Sent, reply: &Reply) -> String {
    let mut line = format!("mcp request {} from {}: {}", id.unwrap_or("-"), requester.unwrap_or("unknown"), reply.outcome.as_str());
    if let Some(op) = &sent.op {
        line.push_str(&format!(" op={op:?}"));
    }
    if let Some(app) = &sent.app {
        line.push_str(&format!(" app={app:?}"));
    }
    if let Some(e) = &reply.error {
        line.push_str(&format!(" {e:?}"));
    }
    line
}

// ---- Client -------------------------------------------------------------------------------

pub const NOT_RUNNING: &str = "Konak is not running; start Konak, then try again";

/// Opens the pipe as a client, with identification-level impersonation only.
fn connect(name: &str) -> Result<Pipe, String> {
    let w = wide(name);
    for attempt in 0..2 {
        // SAFETY: NUL-terminated name; no security attributes or template.
        let h = unsafe {
            CreateFileW(w.as_ptr(), ACCESS, 0, std::ptr::null(), OPEN_EXISTING, CLIENT_FLAGS, std::ptr::null_mut())
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

/// Refuses a server that is not this same konak.exe running as this same user (a process that
/// took the pipe name).
fn verify_server(pipe: &Pipe) -> Result<(), String> {
    let pid = pipe.server_pid().ok_or("cannot identify the Konak process")?;
    let image = crate::diag::process_image(pid).ok_or("cannot identify the Konak process")?;
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    check_server_image(&image, &me)?;
    let server_user = process_user_sid(pid).map_err(|e| format!("cannot identify the Konak process's user: {e}"))?;
    check_server_user(&server_user, &current_user_sid()?)
}

/// The server process runs `image`; it must be this konak.exe (`me`), the same file by identity.
fn check_server_image(image: &str, me: &Path) -> Result<(), String> {
    if !same_file(Path::new(image), me) {
        return Err(format!("refused: the pipe is served by {image}, not by this Konak"));
    }
    Ok(())
}

/// The server process runs as `server_user`; it must be this process's user, `my_user`.
fn check_server_user(server_user: &str, my_user: &str) -> Result<(), String> {
    if server_user != my_user {
        return Err(format!("refused: the pipe is served by another Windows user ({server_user})"));
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
    let pipe = connect(&name_for(distro)?)?;
    verify_server(&pipe)?;
    exchange(&pipe, req)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::installer::InstalledApp;
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
    use windows_sys::Win32::Security::{RevertToSelf, SecurityIdentification, TokenImpersonationLevel, SECURITY_IMPERSONATION_LEVEL};
    use windows_sys::Win32::Storage::FileSystem::{FILE_CREATE_PIPE_INSTANCE, SECURITY_DELEGATION};
    use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};

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

    /// The cap is 4 KiB: a line of exactly 4096 bytes is parsed, one byte more is refused.
    #[test]
    fn the_request_cap_is_4_kib() {
        assert_eq!(MAX_REQUEST, 4096);
        let line = |len: usize| {
            let base = r#"{"op":"start","app":""}"#;
            format!(r#"{{"op":"start","app":"{}"}}"#, "a".repeat(len - base.len()))
        };
        assert_eq!(line(4096).len(), 4096);
        assert_eq!(parse(line(4096).as_bytes()).unwrap().app.len(), 4096 - 23);
        assert_eq!(parse(line(4097).as_bytes()).unwrap_err(), "request too large");
    }

    /// The client asks for identification-level impersonation only; instances refuse remote clients.
    #[test]
    fn security_flags() {
        assert_eq!(CLIENT_FLAGS & SECURITY_SQOS_PRESENT, SECURITY_SQOS_PRESENT);
        // The level is two bits; SECURITY_DELEGATION has both set.
        assert_eq!(CLIENT_FLAGS & SECURITY_DELEGATION, SECURITY_IDENTIFICATION);
        assert_eq!(PIPE_MODE & PIPE_REJECT_REMOTE_CLIENTS, PIPE_REJECT_REMOTE_CLIENTS);
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
        // Requests that would change nothing.
        assert!(v(r#"{"op":"rename","app":"git","name":"git"}"#).unwrap_err().starts_with("same_name"));
        assert!(v(r#"{"op":"rename","app":"forgejo","name":"git"}"#).unwrap_err().starts_with("same_name"));
        assert!(v(r#"{"op":"start","app":"movies"}"#).unwrap_err().starts_with("not_stopped"));
        assert!(v(r#"{"op":"claim","app":"jellyfin"}"#).unwrap_err().starts_with("already_claimed"));
        assert_eq!(v(r#"{"op":"claim","app":"git"}"#).unwrap().id, "forgejo");
        assert_eq!(v(r#"{"op":"stop","app":"movies"}"#).unwrap().id, "jellyfin");
        let mut stopped = state();
        stopped.apps[0].stopped = true;
        let v2 = |j: &str| validate(&stopped, &req(j).unwrap());
        assert!(v2(r#"{"op":"stop","app":"movies"}"#).unwrap_err().starts_with("already_stopped"));
        assert_eq!(v2(r#"{"op":"start","app":"movies"}"#).unwrap().id, "jellyfin");
        assert_eq!(v2(r#"{"op":"restart","app":"movies"}"#).unwrap().id, "jellyfin");
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
        let sid = "S-1-5-21-1-2-3-1001";
        let n = name(sid, "konak");
        let hash = n.strip_prefix(r"\\.\pipe\konak-").unwrap().strip_suffix("-konak").unwrap();
        assert!(hash.len() == 8 && hash.chars().all(|c| c.is_ascii_hexdigit()), "{n}");
        assert_eq!(name(sid, "konak"), n, "stable");
        assert_ne!(name("S-1-5-21-1-2-3-1002", "konak"), n, "per user");
        assert_eq!(name(sid, "konak-dev.1"), format!(r"\\.\pipe\konak-{hash}-konak-dev.1"));
        assert_eq!(name(sid, r"a\b c"), format!(r"\\.\pipe\konak-{hash}-a_b_c"));
        assert_eq!(name_for("konak").unwrap(), name(&current_user_sid().unwrap(), "konak"));
    }

    #[test]
    fn descriptor_grants_only_the_current_user() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-5-"), "{sid}");
        assert_eq!(ACCESS, 0x12018b);
        assert_eq!(ACCESS & (FILE_CREATE_PIPE_INSTANCE | WRITE_DAC), 0);
        assert_eq!(sddl(&sid), format!("D:P(A;;0x12018b;;;{sid})(A;;0x20000;;;OW)S:(ML;;NWNRNX;;;ME)"));
        let back = Descriptor::current_user_only().unwrap().sddl().unwrap();
        assert_eq!(back, format!("D:P(A;;0x12018b;;;{sid})(A;;RC;;;OW)S:(ML;;NWNRNX;;;ME)"));
    }

    #[test]
    fn process_user_is_read_from_its_token() {
        assert_eq!(process_user_sid(std::process::id()).unwrap(), current_user_sid().unwrap());
        assert!(process_user_sid(u32::MAX - 3).is_err());
    }

    #[test]
    fn gate_caps_concurrent_handlers() {
        let g = Gate::new(2);
        let a = g.enter().unwrap();
        let b = g.enter().unwrap();
        assert!(g.enter().is_none());
        assert!(g.enter().is_none(), "a refusal does not count");
        drop(a);
        let _c = g.enter().unwrap();
        assert!(g.enter().is_none());
        drop(b);
        assert!(g.enter().is_some());
    }

    #[test]
    fn audit_lines() {
        let r = Reply::new(Outcome::Invalid, Some("unknown_app: x\nfake line".into()));
        let s = sent(br#"{"op":"install","app":"x\nfake line"}"#);
        assert_eq!(audit_line(None, Some(r"C:\a\node.exe"), &s, &r), r#"mcp request - from C:\a\node.exe: invalid op="install" app="x\nfake line" "unknown_app: x\nfake line""#);
        assert_eq!(audit_line(Some("ab12"), None, &Sent::default(), &Reply::new(Outcome::Done, None)), "mcp request ab12 from unknown: done");
        // Refused before parsing: whatever the line named is still shown; a non-string as its JSON.
        let busy = Reply::new(Outcome::Busy, Some("jellyfin has an operation in progress".into()));
        assert_eq!(audit_line(None, None, &sent(br#"{"op":"zap","app":7,"extra":1}"#), &busy), r#"mcp request - from unknown: busy op="zap" app="7" "jellyfin has an operation in progress""#);
        assert_eq!(sent(b"not json"), Sent::default());
        assert_eq!(sent(br#"{"op":"stop"}"#), Sent { op: Some("stop".into()), app: None });
    }

    fn test_name(tag: &str) -> String {
        format!(r"\\.\pipe\konak-test-{tag}-{}-{}", std::process::id(), crate::confirm::new_id().unwrap())
    }

    fn open_raw(name: &str, access: u32) -> Result<Pipe, u32> {
        let w = wide(name);
        let h = unsafe { CreateFileW(w.as_ptr(), access, 0, std::ptr::null(), OPEN_EXISTING, CLIENT_FLAGS, std::ptr::null_mut()) };
        if h == INVALID_HANDLE_VALUE {
            return Err(last_error());
        }
        Ok(Pipe(h))
    }

    /// Whether this handle may change the pipe's DACL (it is set to the final DACL, so a success
    /// changes nothing).
    fn set_dacl(p: &Pipe) -> Result<(), u32> {
        let d = Descriptor::current_user_only().unwrap();
        // SAFETY: a valid handle and descriptor.
        if unsafe { SetKernelObjectSecurity(p.0, DACL_SECURITY_INFORMATION, d.0) } == 0 {
            return Err(last_error());
        }
        Ok(())
    }

    /// The impersonation level the connected client allows, read by impersonating it (it must have
    /// sent something first).
    fn client_impersonation_level(server: &Pipe) -> SECURITY_IMPERSONATION_LEVEL {
        // SAFETY: a connected server instance; the thread reverts before returning; the token is closed.
        unsafe {
            assert!(ImpersonateNamedPipeClient(server.0) != 0, "impersonate: {}", last_error());
            let mut token: HANDLE = std::ptr::null_mut();
            let opened = OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) != 0;
            let err = last_error();
            RevertToSelf();
            assert!(opened, "OpenThreadToken: {err}");
            let (mut level, mut len): (SECURITY_IMPERSONATION_LEVEL, u32) = (-1, 0);
            let ok = GetTokenInformation(token, TokenImpersonationLevel, (&mut level as *mut SECURITY_IMPERSONATION_LEVEL).cast(), 4, &mut len) != 0;
            CloseHandle(token);
            assert!(ok, "GetTokenInformation: {}", last_error());
            level
        }
    }

    /// One request on a server instance: accept, read, reply, wait for the client, disconnect.
    fn serve_once(server: &Pipe, reply: &Reply) -> Request {
        assert!(server.accept());
        let line = server.read_line(MAX_REQUEST, Some(Instant::now() + REQUEST_TIMEOUT)).ok().unwrap();
        server.write_line(&serde_json::to_vec(reply).unwrap()).unwrap();
        server.linger(Duration::from_secs(5));
        server.disconnect();
        parse(&line).unwrap()
    }

    /// The real pipe: its own descriptor, no new instances and no DACL changes by the user,
    /// first-instance exclusivity, reuse of an instance, framing, the size cap and the timeout.
    #[test]
    fn pipe_roundtrip_and_acl() {
        let name = test_name("rt");
        let l = Listener::bind(&name, 1).unwrap();
        let server = &l.instances[0];
        // Another first instance under the same name is refused (by the first-instance flag or the
        // final DACL, both error 5; `a_squatted_name_is_refused` isolates the flag).
        assert!(Listener::bind(&name, 1).err().unwrap().contains("(error 5)"));
        // No handle Konak keeps may change the DACL.
        assert_eq!(set_dacl(server), Err(ERROR_ACCESS_DENIED));
        let sid = current_user_sid().unwrap();
        let acl = server.sddl().unwrap();
        println!("pipe descriptor: {acl}");
        assert_eq!(acl, format!("D:P(A;;0x12018b;;;{sid})(A;;RC;;;OW)S:AI(ML;;NWNRNX;;;ME)"));
        // Nobody may add an instance now, Konak included (the instance limit is unlimited, so this
        // is the DACL speaking), and the owner's implicit WRITE_DAC is gone.
        let w = wide(&name);
        assert_eq!(create(&w, &Descriptor::current_user_only().unwrap(), 0).err(), Some(5));
        assert_eq!(open_raw(&name, ACCESS | WRITE_DAC).err(), Some(5));

        for round in 0..2 {
            let n2 = name.clone();
            let client = std::thread::spawn(move || {
                let p = connect(&n2).unwrap();
                let pid = p.server_pid().unwrap();
                let r = exchange(&p, &Request { op: Op::Start, app: "movies".into(), name: None, delete_data: None });
                (pid, r)
            });
            assert_eq!(serve_once(server, &Reply::new(Outcome::Denied, None)).app, "movies", "round {round}");
            let (pid, r) = client.join().unwrap();
            assert_eq!(pid, std::process::id());
            assert_eq!(r.unwrap().outcome, Outcome::Denied);
        }

        // Oversize request, newline at the end: refused.
        let n3 = name.clone();
        let client = std::thread::spawn(move || {
            let p = connect(&n3).unwrap();
            let _ = p.write_line(&vec![b'a'; MAX_REQUEST + 10]);
            p.linger(Duration::from_secs(5));
        });
        assert!(server.accept());
        assert_eq!(server.client_pid(), Some(std::process::id()));
        assert!(matches!(server.read_line(MAX_REQUEST, Some(Instant::now() + REQUEST_TIMEOUT)), Err(ReadError::TooLarge)));
        server.disconnect();
        client.join().unwrap();

        // Oversize request with no newline, the client staying connected: refused as soon as the
        // cap is passed, not at the deadline.
        let n5 = name.clone();
        let client = std::thread::spawn(move || {
            let p = connect(&n5).unwrap();
            let data = vec![b'a'; MAX_REQUEST + 10];
            let mut n = 0u32;
            // SAFETY: the slice length is passed in. It may fail once the server drops the client.
            unsafe { WriteFile(p.0, data.as_ptr(), data.len() as u32, &mut n, std::ptr::null_mut()) };
            p.linger(Duration::from_secs(5));
        });
        assert!(server.accept());
        let start = Instant::now();
        assert!(matches!(server.read_line(MAX_REQUEST, Some(start + Duration::from_secs(5))), Err(ReadError::TooLarge)));
        assert!(start.elapsed() < Duration::from_secs(2), "{:?}", start.elapsed());
        server.disconnect();
        client.join().unwrap();

        // A client that hangs up without sending anything.
        let n6 = name.clone();
        let client = std::thread::spawn(move || drop(connect(&n6).unwrap()));
        assert!(server.accept());
        client.join().unwrap();
        assert!(matches!(server.read_line(MAX_REQUEST, Some(Instant::now() + REQUEST_TIMEOUT)), Err(ReadError::NothingSent)));
        server.disconnect();

        // A client that sends part of a line, then hangs up.
        let n7 = name.clone();
        let client = std::thread::spawn(move || {
            let p = connect(&n7).unwrap();
            let mut n = 0u32;
            // SAFETY: a valid buffer and length.
            unsafe { WriteFile(p.0, b"{\"op\"".as_ptr(), 5, &mut n, std::ptr::null_mut()) };
        });
        assert!(server.accept());
        client.join().unwrap();
        assert!(matches!(server.read_line(MAX_REQUEST, Some(Instant::now() + REQUEST_TIMEOUT)), Err(ReadError::Closed)));
        server.disconnect();

        // A client that sends nothing hits the deadline.
        let n4 = name.clone();
        let client = std::thread::spawn(move || {
            let p = connect(&n4).unwrap();
            std::thread::sleep(Duration::from_millis(600));
            drop(p);
        });
        assert!(server.accept());
        assert!(matches!(server.read_line(MAX_REQUEST, Some(Instant::now() + Duration::from_millis(200))), Err(ReadError::Timeout)));
        client.join().unwrap();
        server.linger(Duration::from_secs(2));
        assert!(!server.connected());
        server.disconnect();
    }

    /// With every instance connected, a new client waits briefly and then gets a `busy` error.
    #[test]
    fn all_instances_in_use_means_busy() {
        let name = test_name("full");
        let l = Listener::bind(&name, 1).unwrap();
        let first = connect(&name).unwrap();
        assert!(l.instances[0].accept());
        assert!(connect(&name).err().unwrap().starts_with("busy"));
        drop(first);
    }

    /// Several instances: each has the final descriptor, and a client reaches the second one while
    /// the first is connected.
    #[test]
    fn every_instance_gets_the_final_descriptor() {
        let name = test_name("multi");
        let l = Listener::bind(&name, 3).unwrap();
        let sid = current_user_sid().unwrap();
        for p in &l.instances {
            assert_eq!(p.sddl().unwrap(), format!("D:P(A;;0x12018b;;;{sid})(A;;RC;;;OW)S:AI(ML;;NWNRNX;;;ME)"));
            assert_eq!(set_dacl(p), Err(ERROR_ACCESS_DENIED));
        }
        assert_eq!(create(&wide(&name), &Descriptor::current_user_only().unwrap(), 0).err(), Some(5));
        // Two clients at once, so two instances serve (one instance takes one client; see
        // `all_instances_in_use_means_busy`).
        let a = connect(&name).unwrap();
        let b = connect(&name).unwrap();
        drop((a, b));
    }

    /// `call` end to end: the server here is this same executable and user, so both checks pass.
    #[test]
    fn call_checks_the_server_and_returns_its_reply() {
        let distro = format!("test-call-{}-{}", std::process::id(), crate::confirm::new_id().unwrap());
        let l = Listener::bind(&name_for(&distro).unwrap(), 1).unwrap();
        let client = std::thread::spawn(move || call(&distro, &Request { op: Op::Remove, app: "git".into(), name: None, delete_data: Some(true) }));
        let server = &l.instances[0];
        assert!(server.accept());
        let got = parse(&server.read_line(MAX_REQUEST, Some(Instant::now() + REQUEST_TIMEOUT)).ok().unwrap()).unwrap();
        assert_eq!(got.delete_data, Some(true));
        server.write_line(br#"{"outcome":"failed","error":"remove of forgejo failed"}"#).unwrap();
        server.linger(Duration::from_secs(5));
        assert_eq!(client.join().unwrap().unwrap(), Reply::new(Outcome::Failed, Some("remove of forgejo failed".into())));
    }

    /// A process of this user that created the name first, with a DACL that would let Konak add
    /// instances to it: Konak refuses to share the name (`FILE_FLAG_FIRST_PIPE_INSTANCE`).
    #[test]
    fn a_squatted_name_is_refused() {
        let name = test_name("squat");
        let open = Descriptor::from_sddl(&format!("D:P(A;;GA;;;{})", current_user_sid().unwrap())).unwrap();
        let _squatter = create(&wide(&name), &open, 0).unwrap();
        assert!(create(&wide(&name), &open, 0).is_ok(), "the squatter's DACL lets anyone of this user add instances");
        let err = Listener::bind(&name, 1).err().unwrap();
        assert!(err.starts_with("cannot create ") && err.contains("(error 5)"), "{err}");
    }

    /// The client's handle carries identification-level impersonation, so a server learns who the
    /// client is but cannot act as the client.
    #[test]
    fn the_server_can_only_identify_the_client() {
        let name = test_name("sqos");
        let l = Listener::bind(&name, 1).unwrap();
        let n = name.clone();
        let client = std::thread::spawn(move || {
            let p = connect(&n).unwrap();
            p.write_line(b"{}").unwrap();
            let _ = p.read_line(MAX_REPLY, None);
        });
        let server = &l.instances[0];
        assert!(server.accept());
        server.read_line(MAX_REQUEST, Some(Instant::now() + REQUEST_TIMEOUT)).ok().unwrap();
        assert_eq!(client_impersonation_level(server), SecurityIdentification);
        server.write_line(b"{}").unwrap();
        client.join().unwrap();
    }

    #[test]
    fn missing_pipe_means_not_running() {
        assert_eq!(connect(&test_name("none")).err().unwrap(), NOT_RUNNING);
    }

    #[test]
    fn the_server_must_be_this_exe_and_this_user() {
        let me = std::env::current_exe().unwrap();
        assert_eq!(check_server_image(&me.to_string_lossy(), &me), Ok(()));
        assert_eq!(check_server_image(&me.to_string_lossy().to_uppercase(), &me), Ok(()), "same file, other spelling");
        let other = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let other = other.to_string_lossy();
        assert_eq!(check_server_image(&other, &me).unwrap_err(), format!("refused: the pipe is served by {other}, not by this Konak"));
        assert!(check_server_image(r"C:\does\not\exist.exe", &me).is_err());
        let sid = current_user_sid().unwrap();
        assert_eq!(check_server_user(&sid, &sid), Ok(()));
        assert_eq!(check_server_user("S-1-5-21-1-2-3-1002", &sid).unwrap_err(), "refused: the pipe is served by another Windows user (S-1-5-21-1-2-3-1002)");
    }

    /// A process that is not this exe serves the pipe name (a Windows PowerShell pipe server): `call`
    /// refuses it before writing a byte.
    #[test]
    fn call_refuses_a_server_that_is_not_this_exe() {
        use std::io::BufRead;
        let distro = format!("test-rogue-{}-{}", std::process::id(), crate::confirm::new_id().unwrap());
        let full = name_for(&distro).unwrap();
        let short = full.strip_prefix(r"\\.\pipe\").unwrap();
        let script = format!(
            "$s = New-Object System.IO.Pipes.NamedPipeServerStream('{short}', 'InOut', 1, 'Byte', 'Asynchronous'); \
             $c = $s.WaitForConnectionAsync(); [Console]::Out.WriteLine('ready'); $c.Wait(); \
             [Console]::Out.WriteLine('read ' + $s.ReadByte()); $s.Dispose()"
        );
        let mut ps = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = std::io::BufReader::new(ps.stdout.take().unwrap()).lines();
        assert_eq!(out.next().unwrap().unwrap(), "ready");
        let err = call(&distro, &Request { op: Op::Start, app: "movies".into(), name: None, delete_data: None }).unwrap_err();
        assert!(err.starts_with("refused: the pipe is served by ") && err.to_lowercase().ends_with("powershell.exe, not by this konak"), "{err}");
        // The server saw the connection end without a single byte of the request.
        assert_eq!(out.next().unwrap().unwrap(), "read -1");
        ps.wait().unwrap();
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
