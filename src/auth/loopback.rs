//! One-shot, in-process OAuth loopback server.
//!
//! Binds `127.0.0.1:<port>` (port 0 = ephemeral), accepts exactly ONE HTTP
//! request, extracts query params from the callback path, serves a success
//! page, and drops. The listener lives and dies with this process: nothing
//! is forked, nothing survives the CLI. A hard timeout aborts a stuck login.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub struct Loopback {
    pub listener: TcpListener,
    /// The `redirect_uri` to register with the provider.
    pub redirect_uri: String,
    callback_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Callback {
    pub code: String,
    pub state: String,
}

impl Loopback {
    /// Bind now; nothing is served until [`wait_for_callback`](Self::wait_for_callback).
    ///
    /// * `port` — `Some(p)` fixes the port (ChatGPT needs its registered
    ///   ports), `None` asks the kernel for an ephemeral port.
    /// * `callback_path` — e.g. `/callback` or `/auth/callback`.
    pub async fn bind(port: Option<u16>, callback_path: &str) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port.unwrap_or(0)))
            .await
            .with_context(|| format!("binding loopback server on port {:?}", port))?;
        let addr = listener.local_addr()?;
        let host = if port.is_some() {
            "127.0.0.1"
        } else {
            "localhost"
        };
        Ok(Self {
            listener,
            redirect_uri: format!("http://{host}:{}{callback_path}", addr.port()),
            callback_path: callback_path.to_string(),
        })
    }

    /// Wait for the single browser redirect, with a timeout and a Ctrl-C abort.
    /// Returns the parsed `code` + `state` (or the `error` query param as Err).
    pub async fn wait_for_callback(self, timeout: Duration) -> Result<Callback> {
        let Self {
            listener,
            callback_path,
            ..
        } = self;

        let cb = tokio::select! {
            _ = tokio::signal::ctrl_c() => bail!("login cancelled (Ctrl-C)"),
            r = tokio::time::timeout(timeout, async move {
                loop {
                    let (mut stream, _) = listener.accept().await.context("accept")?;
                    match handle_connection(&mut stream, &callback_path).await {
                        Ok(Outcome::Callback(cb)) => return Ok::<Callback, anyhow::Error>(cb),
                        Ok(Outcome::Ignore) => continue, // e.g. favicon requests
                        Err(e) => return Err(e),
                    }
                }
            }) => r
                .map_err(|_| anyhow!("login timed out after {}s", timeout.as_secs()))??,
        };
        Ok(cb)
    }
}

enum Outcome {
    Callback(Callback),
    Ignore,
}

/// Minimal HTTP/1.x handling: read the request line, respond, close.
async fn handle_connection(stream: &mut TcpStream, callback_path: &str) -> Result<Outcome> {
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).await.context("reading request")?;
    if n == 0 {
        return Ok(Outcome::Ignore);
    }
    let req = String::from_utf8_lossy(&buf[..n]);
    let first_line = req.lines().next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let _method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (target, None),
    };

    if !path.starts_with(callback_path) {
        let body = "not found";
        let resp = format!(
            "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(resp.as_bytes()).await.ok();
        return Ok(Outcome::Ignore);
    }

    let query = query.unwrap_or("");
    let params: Vec<(String, String)> = query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), url_decode(v)))
        .collect();

    let get = |k: &str| {
        params
            .iter()
            .find(|(kk, _)| kk == k)
            .map(|(_, v)| v.clone())
    };

    if let Some(err) = get("error") {
        let desc = get("error_description").unwrap_or_default();
        let body = format!("Login failed: {err} {desc}\nYou can close this window.");
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(resp.as_bytes()).await.ok();
        bail!("authorization server returned error={err} {desc}");
    }

    let code = get("code").ok_or_else(|| anyhow!("callback missing 'code' param"))?;
    let state = get("state").ok_or_else(|| anyhow!("callback missing 'state' param"))?;

    let body = "<html><body style=\"font-family:monospace;background:#111;color:#0f0;display:flex;align-items:center;justify-content:center;height:100vh\"><h1>triagent: login complete</h1></body></html>";
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(resp.as_bytes()).await.ok();
    let _ = stream.flush().await;

    Ok(Outcome::Callback(Callback { code, state }))
}

fn url_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
