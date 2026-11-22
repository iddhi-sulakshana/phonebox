//! `Stop::sleep`: a plain thread's timed wait that the agent's stop ends at once (one wake per period when idle, as
//! C#'s `Task.Delay(d, ct)`; the media thread used to wake ten times a second to look for the stop).

use std::time::{Duration, Instant};

use super::Stop;

#[test]
fn a_blocking_sleep_ends_as_soon_as_the_agent_stops() {
    let stop = Stop::default();
    let sleeper = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            (stop.sleep(Duration::from_secs(30)), started.elapsed())
        })
    };
    std::thread::sleep(Duration::from_millis(100));
    stop.stop();
    let (stopping, waited) = sleeper.join().unwrap();
    assert!(stopping, "the sleep reports that the agent is stopping");
    assert!(
        waited < Duration::from_secs(5),
        "the stop ended the sleep (waited {waited:?})"
    );
}

#[test]
fn a_blocking_sleep_waits_its_time_when_nothing_stops() {
    let stop = Stop::default();
    let started = Instant::now();
    assert!(!stop.sleep(Duration::from_millis(150)));
    assert!(started.elapsed() >= Duration::from_millis(150));
}

#[test]
fn a_blocking_sleep_after_the_stop_returns_at_once() {
    let stop = Stop::default();
    stop.stop();
    let started = Instant::now();
    assert!(stop.sleep(Duration::from_secs(30)));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(stop.is_stopped());
}

#[tokio::test]
async fn the_async_stop_still_fires() {
    let stop = Stop::default();
    let waiter = {
        let stop = stop.clone();
        tokio::spawn(async move { stop.stopped().await })
    };
    stop.stop();
    tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("stopped")
        .unwrap();
}
