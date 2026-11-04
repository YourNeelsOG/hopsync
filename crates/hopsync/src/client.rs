//! The CLI side of the daemon (Phase 2 step 8): JSON requests over the owner-only Unix socket, or
//! over TCP with a session cookie (tests, and remote use behind a proxy later).
//!
//! One connection per request: the CLI makes a handful of calls, and HTTP/1.1 keep-alive would
//! only add state.

use std::net::SocketAddr;
use std::path::PathBuf;

use http_body_util::{BodyExt, Full};
pub use hyper::Method;
use hyper::body::Bytes;
use hyper::{Request, StatusCode, header};
use hyper_util::rt::TokioIo;
use serde_json::Value;

/// Responses bigger than this are refused: nothing the API sends is near it.
const MAX_BODY: usize = 64 << 20;

#[derive(Debug, Clone)]
pub enum Endpoint {
    Unix(PathBuf),
    Tcp(SocketAddr),
}

#[derive(Debug)]
pub struct ClientError(pub String);

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ClientError {}

#[derive(Debug, Clone)]
pub struct Reply {
    pub status: StatusCode,
    pub set_cookie: Option<String>,
    pub body: Value,
}

impl Reply {
    /// The body on success, else the API's error text.
    pub fn ok(self) -> Result<Value, ClientError> {
        if self.status.is_success() {
            return Ok(self.body);
        }
        let msg = self.body["error"]
            .as_str()
            .map_or_else(|| self.status.to_string(), str::to_owned);
        Err(ClientError(msg))
    }
}

#[derive(Debug, Clone)]
pub struct Client {
    pub endpoint: Endpoint,
    /// `name=value` sent as the Cookie header (TCP only).
    pub cookie: Option<String>,
}

impl Client {
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            cookie: None,
        }
    }

    pub async fn get(&self, path: &str) -> Result<Reply, ClientError> {
        self.request(Method::GET, path, None).await
    }

    pub async fn post(&self, path: &str, body: Value) -> Result<Reply, ClientError> {
        self.request(Method::POST, path, Some(body)).await
    }

    pub async fn delete(&self, path: &str) -> Result<Reply, ClientError> {
        self.request(Method::DELETE, path, None).await
    }

    pub async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Reply, ClientError> {
        let fail = |what: &str, e: &dyn std::fmt::Display| ClientError(format!("{what}: {e}"));
        let host = match &self.endpoint {
            Endpoint::Unix(_) => "localhost".to_owned(),
            Endpoint::Tcp(a) => a.to_string(),
        };
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, &host);
        if let Endpoint::Tcp(_) = self.endpoint {
            // Same-origin, as a browser on the UI would send it.
            req = req.header(header::ORIGIN, format!("http://{host}"));
            if let Some(c) = &self.cookie {
                req = req.header(header::COOKIE, c);
            }
        }
        let bytes = match &body {
            Some(v) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Bytes::from(serde_json::to_vec(v).map_err(|e| fail("encode", &e))?)
            }
            None => Bytes::new(),
        };
        let req = req
            .body(Full::new(bytes))
            .map_err(|e| fail("request", &e))?;
        let resp = match &self.endpoint {
            Endpoint::Unix(p) => {
                crate::daemon::check_socket_path(p).map_err(ClientError)?;
                let s = tokio::net::UnixStream::connect(p).await.map_err(|e| {
                    ClientError(format!(
                        "cannot reach the hopsync daemon at {}: {e} (is `hopsync serve` running?)",
                        p.display()
                    ))
                })?;
                send(TokioIo::new(s), req).await?
            }
            Endpoint::Tcp(a) => {
                let s = tokio::net::TcpStream::connect(a)
                    .await
                    .map_err(|e| fail(&format!("connect {a}"), &e))?;
                send(TokioIo::new(s), req).await?
            }
        };
        let status = resp.status();
        let set_cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .map(str::to_owned);
        let limited = http_body_util::Limited::new(resp.into_body(), MAX_BODY);
        let raw = limited
            .collect()
            .await
            .map_err(|e| ClientError(format!("response: {e}")))?
            .to_bytes();
        let body = if raw.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&raw).map_err(|e| fail("response is not JSON", &e))?
        };
        Ok(Reply {
            status,
            set_cookie,
            body,
        })
    }
}

async fn send<IO>(
    io: TokioIo<IO>,
    req: Request<Full<Bytes>>,
) -> Result<hyper::Response<hyper::body::Incoming>, ClientError>
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| ClientError(format!("handshake: {e}")))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    sender
        .send_request(req)
        .await
        .map_err(|e| ClientError(format!("request: {e}")))
}
