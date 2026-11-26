//! The one MCP write request waiting for the user's answer in the Konak window.
//!
//! The pipe handler opens a request and gets a receiver. The only sender lives inside the pending
//! entry, and the only code that sends on it is `Slot::answer`, which the `mcp_answer` command
//! calls with the id the window was shown. So a pipe request turns into an operation only when the
//! user answered that exact id with `approve = true`; every other path ends as a denial.

use serde::Serialize;
use std::sync::mpsc::{channel, Receiver, Sender};

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
