//! Port of `PushStore.cs`: values pushed in from outside (scripts, CI, Home Assistant) through the webhook API, kept in
//! `data.json`, plus a short in-memory queue of events for the phone (notifications, page switches, refreshes).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

pub const MAX_BYTES: usize = 64 * 1024;

/// A pushed value; `data.json` holds these with .NET's default (PascalCase) names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PushItem {
    pub data: Value,
    pub at: i64,
    pub expires_at: Option<i64>,
}

/// kind: notify | goto | back | refresh | reload | approval | approval-done.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelEvent {
    pub seq: i64,
    pub kind: String,
    pub data: Option<Value>,
    pub at: i64,
}

const MAX_ITEMS: usize = 200;
const MAX_EVENTS: usize = 50;
const SAVE_DELAY_MS: u64 = 2000;
const MAX_SAVE_RETRY_MS: u64 = 30_000;

pub struct PushStore {
    data_path: PathBuf,
    items: Mutex<IndexMap<String, PushItem>>,
    events: Mutex<VecDeque<PanelEvent>>,
    seq: AtomicI64,
    version: AtomicI64,
    save_pending: AtomicBool,
    persist: Mutex<PersistState>,
    emitted: broadcast::Sender<()>,
    me: Weak<PushStore>,
}

struct PersistState {
    disposed: bool,
    retry_ms: u64,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl PushStore {
    /// `data_path`: where values are kept (data.json next to config.json).
    pub fn new(data_path: &Path) -> Arc<PushStore> {
        let data_path = std::path::absolute(data_path).unwrap_or_else(|_| data_path.to_path_buf());
        let mut items = IndexMap::new();
        if data_path.exists() {
            match crate::app_paths::read_all_text(&data_path)
                .map_err(|e| e.to_string())
                .and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string()))
            {
                // one record at a time: a damaged one (a null time, a wrong type) must not take the others with it
                Ok(Value::Object(map)) => {
                    for (key, value) in map {
                        if !valid_key(&key) || !value.is_object() {
                            continue;
                        }
                        if let Ok(item) = serde_json::from_value::<PushItem>(value) {
                            items.insert(key, item);
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => eprintln!("could not read {}: {e}", data_path.display()),
            }
        }
        Arc::new_cyclic(|me| PushStore {
            data_path,
            items: Mutex::new(items),
            events: Mutex::new(VecDeque::new()),
            seq: AtomicI64::new(0),
            version: AtomicI64::new(now_ms()),
            save_pending: AtomicBool::new(false),
            persist: Mutex::new(PersistState {
                disposed: false,
                retry_ms: SAVE_DELAY_MS,
            }),
            emitted: broadcast::channel(64).0,
            me: me.clone(),
        })
    }

    fn items(&self) -> MutexGuard<'_, IndexMap<String, PushItem>> {
        self.items.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Changes whenever a value is set, removed or expires; read every second, which is also when expired values go.
    pub fn version(&self) -> i64 {
        self.purge_expired();
        self.version.load(Ordering::SeqCst)
    }

    /// Last event number, so a freshly started phone does not replay old notifications.
    pub fn seq(&self) -> i64 {
        self.seq.load(Ordering::SeqCst)
    }

    pub fn snapshot(&self) -> IndexMap<String, PushItem> {
        self.purge_expired();
        let now = now_ms();
        self.items()
            .iter()
            .filter(|(_, v)| v.expires_at.is_none_or(|e| e > now))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub fn get(&self, key: &str) -> Option<PushItem> {
        self.snapshot().shift_remove(key)
    }

    /// An error message, or Ok.
    pub fn set(&self, key: &str, data: Value, ttl_sec: Option<i32>) -> Result<(), String> {
        if !valid_key(key) {
            return Err("key must be 1-64 characters: letters, digits, '.', '_' or '-'".into());
        }
        self.purge_expired(); // expired values do not count against the limit
        // {"ttl": 300, ...} inside the payload works too
        let ttl_sec = ttl_sec.or_else(|| {
            data.get("ttl")
                .and_then(Value::as_i64)
                .and_then(|t| i32::try_from(t).ok())
        });
        let now = now_ms();
        let item = PushItem {
            data,
            at: now,
            expires_at: ttl_sec.filter(|t| *t > 0).map(|t| now + t as i64 * 1000),
        };
        {
            let mut items = self.items();
            if !items.contains_key(key) && items.len() >= MAX_ITEMS {
                return Err(format!(
                    "too many keys (max {MAX_ITEMS}); delete some first"
                ));
            }
            items.insert(key.to_string(), item);
        }
        self.changed();
        Ok(())
    }

    pub fn remove(&self, key: &str) -> bool {
        let removed = self.items().shift_remove(key).is_some();
        if removed {
            self.changed();
        }
        removed
    }

    pub fn clear(&self) {
        self.items().clear();
        self.changed();
    }

    pub fn emit(&self, kind: &str, data: Option<Value>) -> PanelEvent {
        let event = {
            let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
            let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
            let event = PanelEvent {
                seq,
                kind: kind.to_string(),
                data,
                at: now_ms(),
            };
            events.push_back(event.clone());
            while events.len() > MAX_EVENTS {
                events.pop_front();
            }
            event
        };
        let _ = self.emitted.send(());
        event
    }

    /// Events after `seq`, at most a minute old (a phone that was away does not get a flood).
    pub fn since(&self, seq: i64) -> Vec<PanelEvent> {
        let cutoff = now_ms() - 60_000;
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|e| e.seq > seq && e.at > cutoff)
            .cloned()
            .collect()
    }

    /// Woken after every emitted event.
    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.emitted.subscribe()
    }

    /// Drops values whose TTL has passed; that is a change too, so phones reload and drop them as well.
    fn purge_expired(&self) {
        let now = now_ms();
        let removed = {
            let mut items = self.items();
            let before = items.len();
            items.retain(|_, v| v.expires_at.is_none_or(|e| e > now));
            items.len() != before
        };
        if removed {
            self.changed();
        }
    }

    fn changed(&self) {
        let now = now_ms();
        let _ = self
            .version
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |prev| {
                Some(now.max(prev + 1))
            });
        if self.save_pending.swap(true, Ordering::SeqCst) {
            return;
        }
        let state = self.persist.lock().unwrap_or_else(|e| e.into_inner());
        if !state.disposed {
            self.schedule(SAVE_DELAY_MS);
        }
    }

