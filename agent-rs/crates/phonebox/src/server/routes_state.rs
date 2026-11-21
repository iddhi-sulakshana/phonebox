//! `GET /api/state` and the live stream `GET /api/stream` (server-sent events) the Android shell keeps open.

use std::convert::Infallible;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use super::{AppState, json_response, now_ms};
use crate::config::write::to_dotnet_json_compact;
use crate::state::build_state;
use crate::state_signal;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/state", get(get_state))
        .route("/api/stream", get(stream))
}

async fn get_state(
    State(s): State<AppState>,
    Query(query): Query<Vec<(String, String)>>,
) -> Response {
    // ASP.NET binds `long? ev` from the query: names in any case; present but not a number (empty too) is a 400
    let Ok(ev) = super::query::int64(&query, "ev") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    json_response(
        StatusCode::OK,
        &build_state(&s.sources, &s.cfg, &s.push, ev),
    )
}

/// "state" every whole second (and within 100 ms after a `state_signal::poke`), "event" as soon as one happens,
/// "work" when it changed (checked every second tick). One connection instead of polling.
async fn stream(State(s): State<AppState>, headers: HeaderMap) -> Response {
    let is_phone = headers.contains_key("x-phonebox-app");
    let via = "USB/local"; // the Wi-Fi listener (phase 4) opens streams "Wi-Fi"
    let who = if is_phone { "phone" } else { "other client" };
    crate::log::info("Program", &format!("live stream opened ({who}, {via})"));
    let (tx, rx) = mpsc::channel::<Bytes>(16);
    tokio::spawn(run_stream(s, is_phone, via, tx));
    let frames = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|frame| (Ok::<_, Infallible>(frame), rx))
    });
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        Body::from_stream(frames),
    )
        .into_response()
}

fn sse(name: &str, json: &str) -> Bytes {
    Bytes::from(format!("event: {name}\ndata: {json}\n\n"))
}

/// Queues a frame; false when the client went away or the agent stops (a client that stopped reading leaves the queue
/// full: the stop must not wait for it).
async fn send(tx: &mpsc::Sender<Bytes>, s: &AppState, frame: Bytes) -> bool {
    tokio::select! {
        sent = tx.send(frame) => sent.is_ok(),
        _ = s.stop.stopped() => false,
    }
}

