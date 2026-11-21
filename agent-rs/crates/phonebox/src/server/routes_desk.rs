//! The desk routes (plan 3): volume, media, the spectrum, shortcuts and their icons, apps, dictation and the
//! trackpad. Each comes with its monitor.

use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;

use super::query::{int32, value};
use super::{AppState, json_response};

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/volume", post(volume))
}

type QueryList = Query<Vec<(String, String)>>;

fn bad_request() -> Response {
    StatusCode::BAD_REQUEST.into_response()
}

/// `op=mute|up|down` (±5), else `level=N`; success pokes the state and answers the current reading (or `null`).
async fn volume(State(s): State<AppState>, Query(q): QueryList) -> Response {
    let Ok(level) = int32(&q, "level") else {
        return bad_request();
    };
    let op = value(&q, "op");
    let audio = s.audio.clone();
    let result = tokio::task::spawn_blocking(move || {
        let ok = match (op.as_deref(), level) {
            (Some("mute"), _) => audio.toggle_mute(),
            (Some("up"), _) => audio.step(5),
            (Some("down"), _) => audio.step(-5),
            (_, Some(l)) => audio.set_volume(l),
            _ => false,
        };
        ok.then(|| audio.current())
    })
    .await;
    match result {
        Ok(Some(current)) => {
            crate::state_signal::poke(); // the phone's other screens see it now, not at the next second
            json_response(StatusCode::OK, &current)
        }
        _ => bad_request(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::http::StatusCode;
    use http_body_util::BodyExt;

    use crate::audio_control::{AudioControl, AudioSnapshot};
    use crate::server::build_router;
    use crate::server::guard::tests::{send, test_state};

    async fn call(
        app: &crate::server::App,
        method: &str,
        path: &str,
    ) -> (StatusCode, String, Vec<u8>) {
        let res = send(app, method, path, &[("host", "127.0.0.1:8765")]).await;
        let status = res.status();
        let kind = res
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        (
            status,
            kind,
            res.into_body().collect().await.unwrap().to_bytes().to_vec(),
        )
    }

    #[tokio::test]
    async fn volume_follows_op_and_level_and_answers_the_current_reading() {
        let (_dir, mut state) = test_state();
        let device = Arc::new(Mutex::new(AudioSnapshot::new(50, false, true)));
        let (d1, d2, d3) = (device.clone(), device.clone(), device.clone());
        state.audio = AudioControl::with(
            Arc::new(move || Some(d1.lock().unwrap().clone())),
            Some(Arc::new(move |level| {
                d2.lock().unwrap().volume = level;
                Some(level)
            })),
            Some(Arc::new(move || {
                let mut d = d3.lock().unwrap();
                d.muted = !d.muted;
                true
            })),
        );
        let app = build_router(state);
        let json = |b: &[u8]| serde_json::from_slice::<serde_json::Value>(b).unwrap();

        let (status, kind, body) = call(&app, "POST", "/api/volume?op=up").await;
        assert_eq!(
            (status, kind.as_str()),
            (StatusCode::OK, "application/json; charset=utf-8")
        );
        assert_eq!(
            json(&body),
            serde_json::json!({"volume": 55, "muted": false, "canControl": true})
        );
        assert_eq!(
            json(&call(&app, "POST", "/api/volume?op=down").await.2)["volume"],
            50
        );
        assert_eq!(
            json(&call(&app, "POST", "/api/volume?level=30").await.2)["volume"],
            30
        );
        assert_eq!(
            json(&call(&app, "POST", "/api/volume?level=130").await.2)["volume"],
            100
        );
        assert_eq!(
            json(&call(&app, "POST", "/api/volume?op=mute").await.2)["muted"],
            true
        );
        // op wins over level; an unknown op falls back to the level
        assert_eq!(
            json(&call(&app, "POST", "/api/volume?op=up&level=10").await.2)["volume"],
            100
        );
        assert_eq!(
            json(&call(&app, "POST", "/api/volume?op=loud&level=10").await.2)["volume"],
            10
        );

        let (status, _, body) = call(&app, "POST", "/api/volume").await;
        assert_eq!((status, body.is_empty()), (StatusCode::BAD_REQUEST, true));
        assert_eq!(
            call(&app, "POST", "/api/volume?level=x").await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(&app, "POST", "/api/volume?op=MUTE").await.0,
            StatusCode::BAD_REQUEST,
            "ops are case-sensitive"
        );
    }

    #[tokio::test]
    async fn a_failed_volume_change_is_a_400_and_the_state_carries_audio() {
        let (_dir, mut state) = test_state();
        state.audio = AudioControl::with(
            Arc::new(|| Some(AudioSnapshot::new(20, true, true))),
            Some(Arc::new(|_| None)),
            None,
        );
        let audio = state.audio.clone();
        let app = build_router(state);
        assert_eq!(
            call(&app, "POST", "/api/volume?level=40").await.0,
            StatusCode::BAD_REQUEST
        );
        audio.current();
        let v = crate::state::StateSource::snapshot(&*audio);
        assert_eq!(
            v,
            serde_json::json!({"volume": 20, "muted": true, "canControl": true})
        );
    }
}
