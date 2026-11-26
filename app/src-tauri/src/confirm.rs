//! The one MCP write request waiting for the user's answer in the Konak window.
//!
//! The pipe handler opens a request and gets a receiver. The only sender lives inside the pending
//! entry, and the only code that sends on it is `Slot::answer`, which the `mcp_answer` command
//! calls with the id the window was shown. So a pipe request turns into an operation only when the
//! user answered that exact id with `approve = true`; every other path ends as a denial.
//!
//! `handle` is the whole decision for one pipe request (validate, busy, open the slot, wait for the
//! answer, validate again, run), with what it needs from the running Konak behind `Host`, so the
//! tests can check that nothing runs without that approval.

use crate::pipe::{Action, Outcome, Reply, Request};
use serde::Serialize;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long a request waits for an answer before it counts as denied.
pub const ANSWER_TIMEOUT_S: u64 = 300;

/// After a denial (the user's, the timeout's or hiding the window), the same request is refused
/// without a dialog for this long, so an agent cannot wear the person down by asking again.
pub const COOLDOWN_S: u64 = 120;

/// `Slot::open`'s error for a request inside the cooldown.
pub const DECLINED_RECENTLY: &str = "declined_recently";

/// What the window shows, sent as the `mcp-confirm` event and returned by `mcp_pending`.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Payload {
    pub id: String,
    /// File name of the program on the other end of the pipe, when it could be read.
    pub requester: Option<String>,
    pub op: &'static str,
    /// Catalog id.
    pub app: String,
    /// Rename target.
    pub name: Option<String>,
    pub delete_data: bool,
    pub expires_unix: u64,
}

struct Pending {
    payload: Payload,
    answer: Sender<bool>,
}

/// What makes two requests the same for the cooldown.
#[derive(Clone, Debug, PartialEq)]
struct Key {
    op: &'static str,
    app: String,
    name: Option<String>,
    delete_data: bool,
}

impl Key {
    fn of(p: &Payload) -> Self {
        Key { op: p.op, app: p.app.clone(), name: p.name.clone(), delete_data: p.delete_data }
    }
}

#[derive(Default)]
pub struct Slot {
    pending: Option<Pending>,
    /// Denied requests and when, kept for `COOLDOWN_S`.
    declined: Vec<(Key, u64)>,
}

/// 128 random bits from the OS generator, hex encoded.
pub fn new_id() -> Result<String, String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| format!("no random source: {e}"))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

impl Slot {
    /// Opens a request. `DECLINED_RECENTLY` when the same request was denied less than
    /// `COOLDOWN_S` ago; `busy` while another one awaits an answer. An entry left past its expiry
    /// (its handler is gone) is dropped, which reads as a denial on its receiver.
    pub fn open(&mut self, payload: Payload, now: u64) -> Result<Receiver<bool>, String> {
        self.declined.retain(|(_, at)| now.saturating_sub(*at) < COOLDOWN_S);
        let key = Key::of(&payload);
        if self.declined.iter().any(|(k, _)| *k == key) {
            return Err(DECLINED_RECENTLY.into());
        }
        if self.pending.as_ref().is_some_and(|p| now <= p.payload.expires_unix) {
            return Err("busy".into());
        }
        let (answer, rx) = channel();
        self.pending = Some(Pending { payload, answer });
        Ok(rx)
    }

    /// The request awaiting an answer, if it has not expired.
    pub fn current(&self, now: u64) -> Option<Payload> {
        self.pending.as_ref().map(|p| p.payload.clone()).filter(|p| now <= p.expires_unix)
    }

    /// The user's answer. Accepted only for the current, unexpired id; anything else is an error and
    /// changes nothing. The answer is sent while the caller holds the slot's lock, so a handler that
    /// fails to `withdraw` finds it already in its receiver.
    pub fn answer(&mut self, id: &str, approve: bool, now: u64) -> Result<(), String> {
        match &self.pending {
            Some(p) if p.payload.id == id && now <= p.payload.expires_unix => {}
            Some(p) if p.payload.id == id => return Err("expired".into()),
            _ => return Err("no_such_request".into()),
        }
        let p = self.pending.take().expect("checked above");
        if !approve {
            self.declined.push((Key::of(&p.payload), now));
        }
        // The handler may have stopped listening (client gone); then nothing runs either way.
        let _ = p.answer.send(approve);
        Ok(())
    }