/// The stream's loop (`Program.cs` `/api/stream`); ends when the client goes away or the agent stops.
async fn run_stream(s: AppState, is_phone: bool, via: &str, tx: mpsc::Sender<Bytes>) {
    let mut emitted = s.push.subscribe();
    let mut pokes = state_signal::subscribe();
    let mut seq = s.push.seq();
    let mut last_work = String::new();
    let (mut next, mut last_state, mut tick, mut poked) = (0i64, 0i64, 0u64, false);
    let mut last_spectrum = 0i64;
    let spectrum_every = 1000 / crate::spectrum_analyzer::FPS;
    'stream: while !s.stop.is_stopped() {
        for e in s.push.since(seq) {
            if !send(&tx, &s, sse("event", &to_dotnet_json_compact(&e))).await {
                break 'stream;
            }
            seq = e.seq;
        }
        let now = now_ms();
        let tick_due = now >= next;
        let poke_due = poked && now - last_state >= 100;
        if tick_due || poke_due {
            poked = false; // before the frame is built: a change during the build is answered by the next pass
            if is_phone {
                s.phone.last_seen.store(now, Ordering::SeqCst);
            }
            let frame = s.frames.get(s.push.seq(), state_signal::version());
            if !send(&tx, &s, Bytes::copy_from_slice(frame.as_bytes())).await {
                break;
            }
            last_state = now;
        }
        if tick_due {
            if tick % 2 == 0 {
                let work = to_dotnet_json_compact(&s.sources.work.snapshot());
                if work != last_work {
                    if !send(&tx, &s, sse("work", &work)).await {
                        break;
                    }
                    last_work = work;
                }
            }
            tick += 1;
            next = (now / 1000 + 1) * 1000; // the next whole second, the same for every stream (see AppState.frames)
        }
        // spectrum frames for a visualizer on screen (the analyzer runs only while a panel asks for it). While capture
        // runs, every open stream gets them: the stream is opened by the app shell and /api/spectrum by the panel, and
        // nothing ties the two together. The frames are small (48 numbers) and capture stops ~5 s after the last ask.
        let spectrum_active = s.spectrum.active();
        if spectrum_active && now_ms() - last_spectrum >= spectrum_every - 3 {
            last_spectrum = now_ms();
            if let Some(bands) = s.spectrum.frame() {
                let json = to_dotnet_json_compact(&serde_json::json!({ "b": bands }));
                if !send(&tx, &s, sse("spectrum", &json)).await {
                    break;
                }
            }
        }
        let mut wait = next - now_ms();
        // a change that arrived within 100 ms of the last state waits out the rest of that gap
        if poked {
            wait = wait.min(last_state + 100 - now_ms());
        }
        if spectrum_active {
            wait = wait.min(last_spectrum + spectrum_every - now_ms());
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(wait.max(5) as u64)) => {}
            r = emitted.recv() => if matches!(r, Err(RecvError::Closed)) { break }, // the push store is gone
            r = pokes.recv() => if !matches!(r, Err(RecvError::Closed)) { poked = true },
            _ = s.stop.stopped() => break,
            _ = tx.closed() => break,
        }
    }
    crate::log::info("Program", &format!("live stream closed ({via})"));
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::Body;
    use http_body_util::BodyExt;

    use crate::server::build_router;
    use crate::server::guard::tests::{send, test_state};

    const LOCAL: (&str, &str) = ("host", "127.0.0.1:8765");

    /// The next SSE block, or None when the stream ended or nothing came within `wait`.
    async fn next_frame(body: &mut Body, wait: Duration) -> Option<String> {
        match tokio::time::timeout(wait, body.frame()).await {
            Ok(Some(Ok(f))) => Some(String::from_utf8(f.into_data().ok()?.to_vec()).unwrap()),
            _ => None,
        }
    }

    #[tokio::test]
    async fn stream_sends_state_each_second() {
        let (_d, state) = test_state();
        let app = build_router(state);
        let r = send(&app, "GET", "/api/stream", &[LOCAL]).await;
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["content-type"], "text/event-stream");
        assert_eq!(r.headers()["x-accel-buffering"], "no");
        let mut body = r.into_body();
        let started = std::time::Instant::now();
        let mut states = 0;
        while states < 2 && started.elapsed() < Duration::from_millis(2500) {
            if let Some(f) = next_frame(&mut body, Duration::from_millis(2500)).await {
                if f.starts_with("event: state\n") {
                    assert!(f.ends_with("\n\n"));
                    states += 1;
                } else {
                    assert!(
                        f.starts_with("event: work\ndata: {\"sessions\":[],\"github\":null}\n\n")
                            || f.starts_with("event: event\n"),
                        "{f}"
                    );
                }
            }
        }
        assert_eq!(states, 2, "two state frames within 2.5 s");
    }

    #[tokio::test]
    async fn poke_sends_state_within_100ms() {
        let (_d, state) = test_state();
        let app = build_router(state);
        let mut body = send(&app, "GET", "/api/stream", &[LOCAL]).await.into_body();
        // the opening state and work frames
        assert!(
            next_frame(&mut body, Duration::from_secs(1))
                .await
                .unwrap()
                .starts_with("event: state\n")
        );
        assert!(
            next_frame(&mut body, Duration::from_secs(1))
                .await
                .unwrap()
                .starts_with("event: work\n")
        );
        // poke ~300 ms past a whole second, after the tick's frames are read: the answer cannot be the regular tick
        let ms = crate::server::now_ms() % 1000;
        tokio::time::sleep(Duration::from_millis(((1300 - ms) % 1000) as u64)).await;
        while next_frame(&mut body, Duration::from_millis(20))
            .await
            .is_some()
        {}
        let poked = std::time::Instant::now();
        crate::state_signal::poke();
        let f = next_frame(&mut body, Duration::from_millis(150))
            .await
            .expect("a frame soon after the poke");
        assert!(f.starts_with("event: state\n"), "{f}");
        assert!(poked.elapsed() < Duration::from_millis(150));
    }

    #[tokio::test]
    async fn events_go_out_as_they_happen() {
        let (_d, state) = test_state();
        let push = state.push.clone();
        let app = build_router(state);
        let mut body = send(&app, "GET", "/api/stream", &[LOCAL]).await.into_body();
        next_frame(&mut body, Duration::from_secs(1)).await.unwrap();
        next_frame(&mut body, Duration::from_secs(1)).await.unwrap();
        push.emit("notify", Some(serde_json::json!({"title": "hi"})));
        let f = next_frame(&mut body, Duration::from_millis(300))
            .await
            .expect("the event");
        assert!(f.starts_with("event: event\ndata: {\"seq\":1,\"kind\":\"notify\",\"data\":{\"title\":\"hi\"},\"at\":"), "{f}");
    }

    #[tokio::test]
    async fn stream_ends_when_the_agent_stops() {
        let (_d, state) = test_state();
        let stop = state.stop.clone();
        let app = build_router(state);
        let mut body = send(&app, "GET", "/api/stream", &[LOCAL]).await.into_body();
        next_frame(&mut body, Duration::from_secs(1)).await.unwrap();
        stop.stop();
        // the remaining frames, then the end of the body: the server can finish its graceful shutdown
        let ended = tokio::time::timeout(Duration::from_secs(1), async {
            while body.frame().await.is_some() {}
        })
        .await;
        assert!(ended.is_ok(), "the stream did not end");
    }

    #[tokio::test]
    async fn state_route_sends_the_state_and_events_after_ev() {
        let (_d, state) = test_state();
        state.push.emit("notify", None);
        let app = build_router(state);
        let r = send(&app, "GET", "/api/state?ev=0", &[LOCAL]).await;
        assert_eq!(
            r.headers()["content-type"],
            "application/json; charset=utf-8"
        );
        let v: serde_json::Value =
            serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(v["events"][0]["kind"], "notify");
        assert_eq!(v["events"][0]["data"], serde_json::Value::Null);
        let r = send(&app, "GET", "/api/state?ev=abc", &[LOCAL]).await;
        assert_eq!(r.status(), 400);
        // ASP.NET binds long? ev with TryParse: empty and repeated values are 400s too (seen in phase-2 parity)
        for path in ["/api/state?ev=", "/api/state?ev=1&ev=2"] {
            assert_eq!(
                send(&app, "GET", path, &[LOCAL]).await.status(),
                400,
                "{path}"
            );
        }
    }

    /// Review #10: a client that opened the stream and stopped reading (its socket buffers full) cannot keep the agent
    /// from stopping: open connections get a grace period, then the server lets go (ASP.NET: 30 s by default).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_client_that_stops_reading_cannot_hold_up_a_stop() {
        use tokio::io::AsyncWriteExt;
        let (_d, state) = test_state();
        let (push, stop) = (state.push.clone(), state.stop.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(crate::server::serve(
            listener,
            state,
            Duration::from_millis(500),
        ));
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /api/stream HTTP/1.1\r\nHost: 127.0.0.1:8765\r\n\r\n")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        // the client never reads: big events fill its socket buffers, then the stream's queue
        let big = serde_json::json!("x".repeat(1024 * 1024));
        for _ in 0..48 {
            push.emit("notify", Some(big.clone()));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        stop.stop();
        let stopped = tokio::time::timeout(Duration::from_secs(5), server).await;
        assert!(
            stopped.is_ok(),
            "the server was still waiting for the client 5 s after the stop"
        );
        drop(client);
    }

    #[tokio::test]
    async fn spectrum_frames_go_out_on_the_stream_while_capture_runs() {
        let (_dir, state) = test_state();
        let page = serde_json::json!({"pages": [{"id": "p", "title": "P",
            "widgets": [{"id": "s", "type": "spectrum", "x": 0, "y": 0, "w": 6, "h": 3}]}]});
        state.cfg.save(&page.to_string(), None).unwrap();
        state
            .spectrum
            .pretend_running(vec![5; crate::spectrum_analyzer::BANDS]);
        assert!(state.spectrum.want());
        let app = build_router(state);
        let mut body = send(&app, "GET", "/api/stream", &[LOCAL]).await.into_body();
        let started = std::time::Instant::now();
        let mut frames = 0;
        while started.elapsed() < Duration::from_millis(600) {
            if let Some(f) = next_frame(&mut body, Duration::from_millis(600)).await
                && f.starts_with("event: spectrum\n")
            {
                assert!(f.contains(r#"data: {"b":[5,5,5"#), "{f}");
                frames += 1;
            }
        }
        // 30 a second: well over ten in 600 ms, and never more than the rate allows
        assert!(
            (10..=20).contains(&frames),
            "{frames} spectrum frames in 600 ms"
        );
    }
}
