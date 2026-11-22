//! The Discord app's local RPC transport: a Unix socket (`$XDG_RUNTIME_DIR`, `$TMPDIR` or `/tmp`) or a Windows named
//! pipe, `discord-ipc-0` to `-9`; frames are a little-endian op and length, then UTF-8 JSON (at most 1 MiB).

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Any stream the RPC runs over (the app's socket or pipe, a test pipe).
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub const MAX_FRAME: usize = 1 << 20;

/// Connects to the first Discord app that answers; "Discord is not running" when none does.
pub async fn open() -> Result<Box<dyn Io>, String> {
    for i in 0..10 {
        #[cfg(windows)]
        {
            let name = format!(r"\\.\pipe\discord-ipc-{i}");
            if let Ok(pipe) = tokio::net::windows::named_pipe::ClientOptions::new().open(&name) {
                return Ok(Box::new(pipe));
            }
        }
        #[cfg(unix)]
        {
            // an empty variable counts as set, as in C#
            let dir = std::env::var("XDG_RUNTIME_DIR")
                .or_else(|_| std::env::var("TMPDIR"))
                .unwrap_or_else(|_| "/tmp".into());
            let path = std::path::Path::new(&dir).join(format!("discord-ipc-{i}"));
            if let Ok(Ok(socket)) = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                tokio::net::UnixStream::connect(&path),
            )
            .await
            {
                return Ok(Box::new(socket));
            }
        }
        let _ = i;
    }
    Err("Discord is not running".into())
}

/// One frame: the op, the length, the JSON.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    op: i32,
    body: &Value,
) -> std::io::Result<()> {
    let json = body.to_string().into_bytes();
    let mut buf = Vec::with_capacity(8 + json.len());
    buf.extend_from_slice(&op.to_le_bytes());
    buf.extend_from_slice(&(json.len() as i32).to_le_bytes());
    buf.extend_from_slice(&json);
    w.write_all(&buf).await?;
    w.flush().await
}

/// The next frame: None at the end of the stream or on a close (op 2); an empty body has no JSON. A length outside
/// 0..1 MiB or a body that is not JSON is an error (the reader ends).
pub async fn read_frame<R: AsyncRead + Unpin>(
    r: &mut R,
) -> Result<Option<(i32, Option<Value>)>, String> {
    let mut head = [0u8; 8];
    if !read_exact(r, &mut head).await? {
        return Ok(None);
    }
    let op = i32::from_le_bytes(head[..4].try_into().expect("4 bytes"));
    let len = i32::from_le_bytes(head[4..].try_into().expect("4 bytes"));
    if len < 0 || len as usize > MAX_FRAME {
        return Err("Discord sent something unexpected".into());
    }
    let mut body = vec![0u8; len as usize];
    if !read_exact(r, &mut body).await? {
        return Ok(None);
    }
    if op == 2 {
        return Ok(None);
    }
    if body.is_empty() {
        return Ok(Some((op, None)));
    }
    serde_json::from_slice(&body)
        .map(|v| Some((op, Some(v))))
        .map_err(|e| e.to_string())
}

/// False at the end of the stream.
async fn read_exact<R: AsyncRead + Unpin>(r: &mut R, buf: &mut [u8]) -> Result<bool, String> {
    let mut got = 0;
    while got < buf.len() {
        let n = r.read(&mut buf[got..]).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(false);
        }
        got += n;
    }
    Ok(true)
}
