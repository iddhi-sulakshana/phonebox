//! Port of `ApprovalStore.cs`: permission prompts waiting for an answer from the phone. A Claude Code hook posts the
//! request and waits; the phone shows Allow / Deny and posts the decision back. If no phone is around, or nobody
//! answers in time, the request ends without a decision and Claude Code asks in the terminal as usual.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::push_store::PushStore;
use crate::server::PhoneSeenState;

const CATEGORY: &str = "PhoneBox.Agent.ApprovalStore";

/// `tool` is what Claude Code wants to use (Bash, Edit, an MCP tool…), `detail` what it wants to do (the command, the
/// file, the URL…); `kind` is "permission" (Allow / Deny) or "question" (answered in the terminal).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Approval {
    pub id: String,
    pub source: String,
    pub session: String,
    pub project: String,
    pub tool: String,
    pub detail: String,
    pub description: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
    pub kind: String,
}

type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;
type Log = Arc<dyn Fn(&str) + Send + Sync>;

/// One outcome per request: the first of the phone's decision, the expiry and the terminal wins.
#[derive(Default)]
struct Outcome {
    settled: Mutex<Option<Option<String>>>,
    changed: tokio::sync::Notify,
}

struct Pending {
    approval: Approval,
    outcome: Arc<Outcome>,
}

pub struct ApprovalStore {
    push: Arc<PushStore>,
    phone: Arc<PhoneSeenState>,
    clock: Clock,
    log: Log,
    pending: Mutex<HashMap<String, Pending>>,
}

impl ApprovalStore {
    pub fn new(push: Arc<PushStore>, phone: Arc<PhoneSeenState>) -> Arc<ApprovalStore> {
        Self::with(
            push,
            phone,
            Arc::new(crate::server::now_ms),
            Arc::new(|m: &str| crate::log::info(CATEGORY, m)),
        )
    }

    /// With another clock (expiries and the list) and log (tests).
    pub fn with(
        push: Arc<PushStore>,
        phone: Arc<PhoneSeenState>,
        clock: Clock,
        log: Log,
    ) -> Arc<ApprovalStore> {
        Arc::new(ApprovalStore {
            push,
            phone,
            clock,
            log,
            pending: Mutex::new(HashMap::new()),
        })
    }

    /// Approvals are only offered to a phone that polled the agent in the last 5 seconds.
    pub fn phone_connected(&self) -> bool {
        crate::server::now_ms().saturating_sub(
            self.phone
                .last_seen
                .load(std::sync::atomic::Ordering::SeqCst),
        ) < 5000
    }