    fn schedule(&self, delay_ms: u64) {
        let me = self.me.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            if let Some(store) = me.upgrade() {
                let mut state = store.persist.lock().unwrap_or_else(|e| e.into_inner());
                if !state.disposed {
                    store.save_pending_locked(&mut state);
                }
            }
        });
    }

    /// Saves a pending change now (the agent is stopping); later timers do nothing.
    pub fn dispose(&self) {
        let mut state = self.persist.lock().unwrap_or_else(|e| e.into_inner());
        if state.disposed {
            return;
        }
        state.disposed = true;
        self.save_pending_locked(&mut state);
    }

    // timer writes and the final flush share the persist lock, so a stop waits for an active write
    fn save_pending_locked(&self, state: &mut PersistState) {
        if !self.save_pending.swap(false, Ordering::SeqCst) {
            return;
        }
        let result = (|| -> std::io::Result<()> {
            if let Some(dir) = self.data_path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = crate::app_paths::with_suffix(&self.data_path, ".tmp");
            crate::app_paths::write_all_text_private(
                &tmp,
                &serde_json::to_string(&self.snapshot()).expect("push values serialize"),
            )?;
            std::fs::rename(&tmp, &self.data_path)
        })();
        match result {
            Ok(()) => state.retry_ms = SAVE_DELAY_MS,
            Err(e) => {
                if !state.disposed {
                    self.save_pending.store(true, Ordering::SeqCst);
                    self.schedule(state.retry_ms);
                    state.retry_ms = (2 * state.retry_ms).min(MAX_SAVE_RETRY_MS);
                }
                eprintln!("could not save pushed data: {e}");
            }
        }
    }
}

impl Drop for PushStore {
    fn drop(&mut self) {
        self.dispose();
    }
}

