//! `MediaArtworkCache`: refreshes delayed native thumbnails (Windows media sessions) without changing the art id for
//! identical image bytes, so the phone only reloads the cover when it really changed.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::media_monitor::AlbumArt;

/// Why a read gave nothing: the caller gave up (not counted against the thumbnail), or the read failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtError {
    Cancelled,
    Failed(String),
}

/// Reads the current thumbnail; it may watch the cancel flag while it waits.
pub type ArtRead<'a> = &'a mut dyn FnMut(&AtomicBool) -> Result<Option<AlbumArt>, ArtError>;

#[derive(Default)]
struct State {
    track: Option<String>,
    revision: i64,
    generation: i64,
    started: i64,
    last_read: i64,
    failed_at: i64,
    reading: bool,
    has_read: bool,
    failures: u32,
    current: Option<Arc<AlbumArt>>,
    id: i32,
}

pub struct MediaArtworkCache {
    gate: Mutex<State>,
    clock: Box<dyn Fn() -> i64 + Send + Sync>,
}

impl Default for MediaArtworkCache {
    fn default() -> Self {
        Self::new()
    }
}

/// How long to wait after this many failed reads in a row: the first one is retried at once (a thumbnail that isn't
/// ready yet), then the pause doubles from a second up to 30 s, so one that is always too large or unreadable isn't
/// reopened on every poll.
fn failure_backoff(failures: u32) -> i64 {
    if failures < 2 {
        0
    } else {
        30_000.min(1000i64 << (failures - 2).min(5))
    }
}

/// Monotonic milliseconds (`Environment.TickCount64` in the C# agent).
fn monotonic_ms() -> i64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as i64
}

impl MediaArtworkCache {
    /// On the monotonic clock.
    pub fn new() -> Self {
        Self::with_clock(monotonic_ms)
    }