    /// Denies the pending request, whatever its id, as if the user had clicked Deny: the window
    /// was hidden, so nobody can see the dialog any more. True when there was one.
    pub fn deny_current(&mut self, now: u64) -> bool {
        let Some(p) = self.pending.take() else { return false };
        self.declined.push((Key::of(&p.payload), now));
        let _ = p.answer.send(false);
        true
    }

    /// Withdraws a request on timeout (`timed_out`, which starts the cooldown) or when the client
    /// left. True when it was still pending; false when an answer already took it (the answer is
    /// then in the receiver).
    pub fn withdraw(&mut self, id: &str, now: u64, timed_out: bool) -> bool {
        match self.pending.take() {
            Some(p) if p.payload.id == id => {
                if timed_out {
                    self.declined.push((Key::of(&p.payload), now));
                }
                true
            }
            other => {
                self.pending = other;
                false
            }
        }
    }
}

/// What `handle` needs from the running Konak: `lib.rs` implements it over the app handle and the
/// pipe connection.
pub trait Host {
    /// The saved state, checked against `req` (`pipe::validate`).
    fn validate(&self, req: &Request) -> Result<Action, String>;
    /// True while an operation on app `id` is in progress.
    fn busy(&self, id: &str) -> bool;
    fn slot(&self) -> &Mutex<Slot>;
    /// Wall-clock seconds, as the slot counts them.
    fn now(&self) -> u64;
    /// False once the client has hung up.
    fn connected(&self) -> bool;
    /// The request has its dialog: log it and show it.
    fn shown(&self, payload: &Payload);
    /// The request ended without approval.
    fn closed(&self, id: &str);
    /// The user approved the request; it is checked again before it runs.
    fn approved(&self, payload: &Payload);
    /// Runs the approved, re-checked action and waits for its result.
    fn run(&self, action: &Action) -> Reply;
}

fn reply(outcome: Outcome, error: impl Into<String>) -> Reply {
    Reply::new(outcome, Some(error.into()))
}

/// Withdraws request `id` if the handler unwinds while it waits, so a panic in the host does not
/// leave the slot taken until the request expires.
struct WithdrawOnUnwind<'a, H: Host>(&'a H, &'a str);

impl<H: Host> Drop for WithdrawOnUnwind<'_, H> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let now = self.0.now();
            self.0.slot().lock().unwrap_or_else(PoisonError::into_inner).withdraw(self.1, now, false);
        }
    }
}

