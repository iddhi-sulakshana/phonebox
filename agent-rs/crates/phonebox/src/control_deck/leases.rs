//! `ControlDeck.Leases.cs`: push-to-talk holds on a microphone, and the delayed single-use power confirmation.

use std::collections::HashMap;
use std::sync::Mutex;

/// How long a hold lasts without a renewal, and the longest a hold can be renewed for.
pub const LEASE_MS: i64 = 3000;
pub const MAX_HOLD_MS: i64 = 60_000;
const MAX_LEASES: usize = 32;

type Read = Box<dyn Fn(&str) -> Option<bool> + Send + Sync>;
type Write = Box<dyn Fn(&str, bool) -> bool + Send + Sync>;
type Now = Box<dyn Fn() -> i64 + Send + Sync>;

struct Lease {
    device: String,
    started: i64,
    expires: i64,
}

#[derive(Default)]
struct Held {
    leases: HashMap<String, Lease>,
    /// Each held microphone's muted state before the first hold.
    original: HashMap<String, bool>,
}

/// Holds that unmute a microphone and mute it back when the last one ends or expires.
pub struct MicrophoneLeases {
    read: Read,
    write: Write,
    now: Now,
    held: Mutex<Held>,
}

impl MicrophoneLeases {
    /// `read(device)`: muted, or None when unknown; `write(device, muted)`: whether it took.
    pub fn new(
        read: impl Fn(&str) -> Option<bool> + Send + Sync + 'static,
        write: impl Fn(&str, bool) -> bool + Send + Sync + 'static,
        now: impl Fn() -> i64 + Send + Sync + 'static,
    ) -> MicrophoneLeases {
        MicrophoneLeases {
            read: Box::new(read),
            write: Box::new(write),
            now: Box::new(now),
            held: Mutex::new(Held::default()),
        }
    }