    /// The requests the phone can still answer; one past its expiry is left out even before it is cleaned up.
    pub fn snapshot(&self) -> Vec<Approval> {
        let now = (self.clock)();
        let mut list: Vec<Approval> = self
            .lock()
            .values()
            .map(|p| p.approval.clone())
            .filter(|a| a.expires_at > now)
            .collect();
        list.sort_by_key(|a| a.created_at);
        list
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Shows the request on the phone and waits for "allow" / "deny" / "skip"; None = no answer (the terminal asks).
    /// `aborted` is the hook process going away; dropping the returned future ends the request the same way.
    pub async fn ask(&self, a: Approval, aborted: impl Future<Output = ()>) -> Option<String> {
        if !self.phone_connected() || a.expires_at <= (self.clock)() {
            return None;
        }
        let outcome = Arc::new(Outcome::default());
        let id = a.id.clone();
        let line = format!(
            "approval {id}: {} in {} sent to the phone",
            a.tool, a.project
        );
        let expires_at = a.expires_at;
        self.lock().insert(
            id.clone(),
            Pending {
                approval: a,
                outcome: outcome.clone(),
            },
        );
        // the request ends however the wait ends, also when this future is dropped (the hook's curl went away)
        let _done = Done {
            store: self,
            id: &id,
            outcome: &outcome,
        };
        self.push
            .emit("approval", Some(serde_json::json!({ "id": id })));
        (self.log)(&line);
        // exactly the time left: decide refuses an answer from the expiry on, so waiting longer only delays the terminal
        let left = (expires_at - (self.clock)()).max(0) as u64;
        tokio::select! {
            _ = outcome.wait() => {}
            _ = tokio::time::sleep(std::time::Duration::from_millis(left)) => {}
            _ = aborted => {}
        }
        // the outcome is the one answer: a timeout or the terminal closes it with None, so a decision that lost the race
        // reports false; one that won before this line is returned even though the wait ended
        outcome.settle(None);
        outcome.get()
    }

    pub fn decide(&self, id: &str, decision: &str) -> bool {
        if !matches!(decision, "allow" | "deny" | "skip") {
            return false;
        }
        let outcome = {
            let pending = self.lock();
            let Some(p) = pending.get(id) else {
                return false;
            };
            if p.approval.expires_at <= (self.clock)() {
                return false;
            }
            p.outcome.clone()
        };
        // accepted only if it wins against the timeout and the terminal, and only then logged as such
        if !outcome.settle(Some(decision.to_string())) {
            return false;
        }
        (self.log)(&format!("approval {id}: {decision} from the phone"));
        true
    }
}

/// 6 random bytes as lower-case hex.
pub fn new_id() -> String {
    let mut bytes = [0u8; 6];
    getrandom::fill(&mut bytes).expect("the system random source");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Outcome {
    /// True when this value is the outcome (nothing settled it before).
    fn settle(&self, value: Option<String>) -> bool {
        let mut settled = self.settled.lock().unwrap_or_else(|e| e.into_inner());
        if settled.is_some() {
            return false;
        }
        *settled = Some(value);
        drop(settled);
        self.changed.notify_waiters();
        true
    }

    fn get(&self) -> Option<String> {
        self.settled
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .flatten()
    }

    async fn wait(&self) {
        loop {
            let notified = self.changed.notified();
            let mut notified = std::pin::pin!(notified);
            notified.as_mut().enable();
            if self
                .settled
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some()
            {
                return;
            }
            notified.await;
        }
    }
}

/// Ends a request: no answer if nothing answered yet, off the list, and the phone told.
struct Done<'a> {
    store: &'a ApprovalStore,
    id: &'a str,
    outcome: &'a Outcome,
}

impl Drop for Done<'_> {
    fn drop(&mut self) {
        self.outcome.settle(None);
        self.store.lock().remove(self.id);
        self.store
            .push
            .emit("approval-done", Some(serde_json::json!({ "id": self.id })));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::time::Duration;

    struct Fixture {
        _dir: tempfile::TempDir,
        push: Arc<PushStore>,
        phone: Arc<PhoneSeenState>,
        now: Arc<AtomicI64>,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let push = PushStore::new(&dir.path().join("events.json"));
        let phone = Arc::new(PhoneSeenState::default());
        phone.last_seen.store(i64::MAX, Ordering::SeqCst);
        Fixture {
            _dir: dir,
            push,
            phone,
            now: Arc::new(AtomicI64::new(100_000)),
        }
    }

    fn store(f: &Fixture, log: Log) -> Arc<ApprovalStore> {
        let now = f.now.clone();
        ApprovalStore::with(
            f.push.clone(),
            f.phone.clone(),
            Arc::new(move || now.load(Ordering::SeqCst)),
            log,
        )
    }

    fn quiet() -> Log {
        Arc::new(|_: &str| {})
    }

    fn request(f: &Fixture, expires: i64) -> Approval {
        Approval {
            id: "test".into(),
            source: "test".into(),
            session: "s".into(),
            project: "p".into(),
            tool: "tool".into(),
            detail: "detail".into(),
            description: None,
            created_at: f.now.load(Ordering::SeqCst),
            expires_at: expires,
            kind: "permission".into(),
        }
    }

    async fn listed(store: &ApprovalStore) {
        for _ in 0..200 {
            if !store.snapshot().is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the request never reached the list");
    }

    // With 100 ms left the request ends after those 100 ms (no one-second floor), and leaves the list at its expiry.
    #[tokio::test]
    async fn a_request_ends_at_its_expiry_and_is_not_listed_after_it() {
        let f = fixture();
        let s = store(&f, quiet());
        let (abort, aborted) = tokio::sync::oneshot::channel::<()>();
        let req = request(&f, f.now.load(Ordering::SeqCst) + 100);
        let asking = tokio::spawn({
            let s = s.clone();
            async move {
                s.ask(req, async {
                    let _ = aborted.await;
                })
                .await
            }
        });
        listed(&s).await;
        assert_eq!(s.snapshot().len(), 1);
        f.now.fetch_add(100, Ordering::SeqCst);
        assert!(s.snapshot().is_empty());
        let result = tokio::time::timeout(Duration::from_millis(800), asking).await;
        drop(abort);
        assert_eq!(result.expect("ended at its expiry").unwrap(), None);
    }

    // A decision that stalls after its checks while the terminal takes over must not be reported as accepted unless
    // the request returns it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_decision_is_accepted_only_if_the_request_returns_it() {
        let f = fixture();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let entered_tx = Mutex::new(entered_tx);
        let release_rx = Mutex::new(release_rx);
        let log: Log = Arc::new(move |m: &str| {
            if m.contains("from the phone") {
                let _ = entered_tx.lock().unwrap().send(());
                let _ = release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10));
            }
        });
        let s = store(&f, log);
        let (abort, aborted) = tokio::sync::oneshot::channel::<()>();
        let req = request(&f, f.now.load(Ordering::SeqCst) + 60_000);
        let asking = tokio::spawn({
            let s = s.clone();
            async move {
                s.ask(req, async {
                    let _ = aborted.await;
                })
                .await
            }
        });
        listed(&s).await;
        let decide = tokio::task::spawn_blocking({
            let s = s.clone();
            move || s.decide("test", "allow")
        });
        tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(10)))
            .await
            .unwrap()
            .expect("the decision reached its log line");
        let _ = abort.send(()); // the user answered in the terminal
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = release_tx.send(());
        let accepted = tokio::time::timeout(Duration::from_secs(10), decide)
            .await
            .unwrap()
            .unwrap();
        let returned = tokio::time::timeout(Duration::from_secs(10), asking)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(accepted, returned.as_deref() == Some("allow"));
    }

    #[tokio::test]
    async fn an_already_expired_request_is_not_offered_to_the_phone() {
        for delta in [-1, 0] {
            let f = fixture();
            let s = store(&f, quiet());
            let req = request(&f, f.now.load(Ordering::SeqCst) + delta);
            let result = s.ask(req, std::future::pending()).await;
            assert!(s.snapshot().is_empty());
            assert!(!s.decide("test", "allow"));
            assert_eq!(result, None);
            assert!(f.push.since(0).is_empty(), "nothing was shown");
        }
    }

    #[tokio::test]
    async fn a_decision_must_arrive_strictly_before_the_request_expires() {
        for delta in [-1, 0, 1] {
            let f = fixture();
            let s = store(&f, quiet());
            let (abort, aborted) = tokio::sync::oneshot::channel::<()>();
            let req = request(&f, 130_000);
            let asking = tokio::spawn({
                let s = s.clone();
                async move {
                    s.ask(req, async {
                        let _ = aborted.await;
                    })
                    .await
                }
            });
            listed(&s).await;
            f.now.store(130_000 + delta, Ordering::SeqCst);
            let accepted = s.decide("test", "allow");
            assert_eq!(accepted, delta < 0, "{delta}");
            if !accepted {
                let _ = abort.send(());
            }
            let result = tokio::time::timeout(Duration::from_secs(5), asking)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result.as_deref(), (delta < 0).then_some("allow"), "{delta}");
            assert!(s.snapshot().is_empty());
        }
    }

    #[tokio::test]
    async fn dropping_the_request_removes_it_and_refuses_a_late_decision() {
        let f = fixture();
        let s = store(&f, quiet());
        let req = request(&f, f.now.load(Ordering::SeqCst) + 60_000);
        let asking = tokio::spawn({
            let s = s.clone();
            async move { s.ask(req, std::future::pending()).await }
        });
        listed(&s).await;
        asking.abort(); // the hook's curl went away and the server dropped the handler
        let _ = asking.await;
        assert!(s.snapshot().is_empty());
        assert!(!s.decide("test", "allow"));
        let kinds: Vec<String> = f.push.since(0).into_iter().map(|e| e.kind).collect();
        assert_eq!(kinds, ["approval", "approval-done"]);
        assert_eq!(
            f.push.since(0)[1].data,
            Some(serde_json::json!({"id": "test"}))
        );
    }

    #[tokio::test]
    async fn no_phone_no_prompt() {
        let f = fixture();
        f.phone
            .last_seen
            .store(crate::server::now_ms() - 6000, Ordering::SeqCst);
        let s = store(&f, quiet());
        assert!(!s.phone_connected());
        assert_eq!(
            s.ask(request(&f, 200_000), std::future::pending()).await,
            None
        );
        assert!(f.push.since(0).is_empty());
        f.phone
            .last_seen
            .store(crate::server::now_ms(), Ordering::SeqCst);
        assert!(s.phone_connected());
    }

    #[tokio::test]
    async fn only_known_decisions_count_and_the_list_is_in_order() {
        let f = fixture();
        let s = store(&f, quiet());
        let mut second = request(&f, 200_000);
        second.id = "second".into();
        second.created_at = 100_001;
        let first = request(&f, 200_000);
        let a = tokio::spawn({
            let s = s.clone();
            async move { s.ask(second, std::future::pending()).await }
        });
        listed(&s).await;
        let b = tokio::spawn({
            let s = s.clone();
            async move { s.ask(first, std::future::pending()).await }
        });
        for _ in 0..200 {
            if s.snapshot().len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            s.snapshot()
                .iter()
                .map(|a| a.id.as_str())
                .collect::<Vec<_>>(),
            ["test", "second"]
        );
        assert!(!s.decide("test", "maybe"));
        assert!(!s.decide("nope", "allow"));
        assert!(s.decide("test", "skip"));
        assert!(!s.decide("test", "allow"), "decided once");
        assert_eq!(b.await.unwrap().as_deref(), Some("skip"));
        assert!(s.decide("second", "deny"));
        assert_eq!(a.await.unwrap().as_deref(), Some("deny"));
        let id = new_id();
        assert_eq!(id.len(), 12);
        assert!(
            id.bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }
}
