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
}
