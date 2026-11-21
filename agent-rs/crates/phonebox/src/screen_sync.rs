//! Port of `ScreenSynchronizer.cs`: puts each phone's screen to sleep while the computer is locked and wakes it after,
//! once per change. Setup (a newly plugged-in phone) and the session lock monitor's polling are serialized; a phone
//! whose command failed stays pending for the next poll; a cancelled call never leaves an older state remembered.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::server::Stop;

/// Whether the computer is locked right now; None when that cannot be read (no GUI session).
pub type ReadLock = Arc<dyn Fn() -> Option<bool> + Send + Sync>;
pub type ApplyFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;
/// Puts one phone's screen to sleep (true) or wakes it (false); an Err is a failure for that phone.
pub type Apply = Arc<dyn Fn(String, bool) -> ApplyFuture + Send + Sync>;

#[derive(Debug, Clone, PartialEq)]
pub enum SyncError {
    /// The caller's stop signal ended the call.
    Cancelled,
    /// Some phones could not be synchronized (each failure's message); the others were.
    Failed(Vec<String>),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Cancelled => f.write_str("The operation was canceled."),
            // AggregateException's message
            SyncError::Failed(failures) => {
                f.write_str("Could not synchronize phone screens")?;
                for failure in failures {
                    write!(f, " ({failure})")?;
                }
                Ok(())
            }
        }
    }
}

pub struct ScreenSynchronizer {
    read_lock: ReadLock,
    apply: Apply,
    /// The state each phone was last put in; the lock is the gate that serializes calls.
    applied: tokio::sync::Mutex<HashMap<String, bool>>,
}

impl ScreenSynchronizer {
    pub fn new(read_lock: ReadLock, apply: Apply) -> ScreenSynchronizer {
        ScreenSynchronizer {
            read_lock,
            apply,
            applied: Default::default(),
        }
    }

    /// Brings every phone in `devices` to the computer's lock state (`force`: even one already in it). Ok(false) when
    /// the lock state cannot be read; with no phones, Ok(true).
    pub async fn reconcile(
        &self,
        devices: &[String],
        cancel: &Stop,
        force: bool,
    ) -> Result<bool, SyncError> {
        // a queued call that is cancelled changes nothing: what is forgotten is decided only once the gate is taken
        let mut applied = tokio::select! {
            biased;
            _ = cancel.stopped() => return Err(SyncError::Cancelled),
            applied = self.applied.lock() => applied,
        };
        applied.retain(|d, _| devices.contains(d));
        let mut available = true;
        let mut failures = Vec::new();
        for device in devices {
            // sampled per phone inside the gate, including during USB setup
            let Some(locked) = (self.read_lock)() else {
                available = false;
                applied.remove(device);
                continue;
            };
            if !force && applied.get(device) == Some(&locked) {
                continue;
            }
            let outcome = tokio::select! {
                biased;
                outcome = (self.apply)(device.clone(), locked) => Some(outcome),
                _ = cancel.stopped() => None,
            };
            match outcome {
                Some(Ok(())) => {
                    applied.insert(device.clone(), locked);
                }
                // cancelled mid-command: what the phone shows now is unknown, and the rest is not attempted
                None => {
                    applied.remove(device);
                    return Err(SyncError::Cancelled);
                }
                Some(Err(e)) => {
                    applied.remove(device);
                    failures.push(e);
                }
            }
        }
        if failures.is_empty() {
            Ok(available)
        } else {
            Err(SyncError::Failed(failures))
        }
    }

    /// Forgets what every phone was put in, so the next reconcile applies it again.
    pub async fn reset(&self, cancel: &Stop) -> Result<(), SyncError> {
        let mut applied = tokio::select! {
            biased;
            _ = cancel.stopped() => return Err(SyncError::Cancelled),
            applied = self.applied.lock() => applied,
        };
        applied.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    fn devs(names: &[&str]) -> Vec<String> {
        names.iter().map(|d| d.to_string()).collect()
    }

    /// A one-shot signal (C#'s TaskCompletionSource).
    #[derive(Clone)]
    struct Signal(Arc<tokio::sync::watch::Sender<bool>>);

    impl Signal {
        fn new() -> Signal {
            Signal(Arc::new(tokio::sync::watch::channel(false).0))
        }
        fn set(&self) {
            self.0.send_replace(true);
        }
        async fn wait(&self) {
            let mut rx = self.0.subscribe();
            let _ = rx.wait_for(|set| *set).await;
        }
    }

    async fn within<T>(f: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(5), f)
            .await
            .expect("timed out")
    }

    fn apply(f: impl Fn(String, bool) -> ApplyFuture + Send + Sync + 'static) -> Apply {
        Arc::new(f)
    }

