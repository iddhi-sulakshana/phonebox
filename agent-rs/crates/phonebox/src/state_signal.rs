//! Port of `StateSignal.cs`: "something the phone shows has just changed", so live streams send a state at once instead
//! of at the next whole second. (`MediaChanged` comes with the media monitor, phase 3.)

use std::sync::LazyLock;
use std::sync::atomic::{AtomicI64, Ordering};

use tokio::sync::broadcast;

static VERSION: AtomicI64 = AtomicI64::new(0);
static CHANNEL: LazyLock<broadcast::Sender<()>> = LazyLock::new(|| broadcast::channel(64).0);

/// Counts pokes; part of the key of the shared state frame.
pub fn version() -> i64 {
    VERSION.load(Ordering::SeqCst)
}

pub fn poke() {
    VERSION.fetch_add(1, Ordering::SeqCst);
    let _ = CHANNEL.send(()); // no receivers is fine
}

/// A receiver woken by every [`poke`]; a closed or lagging receiver never stops the others.
pub fn subscribe() -> broadcast::Receiver<()> {
    CHANNEL.subscribe()
}

/// Whether a media reading differs from the last one in a way the phone should see now (not just time passing).
pub fn media_changed(
    before: Option<&crate::media_monitor::MediaSnapshot>,
    after: Option<&crate::media_monitor::MediaSnapshot>,
) -> bool {
    let (Some(b), Some(a)) = (before, after) else {
        return before.is_some() || after.is_some();
    };
    if b.title != a.title
        || b.artist != a.artist
        || b.app != a.app
        || b.playing != a.playing
        || b.dur_ms != a.dur_ms
        || b.art_id != a.art_id
    {
        return true;
    }
    // a seek: the position moved by clearly more than the second or two of playback between two readings
    (a.pos_ms - b.pos_ms).abs() > 3000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poke_wakes_subscribers_and_bumps_the_version() {
        let mut rx = subscribe();
        let before = version();
        poke();
        poke();
        let mut woken = 0;
        while rx.try_recv().is_ok() {
            woken += 1;
        }
        assert!(woken >= 2, "woken {woken} times");
        assert!(version() >= before + 2);
    }

    #[test]
    fn a_subscriber_that_throws_does_not_stop_the_poke_or_the_caller() {
        let gone = subscribe();
        let mut good = subscribe();
        drop(gone); // a stream that just closed
        poke();
        assert!(good.try_recv().is_ok());
    }

    #[test]
    fn a_media_change_that_matters_pokes_but_the_ticking_position_does_not() {
        use crate::media_monitor::MediaSnapshot;
        let a = MediaSnapshot::new("Spotify", "One", "A", "", true, 1000, 200000, 1, true);
        let with = |f: &dyn Fn(&mut MediaSnapshot)| {
            let mut b = a.clone();
            f(&mut b);
            b
        };
        assert!(!media_changed(Some(&a), Some(&with(&|b| b.pos_ms = 2200)))); // just time passing
        assert!(media_changed(Some(&a), Some(&with(&|b| b.playing = false)))); // pause
        assert!(media_changed(
            Some(&a),
            Some(&with(&|b| {
                b.title = "Two".into();
                b.pos_ms = 0;
            }))
        )); // next track
        assert!(media_changed(Some(&a), Some(&with(&|b| b.pos_ms = 90_000)))); // a seek, not 1 s of playback
        assert!(media_changed(None, Some(&a)));
        assert!(media_changed(Some(&a), None));
        assert!(!media_changed(None, None));
        // the cover changed, or the duration became known
        assert!(media_changed(Some(&a), Some(&with(&|b| b.art_id = 2))));
        assert!(media_changed(Some(&a), Some(&with(&|b| b.dur_ms = 1))));
        assert!(media_changed(
            Some(&a),
            Some(&with(&|b| b.app = "VLC".into()))
        ));
    }
}
