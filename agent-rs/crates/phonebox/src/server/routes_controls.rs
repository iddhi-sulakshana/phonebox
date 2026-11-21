//! The Control Deck routes (`Program.cs:90-130`); the deck's own routes join these with the deck.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use super::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/controls/task-output/{id}", get(task_output))
}

/// The last output of a deck task's program, as plain text (local only).
async fn task_output(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    ([(header::CONTENT_TYPE, "text/plain")], s.tasks.output(&id)).into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;

    use crate::server::guard::tests::{send, test_state};

    const LOCAL: (&str, &str) = ("host", "127.0.0.1:8765");

    #[tokio::test]
    async fn a_tasks_output_is_plain_text_and_unknown_tasks_have_none() {
        let (_d, s) = test_state();
        let app = crate::server::build_router(s);
        let res = send(&app, "GET", "/api/controls/task-output/unknown", &[LOCAL]).await;
        assert_eq!(res.status(), 200);
        assert_eq!(res.headers()["content-type"], "text/plain");
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"No task output");
    }
}
