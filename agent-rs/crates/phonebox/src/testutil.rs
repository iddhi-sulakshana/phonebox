//! Helpers shared by the tests of several modules.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// A request the fake server got.
#[derive(Debug, Clone)]
pub(crate) struct Req {
    pub method: String,
    pub path: String,
    pub headers: axum::http::HeaderMap,
    pub body: String,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

/// What the fake server answers.
#[derive(Debug, Clone)]
pub(crate) struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

impl Reply {
    pub fn json(status: u16, body: impl Into<String>) -> Reply {
        Reply {
            status,
            content_type: "application/json",
            body: body.into(),
        }
    }
}

type Handler = Arc<dyn Fn(Req) -> Pin<Box<dyn Future<Output = Reply> + Send>> + Send + Sync>;

/// A loopback HTTP server answering every request through `handler`; stopped when dropped.
pub(crate) struct FakeHttp {
    pub url: String,
    task: tokio::task::JoinHandle<()>,
}

impl FakeHttp {
    pub async fn start<F, Fut>(handler: F) -> FakeHttp
    where
        F: Fn(Req) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Reply> + Send + 'static,
    {
        let handler: Handler = Arc::new(move |r| Box::pin(handler(r)));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
            let handler = handler.clone();
            async move {
                use axum::response::IntoResponse;
                let (parts, body) = req.into_parts();
                let body = axum::body::to_bytes(body, usize::MAX)
                    .await
                    .unwrap_or_default();
                let reply = handler(Req {
                    method: parts.method.to_string(),
                    path: parts.uri.path().to_string(),
                    headers: parts.headers,
                    body: String::from_utf8_lossy(&body).into_owned(),
                })
                .await;
                (
                    axum::http::StatusCode::from_u16(reply.status).unwrap(),
                    [(axum::http::header::CONTENT_TYPE, reply.content_type)],
                    reply.body,
                )
                    .into_response()
            }
        });
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        FakeHttp { url, task }
    }
}

impl Drop for FakeHttp {
    fn drop(&mut self) {
        self.task.abort();
    }
}