    fn word(state: bool) -> &'static str {
        if state { "True" } else { "False" }
    }

    #[tokio::test]
    async fn failed_device_does_not_block_others_and_is_retried() {
        let calls: Arc<Mutex<Vec<String>>> = Default::default();
        let fail = Arc::new(AtomicBool::new(true));
        let sync = {
            let (calls, fail) = (calls.clone(), fail.clone());
            ScreenSynchronizer::new(
                Arc::new(|| Some(true)),
                apply(move |device, _| {
                    calls.lock().unwrap().push(device.clone());
                    let failed = device == "first" && fail.load(Ordering::SeqCst);
                    Box::pin(async move {
                        if failed {
                            Err("The operation has timed out.".to_string())
                        } else {
                            Ok(())
                        }
                    })
                }),
            )
        };
        let none = Stop::default();
        let both = devs(&["first", "second"]);
        assert!(matches!(
            sync.reconcile(&both, &none, false).await,
            Err(SyncError::Failed(_))
        ));
        assert_eq!(*calls.lock().unwrap(), ["first", "second"]);
        fail.store(false, Ordering::SeqCst);
        sync.reconcile(&both, &none, false).await.unwrap();
        assert_eq!(*calls.lock().unwrap(), ["first", "second", "first"]);
    }

    #[tokio::test]
    async fn setup_and_polling_are_serialized_and_recheck_the_latest_lock() {
        let locked = Arc::new(AtomicBool::new(false));
        let (entered, release) = (Signal::new(), Signal::new());
        let states: Arc<Mutex<Vec<bool>>> = Default::default();
        let sync = Arc::new({
            let (locked, states) = (locked.clone(), states.clone());
            let (entered, release) = (entered.clone(), release.clone());
            ScreenSynchronizer::new(
                Arc::new(move || Some(locked.load(Ordering::SeqCst))),
                apply(move |_, state| {
                    let first = {
                        let mut states = states.lock().unwrap();
                        states.push(state);
                        states.len() == 1
                    };
                    let (entered, release) = (entered.clone(), release.clone());
                    Box::pin(async move {
                        if first {
                            entered.set();
                            release.wait().await;
                        }
                        Ok(())
                    })
                }),
            )
        });
        let deadline = Stop::default();
        let setup = {
            let (sync, deadline) = (sync.clone(), deadline.clone());
            tokio::spawn(async move { sync.reconcile(&devs(&["phone"]), &deadline, true).await })
        };
        within(entered.wait()).await;
        locked.store(true, Ordering::SeqCst);
        let poll = {
            let (sync, deadline) = (sync.clone(), deadline.clone());
            tokio::spawn(async move { sync.reconcile(&devs(&["phone"]), &deadline, false).await })
        };
        release.set();
        within(setup).await.unwrap().unwrap();
        within(poll).await.unwrap().unwrap();
        assert_eq!(*states.lock().unwrap(), [false, true]);
        sync.reconcile(&devs(&["phone"]), &deadline, false)
            .await
            .unwrap();
        assert_eq!(states.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn unknown_session_never_wakes_the_phone_and_reset_reapplies_state() {
        let locked: Arc<Mutex<Option<bool>>> = Default::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let sync = {
            let (locked, calls) = (locked.clone(), calls.clone());
            ScreenSynchronizer::new(
                Arc::new(move || *locked.lock().unwrap()),
                apply(move |_, _| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async { Ok(()) })
                }),
            )
        };
        let none = Stop::default();
        let phone = devs(&["phone"]);
        assert_eq!(sync.reconcile(&phone, &none, false).await, Ok(false));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        *locked.lock().unwrap() = Some(true);
        assert_eq!(sync.reconcile(&phone, &none, false).await, Ok(true));
        sync.reset(&none).await.unwrap();
        sync.reconcile(&phone, &none, false).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// `ScreenCancellationFixture.Run`: the fake applies the remote side effect, then holds the acknowledgment until
    /// the call is cancelled. Returns (the phone's state, the calls, whether the cancellation reached the caller).
    async fn cancelled_acknowledgment(initial_locked: bool) -> (bool, usize, bool) {
        let locked = Arc::new(AtomicBool::new(initial_locked));
        let phone = Arc::new(AtomicBool::new(initial_locked));
        let calls = Arc::new(AtomicUsize::new(0));
        let (entered, release) = (Signal::new(), Signal::new());
        let sync = Arc::new({
            let (locked, phone, calls) = (locked.clone(), phone.clone(), calls.clone());
            let (entered, release) = (entered.clone(), release.clone());
            ScreenSynchronizer::new(
                Arc::new(move || Some(locked.load(Ordering::SeqCst))),
                apply(move |_, state| {
                    let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                    phone.store(state, Ordering::SeqCst);
                    let (entered, release) = (entered.clone(), release.clone());
                    Box::pin(async move {
                        if n == 2 {
                            entered.set();
                            release.wait().await;
                        }
                        Ok(())
                    })
                }),
            )
        });
        let none = Stop::default();
        sync.reconcile(&devs(&["phone"]), &none, false)
            .await
            .unwrap();
        locked.store(!initial_locked, Ordering::SeqCst);
        let stop = Stop::default();
        let pending = {
            let (sync, stop) = (sync.clone(), stop.clone());
            tokio::spawn(async move { sync.reconcile(&devs(&["phone"]), &stop, false).await })
        };
        within(entered.wait()).await;
        stop.stop();
        let propagated = within(pending).await.unwrap() == Err(SyncError::Cancelled);
        locked.store(initial_locked, Ordering::SeqCst);
        sync.reconcile(&devs(&["phone"]), &none, false)
            .await
            .unwrap();
        release.set();
        (
            phone.load(Ordering::SeqCst),
            calls.load(Ordering::SeqCst),
            propagated,
        )
    }

    #[tokio::test]
    async fn canceled_acknowledgment_does_not_preserve_an_older_applied_state() {
        for initial_locked in [false, true] {
            let (phone_locked, calls, propagated) = cancelled_acknowledgment(initial_locked).await;
            assert!(propagated, "{initial_locked}");
            assert_eq!(phone_locked, initial_locked);
            assert_eq!(calls, 3, "{initial_locked}");
        }
    }

    #[tokio::test]
    async fn cancellation_keeps_other_devices_confirmed() {
        let locked = Arc::new(AtomicBool::new(false));
        let calls: Arc<Mutex<Vec<String>>> = Default::default();
        let held = Arc::new(AtomicBool::new(false));
        let (entered, release) = (Signal::new(), Signal::new());
        let sync = Arc::new({
            let (locked, calls, held) = (locked.clone(), calls.clone(), held.clone());
            let (entered, release) = (entered.clone(), release.clone());
            ScreenSynchronizer::new(
                Arc::new(move || Some(locked.load(Ordering::SeqCst))),
                apply(move |device, state| {
                    calls
                        .lock()
                        .unwrap()
                        .push(format!("{device}:{}", word(state)));
                    let hold = device == "second" && state && !held.swap(true, Ordering::SeqCst);
                    let (entered, release) = (entered.clone(), release.clone());
                    Box::pin(async move {
                        if hold {
                            entered.set();
                            release.wait().await;
                        }
                        Ok(())
                    })
                }),
            )
        });
        let none = Stop::default();
        let both = devs(&["first", "second"]);
        sync.reconcile(&both, &none, false).await.unwrap();
        locked.store(true, Ordering::SeqCst);
        let stop = Stop::default();
        let pending = {
            let (sync, stop, both) = (sync.clone(), stop.clone(), both.clone());
            tokio::spawn(async move { sync.reconcile(&both, &stop, false).await })
        };
        within(entered.wait()).await;
        stop.stop();
        assert_eq!(within(pending).await.unwrap(), Err(SyncError::Cancelled));
        sync.reconcile(&both, &none, false).await.unwrap();
        release.set();
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "first:False",
                "second:False",
                "first:True",
                "second:True",
                "second:True"
            ]
        );
    }

    #[tokio::test]
    async fn canceling_a_queued_call_does_not_invalidate_an_unattempted_device() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (entered, release) = (Signal::new(), Signal::new());
        let sync = Arc::new({
            let calls = calls.clone();
            let (entered, release) = (entered.clone(), release.clone());
            ScreenSynchronizer::new(
                Arc::new(|| Some(false)),
                apply(move |_, _| {
                    let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                    let (entered, release) = (entered.clone(), release.clone());
                    Box::pin(async move {
                        if n == 2 {
                            entered.set();
                            release.wait().await;
                        }
                        Ok(())
                    })
                }),
            )
        });
        let none = Stop::default();
        sync.reconcile(&devs(&["phone"]), &none, false)
            .await
            .unwrap();
        let active = {
            let (sync, none) = (sync.clone(), none.clone());
            tokio::spawn(async move { sync.reconcile(&devs(&["phone"]), &none, true).await })
        };
        within(entered.wait()).await;
        let stop = Stop::default();
        let queued = {
            let (sync, stop) = (sync.clone(), stop.clone());
            tokio::spawn(async move { sync.reconcile(&devs(&["other"]), &stop, false).await })
        };
        stop.stop();
        assert_eq!(within(queued).await.unwrap(), Err(SyncError::Cancelled));
        release.set();
        within(active).await.unwrap().unwrap();
        sync.reconcile(&devs(&["phone"]), &none, false)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