/// One parsed pipe request: validate it, ask the user, and run it only if they approved this exact
/// request and it is still the same request now. `requester` is the client program's path, for the
/// dialog; `answer_within` is how long the user has to answer. Also returns the confirmation id once
/// the request got one.
pub fn handle(host: &impl Host, req: &Request, requester: Option<&str>, answer_within: Duration) -> (Reply, Option<String>) {
    use Outcome::{Busy, Denied, Failed, Invalid};
    let action = match host.validate(req) {
        Ok(a) => a,
        Err(e) => return (reply(Invalid, e), None),
    };
    if host.busy(&action.id) {
        return (reply(Busy, format!("{} has an operation in progress", action.id)), None);
    }
    let id = match new_id() {
        Ok(id) => id,
        Err(e) => return (reply(Failed, e), None),
    };
    let now = host.now();
    let payload = Payload {
        id: id.clone(),
        requester: requester.and_then(|p| std::path::Path::new(p).file_name()).map(|f| f.to_string_lossy().into_owned()),
        op: action.op.as_str(),
        app: action.id.clone(),
        name: action.name.clone(),
        delete_data: action.delete_data,
        expires_unix: now + ANSWER_TIMEOUT_S,
    };
    let answer = match host.slot().lock().unwrap().open(payload.clone(), now) {
        Ok(rx) => rx,
        Err(e) if e == DECLINED_RECENTLY => {
            let msg = format!("the person declined this same request in the Konak window less than {} minutes ago; nothing was changed. Ask them before trying again", COOLDOWN_S / 60);
            return (reply(Denied, msg), None);
        }
        Err(_) => return (reply(Busy, "another request is waiting for an answer in the Konak window"), None),
    };
    let guard = WithdrawOnUnwind(host, &id);
    host.shown(&payload);
    let approved = wait_for_answer(host, &id, &answer, Instant::now() + answer_within);
    drop(guard);
    if !approved {
        host.closed(&id);
        return (reply(Denied, "the person did not approve this in the Konak window (or did not answer within 5 minutes); nothing was changed"), Some(id));
    }
    host.approved(&payload);
    // The apps may have changed while the dialog was open.
    match host.validate(req) {
        Ok(current) if current == action => {}
        Ok(_) => return (reply(Failed, "the app changed while the request waited; nothing was done"), Some(id)),
        Err(e) => return (reply(Failed, format!("{e}; nothing was done")), Some(id)),
    }
    (host.run(&action), Some(id))
}