/// `^[A-Za-z0-9_.\-]{1,64}$` (.NET's `$` also accepts a final line feed).
pub fn valid_key(key: &str) -> bool {
    let key = key.strip_suffix('\n').unwrap_or(key);
    (1..=64).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// "21.5" becomes a number, "true" a boolean, anything else a string.
pub fn parse_value(text: &str) -> Value {
    // NumberStyles.Float: surrounding white space (tab to carriage return, space), a sign, a point, an exponent
    let t = text.trim_matches(|c: char| matches!(c, '\t'..='\r' | ' '));
    let numeric = t
        .bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E'));
    if numeric
        && let Ok(d) = t.parse::<f64>()
        && d.is_finite()
    {
        return if d.fract() == 0.0 && d.abs() < 9_007_199_254_740_992.0 {
            Value::from(d as i64)
        } else {
            Value::from(d)
        };
    }
    match text {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => Value::String(text.to_string()),
    }
}

/// Reads a webhook body: JSON as-is (with a JSON content type), anything else as a trimmed string; `?value=` wins.
pub fn read_body(
    query_value: Option<&str>,
    content_type: Option<&str>,
    content_length: Option<u64>,
    body: &[u8],
) -> Result<Value, String> {
    if let Some(v) = query_value {
        return Ok(parse_value(v));
    }
    let too_large = || format!("body too large (max {} KB)", MAX_BYTES / 1024);
    if content_length.is_some_and(|n| n > MAX_BYTES as u64) || body.len() > MAX_BYTES {
        return Err(too_large());
    }
    let body = body.strip_prefix(b"\xef\xbb\xbf").unwrap_or(body);
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    if content_type.is_some_and(|c| c.to_ascii_lowercase().contains("json")) {
        return serde_json::from_str(if text.is_empty() { "null" } else { text })
            .map_err(|e| format!("invalid JSON: {e}"));
    }
    Ok(parse_value(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::time::Duration;

    // not-ported: AuditRegressionTests.PushStoreStillComesFromTheAgentsServices: ASP.NET dependency injection; main builds the one store and hands it to AppState
    // not-ported: TestPortsTests.ConcurrentFixturesKeepDistinctBoundListeners: tests the C# fixtures' port helper; Rust test servers bind port 0

    // StoreFollowUpTests: a Set that has to wait for a save in progress must not hold up every other Set behind it.
    #[test]
    fn a_set_waiting_for_a_save_does_not_block_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let store = PushStore::new(&dir.path().join("data.json"));
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        // a save in progress: the timer holds the persist lock
        let saver = {
            let store = store.clone();
            std::thread::spawn(move || {
                let _state = store.persist.lock().unwrap();
                held_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(Duration::from_secs(30));
            })
        };
        held_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        // waits for the save to arm its timer
        let first = {
            let store = store.clone();
            std::thread::spawn(move || store.set("first", parse_value("1"), None))
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !store.save_pending.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(store.save_pending.load(Ordering::SeqCst));
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        {
            let store = store.clone();
            std::thread::spawn(move || {
                done_tx
                    .send(store.set("second", parse_value("2"), None))
                    .unwrap()
            });
        }
        let second = done_rx.recv_timeout(Duration::from_secs(5));
        release_tx.send(()).unwrap();
        saver.join().unwrap();
        first.join().unwrap().unwrap();
        assert_eq!(
            second.expect("the second Set waited behind the first one's save"),
            Ok(())
        );
        assert_eq!(
            store.get("second").map(|i| i.data.to_string()).as_deref(),
            Some("2")
        );
    }

    // StoreFollowUpTests: a save that fails is retried (Rust writes the warning with eprintln, which cannot fail the
    // way the C# logger could)
    #[test]
    fn a_failed_save_is_retried_even_when_its_warning_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.json");
        std::fs::create_dir(&path).unwrap(); // the save cannot replace a folder
        let store = PushStore::new(&path);
        store.save_pending.store(true, Ordering::SeqCst); // a change waiting to be saved, no timer armed by the test
        {
            let mut state = store.persist.lock().unwrap();
            store.save_pending_locked(&mut state); // the timer callback, run here
        }
        assert!(store.save_pending.load(Ordering::SeqCst));
        std::fs::remove_dir(&path).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !path.is_file() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(path.is_file(), "the save was not retried");
    }

    // AuditRegressionTests (F3): an expired value changes the version once, so the phone drops it
    #[test]
    fn an_expired_value_changes_the_version_once_so_the_phone_drops_it() {
        let dir = tempfile::tempdir().unwrap();
        let s = PushStore::new(&dir.path().join("data.json"));
        let open = serde_json::json!("open");
        s.set("door", open.clone(), Some(1)).unwrap();
        s.set("keep", open.clone(), None).unwrap();
        s.set("renewed", open.clone(), Some(1)).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        s.set("renewed", serde_json::json!("still here"), None)
            .unwrap();
        let before = s.version();
        std::thread::sleep(Duration::from_millis(1100));
        let after = s.version();
        assert_ne!(before, after);
        assert_eq!(after, s.version());
        let snap = s.snapshot();
        assert!(!snap.contains_key("door"));
        assert!(snap.contains_key("keep"));
        assert_eq!(snap["renewed"].data, serde_json::json!("still here"));
    }

    // AuditRegressionTests (F4)
    #[test]
    fn expired_values_do_not_count_against_the_key_limit() {
        let dir = tempfile::tempdir().unwrap();
        let s = PushStore::new(&dir.path().join("data.json"));
        let open = serde_json::json!("open");
        for i in 0..200 {
            s.set(&format!("tmp-{i}"), open.clone(), Some(1)).unwrap();
        }
        assert!(s.set("new", open.clone(), None).is_err());
        std::thread::sleep(Duration::from_millis(1100));
        assert!(s.set("new", open, None).is_ok());
    }

    #[test]
    fn keys() {
        for (key, valid) in [
            ("temperature", true),
            ("build.main-ci_2", true),
            ("", false),
            ("has space", false),
            ("../etc", false),
        ] {
            assert_eq!(valid_key(key), valid, "{key}");
        }
    }

    #[test]
    fn values_are_typed() {
        assert!(parse_value("21.5").is_number());
        assert_eq!(parse_value("true"), Value::Bool(true));
        assert_eq!(parse_value("passed"), Value::String("passed".into()));
        assert_eq!(parse_value("NaN"), Value::String("NaN".into()));
    }

    fn capacity_run(
        initial: usize,
        writers: usize,
        replace: bool,
        shared_key: bool,
    ) -> (usize, usize) {
        let dir = tempfile::tempdir().unwrap();
        let store = PushStore::new(&dir.path().join("data.json"));
        for i in 0..initial {
            store
                .set(&format!("seed-{i}"), Value::from(1), None)
                .unwrap();
        }
        let barrier = Arc::new(Barrier::new(writers));
        let threads: Vec<_> = (0..writers)
            .map(|i| {
                let (s, b) = (store.clone(), barrier.clone());
                std::thread::spawn(move || {
                    let key = if replace {
                        format!("seed-{i}")
                    } else if shared_key {
                        "shared".into()
                    } else {
                        format!("writer-{i}")
                    };
                    b.wait();
                    s.set(&key, Value::from(1), None).is_ok()
                })
            })
            .collect();
        let accepted = threads
            .into_iter()
            .filter(|_| true)
            .map(|t| t.join().unwrap())
            .filter(|ok| *ok)
            .count();
        let count = store.snapshot().len();
        store.dispose();
        (accepted, count)
    }

    #[test]
    fn concurrent_new_keys_cannot_exceed_the_live_capacity() {
        assert_eq!(capacity_run(199, 8, false, false), (1, 200));
    }

    #[test]
    fn capacity_checks_preserve_free_slots_and_existing_keys() {
        for (initial, writers, replace, shared, accepted, count) in [
            (198, 8, false, false, 2, 200),
            (0, 8, false, false, 8, 8),
            (200, 4, false, false, 0, 200),
            (200, 4, true, false, 4, 200),
            (199, 8, false, true, 8, 200),
        ] {
            assert_eq!(
                capacity_run(initial, writers, replace, shared),
                (accepted, count),
                "{initial} {writers} {replace} {shared}"
            );
        }
    }

    #[test]
    fn a_partial_push_snapshot_keeps_its_valid_entries_without_breaking_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.json");
        std::fs::write(&path, r#"{"missing":null,"partial":{},"noTime":{"Data":"x","At":null},"text":"oops","valid":{"Data":"ready","At":1}}"#).unwrap();
        let store = PushStore::new(&path);
        let items = store.snapshot();
        assert_eq!(items.len(), 1);
        assert_eq!(items["valid"].data, "ready");
        assert!(serde_json::to_string(&items).unwrap().contains("ready"));
    }

    #[test]
    fn non_numeric_payload_ttl_remains_ordinary_data() {
        for ttl in ["\"later\"", "true", "null", "[]"] {
            let dir = tempfile::tempdir().unwrap();
            let store = PushStore::new(&dir.path().join("data.json"));
            let data: Value =
                serde_json::from_str(&format!(r#"{{"ttl":{ttl},"value":1}}"#)).unwrap();
            store.set("input", data.clone(), None).unwrap();
            assert_eq!(store.get("input").unwrap().expires_at, None, "{ttl}");
            assert_eq!(store.get("input").unwrap().data, data);
            store
                .set("numeric", serde_json::json!({ "ttl": 60 }), None)
                .unwrap();
            assert!(store.get("numeric").unwrap().expires_at.is_some());
            store.set("explicit", data, Some(60)).unwrap();
            assert!(store.get("explicit").unwrap().expires_at.is_some());
            store.clear();
            store.dispose();
        }
    }

    #[test]
    fn unknown_length_request_limit_counts_bytes() {
        for (symbol, count, rejected) in [
            ("界", 30000, true),
            ("a", 65536, false),
            ("a", 65537, true),
            ("界", 20000, false),
        ] {
            let text = symbol.repeat(count);
            let result = read_body(None, None, None, text.as_bytes());
            assert_eq!(result.is_err(), rejected, "{symbol} x {count}");
            if !rejected {
                assert_eq!(result.unwrap(), Value::String(text));
            }
        }
    }

    #[test]
    fn disposing_the_host_flushes_updates_still_waiting_for_the_timer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.json");
        {
            let store = PushStore::new(&path);
            store.set("deployment", parse_value("ready"), None).unwrap();
        } // the last reference goes: the pending save is written now
        let restarted = PushStore::new(&path);
        assert_eq!(restarted.get("deployment").unwrap().data, "ready");
    }

    #[test]
    fn a_plain_filename_persists_in_the_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("phonebox-push.json");
        {
            let store = PushStore::new(&path);
            store.set("deployment", parse_value("ready"), None).unwrap();
            store.dispose();
        }
        assert_eq!(
            PushStore::new(&path).get("deployment").unwrap().data,
            "ready"
        );
    }

    #[test]
    fn saving_retries_after_a_temporary_filesystem_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.json");
        std::fs::create_dir(&path).unwrap(); // a folder where the file belongs: the save fails
        let store = PushStore::new(&path);
        store.set("deployment", parse_value("ready"), None).unwrap();
        std::thread::sleep(Duration::from_millis(2500)); // first attempt (2 s) has failed
        assert_eq!(store.get("deployment").unwrap().data, "ready");
        std::fs::remove_dir(&path).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(6);
        while !path.is_file() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            path.is_file(),
            "the unchanged value should retry once the destination is available"
        );
        assert_eq!(
            PushStore::new(&path).get("deployment").unwrap().data,
            "ready"
        );
    }

    #[test]
    fn concurrent_updates_each_advance_the_version() {
        let dir = tempfile::tempdir().unwrap();
        let store = PushStore::new(&dir.path().join("data.json"));
        let original = store.version();
        let value = parse_value("sample");
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (s, v) = (store.clone(), value.clone());
                std::thread::spawn(move || {
                    (0..1000).for_each(|_| s.set("sample", v.clone(), None).unwrap())
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        assert!(
            store.version() - original >= 8000,
            "every successful write must advance the version"
        );
        store.dispose();
    }

    #[test]
    fn events_are_numbered_capped_and_only_recent_ones_replay() {
        let dir = tempfile::tempdir().unwrap();
        let store = PushStore::new(&dir.path().join("data.json"));
        let mut rx = store.subscribe();
        for i in 0..60 {
            store.emit("notify", Some(Value::from(i)));
        }
        assert!(rx.try_recv().is_ok());
        assert_eq!(store.seq(), 60);
        let all = store.since(0);
        assert_eq!(all.len(), 50);
        assert_eq!(all[0].seq, 11);
        assert_eq!(store.since(59).len(), 1);
        store.dispose();
    }
}