    pub fn with_clock(clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        MediaArtworkCache {
            gate: Mutex::new(State::default()),
            clock: Box::new(clock),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.gate.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn current(&self) -> Option<Arc<AlbumArt>> {
        self.lock().current.clone()
    }

    pub fn id(&self) -> i32 {
        self.lock().id
    }

    /// Reads the thumbnail of `track` when it is due: at once for a new track or revision (a metadata event), every
    /// second while a new track's image settles (8 s), then every 30 s; failures back off.
    pub fn update(
        &self,
        track: &str,
        revision: i64,
        read: Option<ArtRead>,
        cancel: &AtomicBool,
    ) -> Result<(), ArtError> {
        use std::sync::atomic::Ordering::SeqCst;
        if cancel.load(SeqCst) {
            return Err(ArtError::Cancelled);
        }
        let (generation, sample_time) = {
            let mut st = self.lock();
            let now = (self.clock)();
            if st.track.as_deref() != Some(track) || st.revision != revision {
                if st.track.as_deref() != Some(track) {
                    st.current = None;
                }
                st.track = Some(track.to_string());
                st.revision = revision;
                st.generation += 1;
                st.started = now;
                st.has_read = false;
                st.reading = false;
                st.failures = 0;
            }
            // Chromium may publish the previous video's thumbnail before the new one arrives
            let interval = if now - st.started <= 8000 {
                1000
            } else {
                30000
            };
            if read.is_none()
                || st.reading
                || (st.has_read && now - st.last_read < interval)
                || (st.failures > 0 && now - st.failed_at < failure_backoff(st.failures))
            {
                return Ok(());
            }
            st.reading = true;
            (st.generation, now)
        };
        let result = match read {
            Some(read) => read(cancel),
            None => Ok(None),
        };
        {
            let mut st = self.lock();
            let outcome = match result {
                Ok(_) if cancel.load(SeqCst) => Err(ArtError::Cancelled),
                Ok(art) => {
                    if generation == st.generation {
                        match art.filter(|a| !a.bytes.is_empty()) {
                            None => Self::failed(&mut st, generation, sample_time),
                            Some(art) => {
                                st.has_read = true;
                                st.last_read = sample_time;
                                st.failures = 0;
                                if st.current.as_deref() != Some(&art) {
                                    st.current = Some(Arc::new(art));
                                    st.id += 1;
                                }
                            }
                        }
                    }
                    Ok(())
                }
                Err(ArtError::Cancelled) => Err(ArtError::Cancelled),
                Err(e) => {
                    Self::failed(&mut st, generation, sample_time);
                    Err(e)
                }
            };
            if generation == st.generation {
                st.reading = false;
            }
            outcome
        }
    }

    /// A read of the current track found nothing usable.
    fn failed(st: &mut State, generation: i64, sample_time: i64) {
        if generation != st.generation {
            return;
        }
        st.failures += 1;
        st.failed_at = sample_time;
    }

    pub fn clear(&self) {
        let mut st = self.lock();
        st.generation += 1;
        st.track = None;
        st.current = None;
        st.reading = false;
        st.has_read = false;
        st.failures = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    type R = Result<Option<AlbumArt>, ArtError>;

    fn image(value: u8) -> AlbumArt {
        AlbumArt {
            bytes: vec![value],
            mime: "image/png".into(),
        }
    }

    struct Clock(Arc<AtomicI64>);
    impl Clock {
        fn set(&self, v: i64) {
            self.0.store(v, Ordering::SeqCst);
        }
    }

    fn cache() -> (MediaArtworkCache, Clock) {
        let now = Arc::new(AtomicI64::new(0));
        let read = now.clone();
        (
            MediaArtworkCache::with_clock(move || read.load(Ordering::SeqCst)),
            Clock(now),
        )
    }

    fn never() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn bytes(c: &MediaArtworkCache) -> Vec<u8> {
        c.current().unwrap().bytes.clone()
    }

    #[test]
    fn late_changes_are_rechecked_without_an_event_and_stable_images_are_throttled() {
        let (cache, now) = cache();
        let mut reads = 0u8;
        let mut read = |_: &AtomicBool| -> R {
            reads += 1;
            Ok(Some(image(reads)))
        };
        cache.update("track", 0, Some(&mut read), &never()).unwrap();
        now.set(500);
        cache.update("track", 0, Some(&mut read), &never()).unwrap();
        now.set(9000);
        cache.update("track", 0, Some(&mut read), &never()).unwrap();
        now.set(30000);
        cache.update("track", 0, Some(&mut read), &never()).unwrap();
        assert_eq!(reads, 2);
        assert_eq!(bytes(&cache), [2]);
    }

    #[test]
    fn old_async_read_cannot_restore_artwork_after_track_change_or_clear() {
        for clear in [false, true] {
            let cache = Arc::new(MediaArtworkCache::new());
            let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let old = {
                let cache = cache.clone();
                std::thread::spawn(move || {
                    let mut read = |_: &AtomicBool| -> R {
                        entered_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        Ok(Some(image(1)))
                    };
                    cache.update("old", 0, Some(&mut read), &never())
                })
            };
            entered_rx.recv().unwrap();
            if clear {
                cache.clear();
            } else {
                cache
                    .update(
                        "new",
                        0,
                        Some(&mut |_: &AtomicBool| Ok(Some(image(2)))),
                        &never(),
                    )
                    .unwrap();
            }
            release_tx.send(()).unwrap();
            old.join().unwrap().unwrap();
            if clear {
                assert!(cache.current().is_none());
            } else {
                assert_eq!(bytes(&cache), [2]);
            }
        }
    }

    #[test]
    fn missing_empty_and_failed_thumbnails_are_retried_and_cancellation_cannot_publish() {
        let (cache, now) = cache();
        cache.update("track", 0, None, &never()).unwrap();
        cache
            .update(
                "track",
                0,
                Some(&mut |_: &AtomicBool| {
                    Ok(Some(AlbumArt {
                        bytes: vec![],
                        mime: "image/png".into(),
                    }))
                }),
                &never(),
            )
            .unwrap();
        now.set(1000);
        assert_eq!(
            cache.update(
                "track",
                0,
                Some(&mut |_: &AtomicBool| Err(ArtError::Failed("io".into()))),
                &never()
            ),
            Err(ArtError::Failed("io".into()))
        );
        now.set(2000);
        cache
            .update(
                "track",
                0,
                Some(&mut |_: &AtomicBool| Ok(Some(image(2)))),
                &never(),
            )
            .unwrap();
        assert_eq!(bytes(&cache), [2]);
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            cache.update(
                "track",
                1,
                Some(&mut |_: &AtomicBool| Ok(Some(image(3)))),
                &cancelled
            ),
            Err(ArtError::Cancelled)
        );
        assert_eq!(bytes(&cache), [2]);
    }

    #[test]
    fn delayed_chromium_thumbnail_replaces_the_previous_videos_image() {
        let (cache, now) = cache();
        cache
            .update(
                "browser|first",
                0,
                Some(&mut |_: &AtomicBool| Ok(Some(image(1)))),
                &never(),
            )
            .unwrap();
        now.set(1000);
        cache
            .update(
                "browser|second",
                0,
                Some(&mut |_: &AtomicBool| Ok(Some(image(1)))),
                &never(),
            )
            .unwrap();
        let stale_id = cache.id();
        now.set(3000);
        cache
            .update(
                "browser|second",
                0,
                Some(&mut |_: &AtomicBool| Ok(Some(image(2)))),
                &never(),
            )
            .unwrap();
        assert_eq!(bytes(&cache), [2]);
        assert!(cache.id() > stale_id);
    }

    #[test]
    fn metadata_event_refreshes_artwork_for_an_unchanged_title() {
        let (cache, now) = cache();
        cache
            .update(
                "browser|live",
                0,
                Some(&mut |_: &AtomicBool| Ok(Some(image(1)))),
                &never(),
            )
            .unwrap();
        now.set(60_000);
        cache
            .update(
                "browser|live",
                1,
                Some(&mut |_: &AtomicBool| Ok(Some(image(2)))),
                &never(),
            )
            .unwrap();
        assert_eq!(bytes(&cache), [2]);
    }

    #[test]
    fn identical_artwork_does_not_change_its_cache_id() {
        let (cache, now) = cache();
        let mut read = |_: &AtomicBool| -> R { Ok(Some(image(1))) };
        cache.update("same", 0, Some(&mut read), &never()).unwrap();
        let id = cache.id();
        now.set(1000);
        cache.update("same", 0, Some(&mut read), &never()).unwrap();
        assert_eq!(cache.id(), id);
        assert!(cache.current().is_some());
    }

    #[test]
    fn cancelled_reads_neither_wipe_previous_track_artwork_nor_throttle_immediate_retry() {
        let (cache, now) = cache();
        let cache = Arc::new(cache);
        cache
            .update(
                "track-1",
                0,
                Some(&mut |_: &AtomicBool| Ok(Some(image(1)))),
                &never(),
            )
            .unwrap();

        let pre_cancelled = AtomicBool::new(true);
        assert_eq!(
            cache.update(
                "track-2",
                0,
                Some(&mut |_: &AtomicBool| Ok(Some(image(9)))),
                &pre_cancelled
            ),
            Err(ArtError::Cancelled)
        );
        assert_eq!(bytes(&cache), [1]);

        now.set(10_000);
        let mid_cancel = Arc::new(AtomicBool::new(false));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let inflight = {
            let (cache, mid_cancel) = (cache.clone(), mid_cancel.clone());
            std::thread::spawn(move || {
                let mut read = |cancel: &AtomicBool| -> R {
                    entered_tx.send(()).unwrap();
                    while !cancel.load(Ordering::SeqCst) {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(ArtError::Cancelled)
                };
                cache.update("track-1", 1, Some(&mut read), &mid_cancel)
            })
        };
        entered_rx.recv().unwrap();
        mid_cancel.store(true, Ordering::SeqCst);
        assert_eq!(inflight.join().unwrap(), Err(ArtError::Cancelled));

        now.set(10_050);
        cache
            .update(
                "track-1",
                1,
                Some(&mut |_: &AtomicBool| Ok(Some(image(2)))),
                &never(),
            )
            .unwrap();
        assert_eq!(bytes(&cache), [2]);
    }

    #[test]
    fn repeated_failures_back_off_and_a_track_change_resets_it() {
        let (cache, now) = cache();
        let reads = std::cell::Cell::new(0);
        let mut fail = |_: &AtomicBool| -> R {
            reads.set(reads.get() + 1);
            Ok(None)
        };
        cache.update("track", 0, Some(&mut fail), &never()).unwrap();
        now.set(100);
        cache.update("track", 0, Some(&mut fail), &never()).unwrap();
        now.set(500);
        cache.update("track", 0, Some(&mut fail), &never()).unwrap();
        now.set(1100);
        cache.update("track", 0, Some(&mut fail), &never()).unwrap();
        now.set(2000);
        cache.update("track", 0, Some(&mut fail), &never()).unwrap();
        assert_eq!(reads.get(), 3); // one immediate retry, then the pause grows
        now.set(3100);
        let mut thrown = 0;
        assert!(
            cache
                .update(
                    "track",
                    0,
                    Some(&mut |_: &AtomicBool| {
                        thrown += 1;
                        Err(ArtError::Failed("io".into()))
                    }),
                    &never()
                )
                .is_err()
        );
        assert_eq!(thrown, 1);
        now.set(3200);
        cache.update("track", 0, Some(&mut fail), &never()).unwrap();
        assert_eq!(reads.get(), 3); // an error counts as a failure too

        cache.update("next", 0, Some(&mut fail), &never()).unwrap();
        assert_eq!(reads.get(), 4);
        cache
            .update(
                "next",
                0,
                Some(&mut |_: &AtomicBool| Ok(Some(image(1)))),
                &never(),
            )
            .unwrap();
        assert_eq!(bytes(&cache), [1]);
    }

    #[test]
    fn cancellation_is_not_counted_as_a_failure() {
        let (cache, _now) = cache();
        let reads = std::cell::Cell::new(0);
        let cts = AtomicBool::new(false);
        assert_eq!(
            cache.update(
                "track",
                0,
                Some(&mut |cancel: &AtomicBool| {
                    reads.set(reads.get() + 1);
                    cancel.store(true, Ordering::SeqCst);
                    Err(ArtError::Cancelled)
                }),
                &cts
            ),
            Err(ArtError::Cancelled)
        );
        let mut fail = |_: &AtomicBool| -> R {
            reads.set(reads.get() + 1);
            Ok(None)
        };
        cache.update("track", 0, Some(&mut fail), &never()).unwrap();
        cache.update("track", 0, Some(&mut fail), &never()).unwrap();
        assert_eq!(reads.get(), 3);
        cache.update("track", 0, Some(&mut fail), &never()).unwrap();
        assert_eq!(reads.get(), 3);
    }
}