/// True only when the user approved request `id` in the window. A denial, no answer by `deadline`,
/// and the client hanging up all give false.
fn wait_for_answer(host: &impl Host, id: &str, answer: &Receiver<bool>, deadline: Instant) -> bool {
    loop {
        match answer.recv_timeout(Duration::from_millis(250)) {
            Ok(approve) => return approve,
            Err(RecvTimeoutError::Disconnected) => return false,
            Err(RecvTimeoutError::Timeout) if Instant::now() < deadline && host.connected() => {}
            Err(RecvTimeoutError::Timeout) => {
                // Give up, unless an answer took the request first: it was sent under this lock.
                // Only the timeout counts as a denial for the cooldown, not a client that left.
                let mut slot = host.slot().lock().unwrap();
                if slot.withdraw(id, host.now(), Instant::now() >= deadline) {
                    return false;
                }
                drop(slot);
                return answer.try_recv().unwrap_or(false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(id: &str, expires_unix: u64) -> Payload {
        Payload { id: id.into(), requester: Some("konak.exe".into()), op: "remove", app: "jellyfin".into(), name: None, delete_data: true, expires_unix }
    }

    #[test]
    fn ids_are_random_hex() {
        let (a, b) = (new_id().unwrap(), new_id().unwrap());
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn only_the_current_id_is_answered_once() {
        let mut s = Slot::default();
        let rx = s.open(payload("a1", 100), 10).unwrap();
        assert_eq!(s.current(10).unwrap().id, "a1");
        // A second request while one is pending.
        assert_eq!(s.open(payload("b2", 100), 10).unwrap_err(), "busy");
        // Wrong id: error, nothing sent, still pending.
        assert_eq!(s.answer("b2", true, 10).unwrap_err(), "no_such_request");
        assert_eq!(s.answer("", true, 10).unwrap_err(), "no_such_request");
        assert!(rx.try_recv().is_err());
        assert!(s.current(10).is_some());
        // The right id: one answer, then the slot is free.
        s.answer("a1", false, 10).unwrap();
        assert!(!rx.try_recv().unwrap());
        assert!(s.current(10).is_none());
        // Double answer.
        assert_eq!(s.answer("a1", true, 10).unwrap_err(), "no_such_request");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn approval_reaches_only_its_receiver() {
        let mut s = Slot::default();
        let rx = s.open(payload("a1", 100), 10).unwrap();
        s.answer("a1", true, 50).unwrap();
        assert!(rx.try_recv().unwrap());
        assert!(!s.withdraw("a1", 50, true), "already answered");
    }

    #[test]
    fn expiry_and_withdraw() {
        let mut s = Slot::default();
        let rx = s.open(payload("a1", 100), 10).unwrap();
        assert!(s.current(101).is_none());
        assert_eq!(s.answer("a1", true, 101).unwrap_err(), "expired");
        assert!(rx.try_recv().is_err());
        // The handler withdraws on timeout; a late answer then finds nothing.
        assert!(!s.withdraw("other", 101, true), "only its own id");
        assert!(s.current(50).is_some());
        assert!(s.withdraw("a1", 101, true));
        assert!(!s.withdraw("a1", 101, true));
        assert_eq!(s.answer("a1", true, 50).unwrap_err(), "no_such_request");
        assert!(rx.try_recv().is_err());
        // A timeout starts the cooldown; a client that left does not.
        assert_eq!(s.open(payload("c3", 900), 102).unwrap_err(), DECLINED_RECENTLY);
        let mut s = Slot::default();
        let _rx = s.open(payload("a1", 100), 10).unwrap();
        assert!(s.withdraw("a1", 20, false));
        assert!(s.open(payload("b2", 400), 21).is_ok());
    }

    #[test]
    fn a_denied_request_is_refused_for_two_minutes() {
        let mut s = Slot::default();
        let _rx = s.open(payload("a1", 400), 100).unwrap();
        s.answer("a1", false, 100).unwrap();
        // The same request: refused without a dialog, until the cooldown ends.
        assert_eq!(s.open(payload("b2", 500), 101).unwrap_err(), DECLINED_RECENTLY);
        assert_eq!(s.open(payload("b2", 500), 100 + COOLDOWN_S - 1).unwrap_err(), DECLINED_RECENTLY);
        assert!(s.current(150).is_none(), "no dialog opened");
        // A different request is unaffected: other op, app, name or delete_data.
        for other in [
            Payload { op: "stop", ..payload("c1", 500) },
            Payload { app: "immich".into(), ..payload("c2", 500) },
            Payload { delete_data: false, ..payload("c3", 500) },
            Payload { name: Some("x".into()), ..payload("c4", 500) },
        ] {
            let id = other.id.clone();
            s.open(other, 150).unwrap();
            assert!(s.withdraw(&id, 150, false));
        }
        // An approval starts no cooldown.
        let _rx = s.open(payload("d1", 900), 100 + COOLDOWN_S).unwrap();
        s.answer("d1", true, 230).unwrap();
        assert!(s.open(payload("d2", 900), 231).is_ok());
    }

    #[test]
    fn hiding_the_window_denies_the_pending_request() {
        let mut s = Slot::default();
        assert!(!s.deny_current(10));
        let rx = s.open(payload("a1", 400), 10).unwrap();
        assert!(s.deny_current(20));
        assert!(!rx.recv().unwrap());
        assert!(s.current(20).is_none());
        assert_eq!(s.answer("a1", true, 21).unwrap_err(), "no_such_request");
        assert_eq!(s.open(payload("b2", 500), 21).unwrap_err(), DECLINED_RECENTLY);
    }

    use crate::pipe::Op;
    use std::cell::{Cell, RefCell};

    /// A `Host` with scripted answers that records what `handle` did.
    struct Fake {
        slot: Mutex<Slot>,
        /// What `validate` returns, call by call; the last one repeats.
        validations: RefCell<Vec<Result<Action, String>>>,
        busy: bool,
        connected: Cell<bool>,
        /// The user's answer the moment the dialog is shown; `None`: no answer.
        answer: Option<bool>,
        /// An approval that arrives just as the client hangs up (after the wait gave up on it).
        approve_on_hangup: bool,
        panic_on_shown: bool,
        shown: RefCell<Vec<Payload>>,
        closed: RefCell<Vec<String>>,
        approved: Cell<u32>,
        runs: RefCell<Vec<Action>>,
    }

    const NOW: u64 = 1000;

    fn action() -> Action {
        Action { op: Op::Remove, id: "jellyfin".into(), name: None, delete_data: true }
    }

    fn request() -> Request {
        Request { op: Op::Remove, app: "movies".into(), name: None, delete_data: Some(true) }
    }

    fn fake(answer: Option<bool>) -> Fake {
        Fake {
            slot: Mutex::default(),
            validations: RefCell::new(vec![Ok(action())]),
            busy: false,
            connected: Cell::new(true),
            answer,
            approve_on_hangup: false,
            panic_on_shown: false,
            shown: RefCell::default(),
            closed: RefCell::default(),
            approved: Cell::new(0),
            runs: RefCell::default(),
        }
    }

    impl Host for Fake {
        fn validate(&self, _: &Request) -> Result<Action, String> {
            let mut v = self.validations.borrow_mut();
            if v.len() > 1 { v.remove(0) } else { v[0].clone() }
        }
        fn busy(&self, _: &str) -> bool {
            self.busy
        }
        fn slot(&self) -> &Mutex<Slot> {
            &self.slot
        }
        fn now(&self) -> u64 {
            NOW
        }
        fn connected(&self) -> bool {
            if self.approve_on_hangup && self.connected.get() {
                let id = self.shown.borrow()[0].id.clone();
                self.slot.lock().unwrap().answer(&id, true, NOW).unwrap();
                self.connected.set(false);
            }
            self.connected.get()
        }
        fn shown(&self, payload: &Payload) {
            self.shown.borrow_mut().push(payload.clone());
            assert!(!self.panic_on_shown, "host failed");
            if let Some(approve) = self.answer {
                self.slot.lock().unwrap().answer(&payload.id, approve, NOW).unwrap();
            }
        }
        fn closed(&self, id: &str) {
            self.closed.borrow_mut().push(id.into());
        }
        fn approved(&self, _: &Payload) {
            self.approved.set(self.approved.get() + 1);
        }
        fn run(&self, action: &Action) -> Reply {
            self.runs.borrow_mut().push(action.clone());
            Reply::new(Outcome::Done, None)
        }
    }

    fn run(f: &Fake) -> (Reply, Option<String>) {
        handle(f, &request(), Some(r"C:\tools\claude.exe"), Duration::from_secs(60))
    }

    /// The same request as `run`'s again, as a new dialog would get it.
    fn reopen(f: &Fake) -> Result<Receiver<bool>, String> {
        let p = f.shown.borrow()[0].clone();
        f.slot.lock().unwrap().open(Payload { id: "again".into(), ..p }, NOW + 1)
    }

    #[test]
    fn a_denied_request_never_runs() {
        let f = fake(Some(false));
        let (r, id) = run(&f);
        assert_eq!(r.outcome, Outcome::Denied);
        assert!(f.runs.borrow().is_empty());
        assert_eq!(f.approved.get(), 0);
        assert_eq!(f.closed.borrow().as_slice(), [id.unwrap()]);
        assert_eq!(reopen(&f).unwrap_err(), DECLINED_RECENTLY);
    }

    #[test]
    fn a_timeout_never_runs_and_counts_as_a_denial() {
        let f = fake(None);
        let (r, id) = handle(&f, &request(), None, Duration::ZERO);
        assert_eq!(r.outcome, Outcome::Denied);
        assert!(f.runs.borrow().is_empty());
        assert_eq!(f.closed.borrow().as_slice(), [id.unwrap()]);
        assert!(f.slot.lock().unwrap().current(NOW).is_none(), "withdrawn");
        assert_eq!(reopen(&f).unwrap_err(), DECLINED_RECENTLY);
    }

    #[test]
    fn a_client_that_leaves_never_runs_and_starts_no_cooldown() {
        let f = fake(None);
        f.connected.set(false);
        let (r, _) = run(&f);
        assert_eq!(r.outcome, Outcome::Denied);
        assert!(f.runs.borrow().is_empty());
        assert!(f.slot.lock().unwrap().current(NOW).is_none(), "withdrawn");
        assert!(reopen(&f).is_ok());
    }

    #[test]
    fn an_approved_request_that_changed_meanwhile_never_runs() {
        let changed = Action { delete_data: false, ..action() };
        for second in [Ok(changed), Err("not_installed: jellyfin".to_string())] {
            let f = fake(Some(true));
            *f.validations.borrow_mut() = vec![Ok(action()), second];
            let (r, id) = run(&f);
            assert_eq!(r.outcome, Outcome::Failed);
            assert!(r.error.unwrap().contains("nothing was done"));
            assert!(id.is_some());
            assert!(f.runs.borrow().is_empty());
        }
    }

    #[test]
    fn an_approved_unchanged_request_runs_once_as_shown() {
        let f = fake(Some(true));
        let (r, id) = run(&f);
        assert_eq!(r, Reply::new(Outcome::Done, None));
        assert_eq!(f.runs.borrow().as_slice(), [action()]);
        assert_eq!(f.approved.get(), 1);
        assert!(f.closed.borrow().is_empty());
        let shown = &f.shown.borrow()[0];
        assert_eq!(Some(&shown.id), id.as_ref());
        assert_eq!((shown.op, shown.app.as_str(), shown.delete_data, shown.name.as_deref()), ("remove", "jellyfin", true, None));
        assert_eq!(shown.requester.as_deref(), Some("claude.exe"));
        assert_eq!(shown.expires_unix, NOW + ANSWER_TIMEOUT_S);
        // An approval starts no cooldown.
        assert!(reopen(&f).is_ok());
    }

    #[test]
    fn an_approval_that_beats_the_hang_up_still_counts() {
        let f = Fake { approve_on_hangup: true, ..fake(None) };
        run(&f);
        assert_eq!(f.runs.borrow().as_slice(), [action()]);
    }

    #[test]
    fn refused_requests_get_no_dialog() {
        let untouched = |f: &Fake| f.shown.borrow().is_empty() && f.runs.borrow().is_empty();
        // Invalid.
        let f = fake(Some(true));
        *f.validations.borrow_mut() = vec![Err("unknown_app: x".into())];
        assert_eq!(run(&f), (Reply::new(Outcome::Invalid, Some("unknown_app: x".into())), None));
        assert!(untouched(&f));
        // The app is busy.
        let f = Fake { busy: true, ..fake(Some(true)) };
        assert_eq!(run(&f).0.outcome, Outcome::Busy);
        assert!(f.slot.lock().unwrap().current(NOW).is_none());
        assert!(untouched(&f));
        // Another request waits for an answer.
        let f = fake(Some(true));
        let _rx = f.slot.lock().unwrap().open(Payload { op: "stop", ..payload("x1", NOW + 100) }, NOW).unwrap();
        assert_eq!(run(&f).0.outcome, Outcome::Busy);
        assert!(untouched(&f));
        // The same request was just denied: no second dialog.
        let f = fake(Some(false));
        run(&f);
        let (r, id) = run(&f);
        assert_eq!(r.outcome, Outcome::Denied);
        assert!(r.error.unwrap().contains(&format!("less than {} minutes ago", COOLDOWN_S / 60)));
        assert!(id.is_none());
        assert_eq!(f.shown.borrow().len(), 1);
        assert!(f.runs.borrow().is_empty());
    }

    #[test]
    fn a_panicking_host_frees_the_slot() {
        let f = Fake { panic_on_shown: true, ..fake(None) };
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&f))).is_err());
        assert!(f.slot.lock().unwrap().current(NOW).is_none());
        assert!(f.runs.borrow().is_empty());
        assert!(reopen(&f).is_ok(), "no cooldown either");
    }

    #[test]
    fn a_stale_entry_does_not_block_forever() {
        let mut s = Slot::default();
        let old = s.open(payload("a1", 100), 10).unwrap();
        let _new = s.open(payload("b2", 500), 101).unwrap();
        // The dropped entry's sender is gone: its receiver reads as "no approval".
        assert!(old.recv().is_err());
        assert_eq!(s.current(101).unwrap().id, "b2");
    }
}
