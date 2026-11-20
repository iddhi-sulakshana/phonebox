//! Port of `StateFrameCache.cs`: the live stream's "state" frame, built once for all phones that ask within a moment.

use std::sync::{Arc, Mutex};

pub type Build = Box<dyn Fn() -> serde_json::Value + Send + Sync>;
pub type Clock = Box<dyn Fn() -> i64 + Send + Sync>;

pub struct StateFrameCache {
    build: Build,
    clock: Clock,
    max_age_ms: i64,
    gate: Mutex<Frame>,
}

struct Frame {
    text: Option<Arc<str>>,
    built_at: i64,
    seq: i64,
    signal: i64,
    builds: usize,
}

impl StateFrameCache {
    /// A frame is reused while younger than `max_age_ms` (100 by default) and no event or poke happened since.
    pub fn new(build: Build, max_age_ms: i64, clock: Option<Clock>) -> Self {
        let start = std::time::Instant::now();
        let clock = clock.unwrap_or_else(|| Box::new(move || start.elapsed().as_millis() as i64));
        StateFrameCache {
            build,
            clock,
            max_age_ms,
            gate: Mutex::new(Frame {
                text: None,
                built_at: 0,
                seq: -1,
                signal: -1,
                builds: 0,
            }),
        }
    }

    /// The ready-to-write SSE block (`event: state` + data) for the state as of now.
    pub fn get(&self, event_seq: i64, signal: i64) -> Arc<str> {
        let mut f = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        let now = (self.clock)();
        match &f.text {
            Some(text)
                if event_seq == f.seq
                    && signal == f.signal
                    && now - f.built_at < self.max_age_ms =>
            {
                text.clone()
            }
            _ => {
                let json = crate::config::write::to_dotnet_json_compact(&(self.build)());
                let text: Arc<str> = format!("event: state\ndata: {json}\n\n").into();
                *f = Frame {
                    text: Some(text.clone()),
                    built_at: now,
                    seq: event_seq,
                    signal,
                    builds: f.builds + 1,
                };
                text
            }
        }
    }

    pub fn builds(&self) -> usize {
        self.gate.lock().unwrap_or_else(|e| e.into_inner()).builds
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

    fn counter_cache(max_age: Option<i64>) -> (StateFrameCache, Arc<AtomicI64>) {
        let clock = Arc::new(AtomicI64::new(1000));
        let n = Arc::new(AtomicI64::new(0));
        let c = clock.clone();
        let cache = StateFrameCache::new(
            Box::new(move || serde_json::json!({ "n": n.fetch_add(1, Ordering::SeqCst) + 1 })),
            max_age.unwrap_or(100),
            Some(Box::new(move || c.load(Ordering::SeqCst))),
        );
        (cache, clock)
    }

    #[test]
    fn streams_asking_together_share_one_build_and_a_frame_is_a_state_block() {
        let clock = Arc::new(AtomicI64::new(1000));
        let n = Arc::new(AtomicI64::new(0));
        let c = clock.clone();
        let cache = StateFrameCache::new(
            Box::new(
                move || serde_json::json!({ "n": n.fetch_add(1, Ordering::SeqCst) + 1, "host": "pc" }),
            ),
            400,
            Some(Box::new(move || c.load(Ordering::SeqCst))),
        );
        let a = cache.get(5, 0);
        clock.fetch_add(50, Ordering::SeqCst);
        let b = cache.get(5, 0);
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(cache.builds(), 1);
        assert_eq!(&*a, "event: state\ndata: {\"n\":1,\"host\":\"pc\"}\n\n");
    }

    #[test]
    fn an_old_frame_or_a_new_event_forces_a_rebuild() {
        let (cache, clock) = counter_cache(Some(400));
        cache.get(5, 0);
        clock.fetch_add(399, Ordering::SeqCst);
        cache.get(5, 0);
        assert_eq!(cache.builds(), 1);
        clock.fetch_add(1, Ordering::SeqCst);
        cache.get(5, 0);
        assert_eq!(cache.builds(), 2);
        cache.get(6, 0);
        assert_eq!(cache.builds(), 3);
        cache.get(6, 1);
        assert_eq!(cache.builds(), 4);
        cache.get(6, 1);
        assert_eq!(cache.builds(), 4);
    }

    #[test]
    fn a_default_frame_is_not_reused_past_about_a_hundred_milliseconds() {
        let (cache, clock) = counter_cache(None);
        cache.get(1, 0);
        clock.fetch_add(99, Ordering::SeqCst);
        cache.get(1, 0);
        assert_eq!(cache.builds(), 1);
        clock.fetch_add(1, Ordering::SeqCst);
        cache.get(1, 0);
        assert_eq!(cache.builds(), 2);
    }

    #[test]
    fn concurrent_streams_build_once() {
        let builds = Arc::new(AtomicUsize::new(0));
        let b = builds.clone();
        let cache = Arc::new(StateFrameCache::new(
            Box::new(move || {
                b.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(30));
                serde_json::json!({ "ok": true })
            }),
            5000,
            None,
        ));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let c = cache.clone();
                std::thread::spawn(move || {
                    c.get(1, 0);
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        assert_eq!(builds.load(Ordering::SeqCst), 1);
    }
}
