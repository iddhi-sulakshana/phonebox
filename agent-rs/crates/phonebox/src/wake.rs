//! Port of `Wake` (`PushStore.cs`): lets the webhook API (`POST /api/refresh`) wake the slow monitors (quotas,
//! GitHub, calendars, feeds) at once.

use std::sync::LazyLock;
use std::time::Duration;

use crate::server::Stop;

static WAKE: LazyLock<tokio::sync::watch::Sender<u64>> =
    LazyLock::new(|| tokio::sync::watch::channel(0).0);

/// Goes up on every [`now`], so a loop can tell a wake-up from its own timer.
pub fn count() -> u64 {
    *WAKE.borrow()
}

/// Ends every pending [`delay`].
pub fn now() {
    WAKE.send_modify(|count| *count += 1);
}

/// Like `sleep(d)`, but returns early on [`now`] or when the agent stops.
pub async fn delay(d: Duration, stop: &Stop) {
    let mut woken = WAKE.subscribe();
    tokio::select! {
        _ = tokio::time::sleep(d) => {}
        _ = woken.changed() => {}
        _ = stop.stopped() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wake_ends_every_delay_and_counts() {
        let stop = Stop::default();
        let before = count();
        let waits: Vec<_> = (0..2)
            .map(|_| {
                let stop = stop.clone();
                tokio::spawn(async move { delay(Duration::from_secs(60), &stop).await })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(100)).await;
        now();
        for w in waits {
            tokio::time::timeout(Duration::from_secs(5), w)
                .await
                .expect("woken")
                .unwrap();
        }
        assert!(count() > before);

        // a stop ends a delay too, and a short delay ends by itself
        let stopped = Stop::default();
        stopped.stop();
        tokio::time::timeout(
            Duration::from_secs(5),
            delay(Duration::from_secs(60), &stopped),
        )
        .await
        .expect("stopped");
        tokio::time::timeout(
            Duration::from_secs(5),
            delay(Duration::from_millis(10), &stop),
        )
        .await
        .expect("timer");
    }
}