    /// A new hold that unmutes `device`: its id, or None (too many holds, unknown state, the unmute failed).
    pub fn begin(&self, device: &str) -> Option<String> {
        let mut held = self.lock();
        self.sweep_locked(&mut held, false);
        if held.leases.len() >= MAX_LEASES {
            return None;
        }
        if !held.original.contains_key(device) {
            let original = (self.read)(device)?;
            held.original.insert(device.to_string(), original);
        }
        if !(self.write)(device, false) {
            self.restore_locked(&mut held, device);
            return None;
        }
        let id = nonce();
        let now = (self.now)();
        held.leases.insert(
            id.clone(),
            Lease {
                device: device.to_string(),
                started: now,
                expires: now + LEASE_MS,
            },
        );
        Some(id)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Held> {
        self.held.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Extends a hold by [`LEASE_MS`], never past [`MAX_HOLD_MS`] from its start.
    pub fn renew(&self, id: &str) -> bool {
        let mut held = self.lock();
        self.sweep_locked(&mut held, false);
        let now = (self.now)();
        match held.leases.get_mut(id) {
            Some(lease) if now - lease.started < MAX_HOLD_MS => {
                lease.expires = (now + LEASE_MS).min(lease.started + MAX_HOLD_MS);
                true
            }
            _ => false,
        }
    }

    pub fn end(&self, id: &str) -> bool {
        let mut held = self.lock();
        let Some(lease) = held.leases.remove(id) else {
            return false;
        };
        self.restore_locked(&mut held, &lease.device);
        true
    }

    /// Ends expired holds (`all`: every hold, the phone went away) and restores microphones no hold keeps open.
    pub fn sweep(&self, all: bool) {
        let mut held = self.lock();
        self.sweep_locked(&mut held, all);
    }

    fn sweep_locked(&self, held: &mut Held, all: bool) {
        let now = (self.now)();
        held.leases.retain(|_, l| !all && l.expires > now);
        let devices: Vec<String> = held.original.keys().cloned().collect();
        for device in devices {
            self.restore_locked(held, &device);
        }
    }

    /// Puts `device` back as it was when no hold keeps it open; a failed write keeps the original for the next sweep.
    fn restore_locked(&self, held: &mut Held, device: &str) {
        if held.leases.values().any(|l| l.device == device) {
            return;
        }
        if let Some(&original) = held.original.get(device)
            && (self.write)(device, original)
        {
            held.original.remove(device);
        }
    }

    /// Forgets every hold and original state (panic mute: a microphone is never reopened).
    pub fn abandon_all(&self) {
        let mut held = self.lock();
        held.leases.clear();
        held.original.clear();
    }
}

/// The phone arms a power action, then fires it 1.5–6 s later with the nonce it got.
pub struct PowerConfirmations {
    now: Now,
    pending: Mutex<HashMap<String, (String, i64)>>,
}

impl PowerConfirmations {
    pub fn new(now: impl Fn() -> i64 + Send + Sync + 'static) -> PowerConfirmations {
        PowerConfirmations {
            now: Box::new(now),
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// A nonce for `action`, or "Too many pending confirmations".
    pub fn arm(&self, action: &str) -> Result<String, String> {
        let now = (self.now)();
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.retain(|_, (_, at)| now - *at <= 6000);
        if pending.len() >= 64 {
            return Err("Too many pending confirmations".into());
        }
        let id = nonce();
        pending.insert(id.clone(), (action.to_string(), now));
        Ok(id)
    }

    /// True once, for the same action, between 1.5 and 6 s after it was armed.
    pub fn consume(&self, nonce: &str, action: &str) -> bool {
        let now = (self.now)();
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        match pending.get(nonce) {
            Some((armed, at)) if armed == action && (1500..=6000).contains(&(now - at)) => {
                pending.remove(nonce);
                true
            }
            _ => false,
        }
    }
}

/// `Guid.NewGuid().ToString("N")`.
pub(crate) fn nonce() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the system random source");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

    fn mic(muted: bool, time: &Arc<AtomicI64>) -> (Arc<AtomicBool>, MicrophoneLeases) {
        let state = Arc::new(AtomicBool::new(muted));
        let (r, w, t) = (state.clone(), state.clone(), time.clone());
        let leases = MicrophoneLeases::new(
            move |_| Some(r.load(Ordering::SeqCst)),
            move |_, value| {
                w.store(value, Ordering::SeqCst);
                true
            },
            move || t.load(Ordering::SeqCst),
        );
        (state, leases)
    }

    #[test]
    fn push_to_talk_expires_and_restores_the_original_state() {
        let time = Arc::new(AtomicI64::new(0));
        let (muted, leases) = mic(true, &time);
        let id = leases.begin("microphone").unwrap();
        assert!(!muted.load(Ordering::SeqCst));
        time.store(2999, Ordering::SeqCst);
        leases.sweep(false);
        assert!(!muted.load(Ordering::SeqCst));
        time.store(3000, Ordering::SeqCst);
        leases.sweep(false);
        assert!(muted.load(Ordering::SeqCst));
        assert!(!leases.renew(&id));
    }

    #[test]
    fn overlapping_holds_do_not_restore_the_microphone_until_the_last_release() {
        let time = Arc::new(AtomicI64::new(0));
        let (muted, leases) = mic(true, &time);
        let a = leases.begin("microphone").unwrap();
        let b = leases.begin("microphone").unwrap();
        assert!(leases.end(&a));
        assert!(!muted.load(Ordering::SeqCst));
        assert!(leases.end(&b));
        assert!(muted.load(Ordering::SeqCst));
    }

    #[test]
    fn a_disconnect_does_not_mute_a_previously_unmuted_microphone() {
        let time = Arc::new(AtomicI64::new(0));
        let (muted, leases) = mic(false, &time);
        leases.begin("microphone");
        leases.sweep(true);
        assert!(!muted.load(Ordering::SeqCst));
    }

    #[test]
    fn renewals_cannot_keep_a_microphone_open_indefinitely() {
        let time = Arc::new(AtomicI64::new(0));
        let (muted, leases) = mic(true, &time);
        let id = leases.begin("microphone").unwrap();
        let mut t = 1000;
        while t < 60_000 {
            time.store(t, Ordering::SeqCst);
            assert!(leases.renew(&id), "{t}");
            t += 1000;
        }
        time.store(t, Ordering::SeqCst);
        assert!(!leases.renew(&id));
        assert!(muted.load(Ordering::SeqCst));
    }

    #[test]
    fn power_confirmation_is_delayed_bound_to_the_action_and_single_use() {
        let now = Arc::new(AtomicI64::new(0));
        let t = now.clone();
        let power = PowerConfirmations::new(move || t.load(Ordering::SeqCst));
        let nonce = power.arm("restart").unwrap();
        assert!(!power.consume(&nonce, "restart"));
        now.store(2000, Ordering::SeqCst);
        assert!(!power.consume(&nonce, "shutdown"));
        assert!(power.consume(&nonce, "restart"));
        assert!(!power.consume(&nonce, "restart"));
        let nonce = power.arm("sleep").unwrap();
        now.fetch_add(6001, Ordering::SeqCst);
        assert!(!power.consume(&nonce, "sleep"));
    }

    #[test]
    fn at_most_32_holds_and_64_pending_confirmations() {
        let time = Arc::new(AtomicI64::new(0));
        let (_muted, leases) = mic(true, &time);
        for _ in 0..32 {
            assert!(leases.begin("microphone").is_some());
        }
        assert!(leases.begin("microphone").is_none());
        let power = PowerConfirmations::new(|| 0);
        for _ in 0..64 {
            power.arm("restart").unwrap();
        }
        assert_eq!(
            power.arm("restart").unwrap_err(),
            "Too many pending confirmations"
        );
    }
}
