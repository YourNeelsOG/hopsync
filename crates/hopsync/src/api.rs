//! JSON API and live progress (Phase 2 step 7, plan.md §7, UX study U1 to U21).
//!
//! - Every route needs a session cookie except `health`, `setup` and `login`. Requests over the
//!   local Unix socket are trusted instead (the socket is owner-only and the peer uid is checked
//!   in `daemon`).
//! - CSRF: every state-changing request from the network must pass `auth::same_origin`, and POST
//!   and PUT bodies must be `application/json` (an HTML form cannot send that cross-site without
//!   a CORS preflight, which this server never approves). A file upload is
//!   `application/octet-stream`, which needs that preflight just the same.
//! - On a loopback listener, the `Host` header must name a loopback host (DNS rebinding).
//! - Every response carries `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer` and
//!   a CSP that allows nothing but this origin. All but hashed UI files carry `Cache-Control:
//!   no-store`.
//! - Live progress is Server-Sent Events at `/api/events`: one `snapshot` event whenever the
//!   service's snapshot changes. Slow clients skip snapshots; nothing queues per client.
//! - Errors are `{"error": "..."}`. Internal failures are logged and answered without detail.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::auth::{AuthError, same_origin};
use crate::service::{Service, ServiceError, SettingsUpdate};
use crate::store::{Direction, JobSpec, JobState, ServerSpec};

pub const COOKIE: &str = "hopsync_session";

/// How a request reached the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// TCP: browsers and anything else on the network. Sessions and CSRF checks apply.
    Network {
        /// Mark cookies `Secure` (the daemon is behind TLS).
        secure_cookies: bool,
        /// The listener is on a loopback address: only loopback names may appear in `Host`,
        /// which defeats DNS rebinding (a foreign page whose name resolves to 127.0.0.1).
        loopback_only: bool,
    },
    /// The owner-only Unix socket: the local CLI.
    Local,
}

#[derive(Clone)]
struct AppState {
    svc: Arc<Service>,
}

pub struct ApiError(StatusCode, String, Option<Duration>);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut r = (self.0, Json(json!({ "error": self.1 }))).into_response();
        if let Some(after) = self.2
            && let Ok(v) = HeaderValue::from_str(&after.as_secs().max(1).to_string())
        {
            r.headers_mut().insert(header::RETRY_AFTER, v);
        }
        r
    }
}

fn err(status: StatusCode, msg: impl Into<String>) -> ApiError {
    ApiError(status, msg.into(), None)
}

impl From<ServiceError> for ApiError {
    fn from(e: ServiceError) -> Self {
        match e {
            ServiceError::BadRequest(m) => err(StatusCode::BAD_REQUEST, m),
            ServiceError::NotFound => err(StatusCode::NOT_FOUND, "not found"),
            ServiceError::Conflict(m) => err(StatusCode::CONFLICT, m),
            // The remote side failed or refused: tell the user what it said.
            e @ (ServiceError::HostKey(_) | ServiceError::Transfer(_)) => err(
                StatusCode::BAD_GATEWAY,
                crate::cli::printable(&e.to_string()),
            ),
            e => {
                eprintln!("hopsync: api: {e}");
                err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error; see the daemon log",
                )
            }
        }
    }
}

impl From<AuthError> for ApiError {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::SetupRequired => err(StatusCode::FORBIDDEN, "setup is required"),
            AuthError::SetupDone => err(StatusCode::CONFLICT, "setup is already done"),
            AuthError::Denied => err(StatusCode::UNAUTHORIZED, "not authorized"),
            AuthError::Throttled(d) => ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "too many attempts; wait and try again".into(),
                Some(d),
            ),
            AuthError::WeakPassword(m) => err(StatusCode::BAD_REQUEST, m),
            e => {
                eprintln!("hopsync: auth: {e}");
                err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error; see the daemon log",
                )
            }
        }
    }
}

type ApiResult<T> = Result<T, ApiError>;

/// Runs blocking work (password hashing) off the async threads.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, AuthError> + Send + 'static,
) -> ApiResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?
        .map_err(Into::into)
}

/// How often an idle event stream re-checks its session and shows the UI it is alive. The UI gives
/// up after three of these (`STALL_MS` in `live.ts`).
const HEARTBEAT_EVERY: Duration = Duration::from_secs(5);

pub fn router(svc: Arc<Service>, transport: Transport) -> Router {
    let state = AppState { svc };
    let open = Router::new()
        .route("/", get(asset))
        .route("/{file}", get(asset))
        .route("/assets/{file}", get(hashed_asset))
        .route("/api/health", get(health))
        .route("/api/setup", post(setup))
        .route("/api/login", post(login));
    let guarded = Router::new()
        .route("/api/logout", post(logout))
        .route("/api/password", post(change_password))
        .route("/api/servers", get(list_servers).post(add_server))
        .route(
            "/api/servers/{id}",
            get(get_server).put(update_server).delete(delete_server),
        )
        .route("/api/servers/{id}/public-key", get(public_key))
        .route("/api/servers/{id}/probe", post(probe))
        .route("/api/servers/{id}/trust", post(trust))
        .route("/api/servers/{id}/install-key", post(install_key))
        .route("/api/servers/{id}/list", get(list_remote))
        .route("/api/local/list", get(list_local))
        .route("/api/local/file", get(download_file).put(upload_file))
        .route("/api/jobs", get(list_jobs).post(add_job))
        .route("/api/jobs/dry-run", post(dry_run))
        .route("/api/jobs/{id}", get(get_job).delete(delete_job))
        .route("/api/jobs/{id}/pause", post(pause_job))
        .route("/api/jobs/{id}/resume", post(resume_job))
        .route("/api/jobs/{id}/cancel", post(cancel_job))
        .route("/api/jobs/{id}/move", post(move_job))
        .route("/api/jobs/{id}/items", get(job_items))
        .route("/api/jobs/{id}/events", get(job_events))
        .route("/api/queue/pause", post(pause_queue))
        .route("/api/queue/resume", post(resume_queue))
        .route("/api/events", get(events))
        .route("/api/audit", get(audit))
        .route("/api/settings", get(get_settings).put(put_settings))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_session,
        ));
    open.merge(guarded)
        .layer(middleware::from_fn(csrf))
        .layer(middleware::from_fn(security_headers))
        .layer(Extension(transport))
        .with_state(state)
}

async fn security_headers(req: Request, next: Next) -> Response {
    let mut r = next.run(req).await;
    let h = r.headers_mut();
    let set = |h: &mut HeaderMap, k: header::HeaderName, v: &'static str| {
        h.insert(k, HeaderValue::from_static(v));
    };
    // Only `hashed_asset` sets its own: everything else, data above all, is never stored.
    if !h.contains_key(header::CACHE_CONTROL) {
        set(h, header::CACHE_CONTROL, "no-store");
    }
    set(h, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    set(h, header::REFERRER_POLICY, "no-referrer");
    set(h, header::X_FRAME_OPTIONS, "DENY");
    set(
        h,
        header::CONTENT_SECURITY_POLICY,
        "default-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
    );
    r
}

fn header_str(h: &HeaderMap, name: impl header::AsHeaderName) -> Option<&str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

/// `Host` names a loopback address: 127.0.0.0/8, ::1 or localhost, with any port.
pub fn loopback_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host.rsplit_once(':').map_or(host, |(h, p)| {
            if p.bytes().all(|b| b.is_ascii_digit()) {
                h
            } else {
                host
            }
        }),
    };
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

async fn csrf(Extension(t): Extension<Transport>, req: Request, next: Next) -> ApiResult<Response> {
    if let Transport::Network {
        loopback_only: true,
        ..
    } = t
        && !header_str(req.headers(), header::HOST).is_some_and(loopback_host)
    {
        return Err(err(
            StatusCode::MISDIRECTED_REQUEST,
            "this daemon only answers to localhost; use http://127.0.0.1",
        ));
    }
    let changes = !matches!(*req.method(), Method::GET | Method::HEAD);
    if changes && t != Transport::Local {
        let h = req.headers();
        if !same_origin(
            header_str(h, header::ORIGIN),
            header_str(h, "sec-fetch-site"),
            header_str(h, header::HOST),
        ) {
            return Err(err(StatusCode::FORBIDDEN, "cross-origin request refused"));
        }
        let wanted = if *req.method() == Method::PUT && req.uri().path() == "/api/local/file" {
            "application/octet-stream"
        } else {
            "application/json"
        };
        if matches!(*req.method(), Method::POST | Method::PUT)
            && !header_str(h, header::CONTENT_TYPE)
                .is_some_and(|c| c.split(';').next().is_some_and(|m| m.trim() == wanted))
        {
            return Err(err(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                format!("the body must be {wanted}"),
            ));
        }
    }
    Ok(next.run(req).await)
}

fn session_token(h: &HeaderMap) -> Option<String> {
    h.get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v.to_owned())
}

async fn require_session(
    State(st): State<AppState>,
    Extension(t): Extension<Transport>,
    req: Request,
    next: Next,
) -> ApiResult<Response> {
    if t == Transport::Local {
        return Ok(next.run(req).await);
    }
    if st.svc.auth.needs_setup() {
        return Err(AuthError::SetupRequired.into());
    }
    let token = session_token(req.headers()).ok_or(AuthError::Denied)?;
    let svc = st.svc.clone();
    blocking(move || svc.auth.check(&token)).await?;
    Ok(next.run(req).await)
}

fn peer(info: Option<&Extension<ConnectInfo<SocketAddr>>>) -> IpAddr {
    // The Unix socket has no address; its requests never reach the throttle with a stranger's.
    info.map_or(IpAddr::V4(Ipv4Addr::LOCALHOST), |c| c.0.0.ip())
}

fn session_cookie(token: &str, t: Transport) -> HeaderValue {
    let secure = matches!(
        t,
        Transport::Network {
            secure_cookies: true,
            ..
        }
    );
    let v = format!(
        "{COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}{}",
        crate::auth::SESSION_MAX_SECS,
        if secure { "; Secure" } else { "" }
    );
    HeaderValue::from_str(&v).unwrap_or_else(|_| HeaderValue::from_static(""))
}

fn with_session(token: &str, t: Transport) -> Response {
    let mut r = Json(json!({ "ok": true })).into_response();
    r.headers_mut()
        .insert(header::SET_COOKIE, session_cookie(token, t));
    r
}

// Auth routes.

async fn health(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({ "ok": true, "setup_required": st.svc.auth.needs_setup() }))
}

#[derive(Deserialize)]
struct SetupBody {
    token: String,
    password: String,
}

async fn setup(
    State(st): State<AppState>,
    Extension(t): Extension<Transport>,
    info: Option<Extension<ConnectInfo<SocketAddr>>>,
    Json(b): Json<SetupBody>,
) -> ApiResult<Response> {
    let (svc, from) = (st.svc.clone(), peer(info.as_ref()));
    let token = blocking(move || svc.auth.setup(&b.token, &b.password, from)).await?;
    Ok(with_session(&token, t))
}

#[derive(Deserialize)]
struct LoginBody {
    password: String,
}

async fn login(
    State(st): State<AppState>,
    Extension(t): Extension<Transport>,
    info: Option<Extension<ConnectInfo<SocketAddr>>>,
    Json(b): Json<LoginBody>,
) -> ApiResult<Response> {
    let (svc, from) = (st.svc.clone(), peer(info.as_ref()));
    let token = blocking(move || svc.auth.login(&b.password, from)).await?;
    Ok(with_session(&token, t))
}

async fn logout(State(st): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(token) = session_token(&headers) {
        st.svc.auth.logout(&token)?;
    }
    let mut r = Json(json!({ "ok": true })).into_response();
    r.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("hopsync_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"),
    );
    Ok(r)
}

#[derive(Deserialize)]
struct PasswordBody {
    current: String,
    new: String,
}

async fn change_password(
    State(st): State<AppState>,
    Extension(t): Extension<Transport>,
    info: Option<Extension<ConnectInfo<SocketAddr>>>,
    Json(b): Json<PasswordBody>,
) -> ApiResult<Response> {
    let (svc, from) = (st.svc.clone(), peer(info.as_ref()));
    let token = blocking(move || svc.auth.change_password(&b.current, &b.new, from)).await?;
    Ok(with_session(&token, t))
}

// Servers.

#[derive(Deserialize)]
struct NewServer {
    #[serde(flatten)]
    spec: ServerSpec,
    #[serde(default)]
    generate_key: bool,
}

async fn list_servers(State(st): State<AppState>) -> ApiResult<Response> {
    Ok(Json(st.svc.store.servers().map_err(ServiceError::from)?).into_response())
}

async fn add_server(State(st): State<AppState>, Json(b): Json<NewServer>) -> ApiResult<Response> {
    let s = st.svc.add_server(&b.spec, b.generate_key)?;
    Ok((StatusCode::CREATED, Json(s)).into_response())
}

async fn get_server(State(st): State<AppState>, Path(id): Path<i64>) -> ApiResult<Response> {
    Ok(Json(st.svc.store.server(id).map_err(ServiceError::from)?).into_response())
}

async fn update_server(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Json(spec): Json<ServerSpec>,
) -> ApiResult<Response> {
    Ok(Json(st.svc.update_server(id, &spec)?).into_response())
}

async fn delete_server(State(st): State<AppState>, Path(id): Path<i64>) -> ApiResult<Response> {
    st.svc.delete_server(id)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn public_key(State(st): State<AppState>, Path(id): Path<i64>) -> ApiResult<Response> {
    Ok(Json(json!({ "public_key": st.svc.public_key(id)? })).into_response())
}

async fn probe(State(st): State<AppState>, Path(id): Path<i64>) -> ApiResult<Response> {
    Ok(Json(st.svc.probe(id).await?).into_response())
}

#[derive(Deserialize)]
struct TrustBody {
    fingerprint: String,
    #[serde(default)]
    replace: bool,
}

async fn trust(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Json(b): Json<TrustBody>,
) -> ApiResult<Response> {
    st.svc.trust(id, &b.fingerprint, b.replace).await?;
    Ok(Json(json!({ "ok": true })).into_response())
}

#[derive(Deserialize)]
struct InstallBody {
    /// Used for this one login, then dropped (wiped from memory); never stored or logged.
    password: Option<zeroize::Zeroizing<String>>,
    /// Or an existing key file on this machine that the server already accepts.
    key_file: Option<String>,
    authorized_keys: Option<String>,
}

async fn install_key(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Json(b): Json<InstallBody>,
) -> ApiResult<Response> {
    let auth = match (b.password, b.key_file) {
        (Some(p), None) => crate::ssh::KeySource::Password(Arc::new(p)),
        (None, Some(f)) if std::path::Path::new(&f).is_absolute() => {
            crate::ssh::KeySource::File(f.into())
        }
        _ => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "give either password or an absolute key_file",
            ));
        }
    };
    st.svc
        .install_key(id, auth, b.authorized_keys.as_deref())
        .await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct PathQuery {
    #[serde(default)]
    path: String,
}

async fn list_remote(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<PathQuery>,
) -> ApiResult<Response> {
    Ok(Json(st.svc.list_remote(id, &q.path).await?).into_response())
}

async fn list_local(State(st): State<AppState>, Query(q): Query<PathQuery>) -> ApiResult<Response> {
    Ok(Json(st.svc.list_local(&q.path).await?).into_response())
}

/// How much of a file a download holds in memory, per download.
const DOWNLOAD_PIECE: usize = 1 << 20;

/// `filename*` of a `Content-Disposition`: everything but plain characters is percent-encoded,
/// so no name can add a header or break out of the value.
fn disposition(name: &str) -> String {
    let mut out = String::from("attachment; filename*=UTF-8''");
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

async fn download_file(
    State(st): State<AppState>,
    Query(q): Query<PathQuery>,
) -> ApiResult<Response> {
    let d = st.svc.open_download(&q.path).await?;
    let (size, name) = (d.size, d.name);
    let reader = Arc::new(d.reader);
    // Reads the next piece only when the connection has taken the last one, and stops at the size
    // it announced if the file grows meanwhile.
    let pieces = futures_util::stream::unfold((reader, 0u64), move |(reader, offset)| async move {
        if offset >= size {
            return None;
        }
        let want = usize::try_from((size - offset).min(DOWNLOAD_PIECE as u64)).ok()?;
        let r = reader.clone();
        let read = tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; want];
            r.read_at(offset, &mut buf).map(|n| {
                buf.truncate(n);
                buf
            })
        })
        .await;
        match read {
            Ok(Ok(buf)) if !buf.is_empty() => {
                let next = offset + buf.len() as u64;
                Some((Ok(axum::body::Bytes::from(buf)), (reader, next)))
            }
            // A file that shrank: the short body tells the browser it did not finish.
            Ok(Ok(_)) => None,
            Ok(Err(e)) => Some((Err(e), (reader, size))),
            Err(e) => Some((Err(std::io::Error::other(e)), (reader, size))),
        }
    });
    let mut r = axum::body::Body::from_stream(pieces).into_response();
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    if let Ok(v) = HeaderValue::from_str(&disposition(&name)) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(r)
}

#[derive(Deserialize)]
struct UploadQuery {
    path: String,
    #[serde(default)]
    overwrite: bool,
}

async fn upload_file(
    State(st): State<AppState>,
    Query(q): Query<UploadQuery>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> ApiResult<Response> {
    let declared = header_str(&headers, header::CONTENT_LENGTH).and_then(|v| v.parse().ok());
    let bytes = st
        .svc
        .upload_local(&q.path, q.overwrite, declared, body.into_data_stream())
        .await?;
    Ok(Json(json!({ "bytes": bytes })).into_response())
}

// Jobs.

#[derive(Deserialize)]
struct NewJob {
    direction: Direction,
    server: i64,
    src: String,
    dst: String,
    #[serde(default = "default_existing")]
    existing: String,
    #[serde(default)]
    verify: bool,
    lanes: Option<u32>,
}

fn default_existing() -> String {
    "skip".into()
}

impl From<NewJob> for JobSpec {
    fn from(j: NewJob) -> Self {
        Self {
            direction: j.direction,
            server: j.server,
            src: j.src,
            dst: j.dst,
            existing: j.existing,
            verify: j.verify,
            lanes: j.lanes,
        }
    }
}

#[derive(Deserialize)]
struct StateQuery {
    state: Option<JobState>,
}

async fn list_jobs(State(st): State<AppState>, Query(q): Query<StateQuery>) -> ApiResult<Response> {
    Ok(Json(st.svc.store.jobs(q.state).map_err(ServiceError::from)?).into_response())
}

async fn add_job(State(st): State<AppState>, Json(b): Json<NewJob>) -> ApiResult<Response> {
    let job = st.svc.add_job(&b.into())?;
    Ok((StatusCode::CREATED, Json(job)).into_response())
}

async fn dry_run(State(st): State<AppState>, Json(b): Json<NewJob>) -> ApiResult<Response> {
    Ok(Json(st.svc.dry_run(&b.into()).await?).into_response())
}

async fn get_job(State(st): State<AppState>, Path(id): Path<i64>) -> ApiResult<Response> {
    Ok(Json(st.svc.store.job(id).map_err(ServiceError::from)?).into_response())
}

async fn delete_job(State(st): State<AppState>, Path(id): Path<i64>) -> ApiResult<Response> {
    st.svc.delete_job(id)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn ok() -> Response {
    Json(json!({ "ok": true })).into_response()
}

async fn pause_job(State(st): State<AppState>, Path(id): Path<i64>) -> ApiResult<Response> {
    st.svc.pause_job(id)?;
    Ok(ok())
}

async fn resume_job(State(st): State<AppState>, Path(id): Path<i64>) -> ApiResult<Response> {
    st.svc.resume_job(id)?;
    Ok(ok())
}

async fn cancel_job(State(st): State<AppState>, Path(id): Path<i64>) -> ApiResult<Response> {
    st.svc.cancel_job(id)?;
    Ok(ok())
}

#[derive(Deserialize)]
struct MoveBody {
    before: Option<i64>,
}

async fn move_job(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Json(b): Json<MoveBody>,
) -> ApiResult<Response> {
    st.svc.move_job(id, b.before)?;
    Ok(ok())
}

#[derive(Deserialize)]
struct PageQuery {
    #[serde(default)]
    offset: u64,
    limit: Option<u64>,
}

async fn job_items(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<PageQuery>,
) -> ApiResult<Response> {
    st.svc.store.job(id).map_err(ServiceError::from)?;
    let limit = q.limit.unwrap_or(500).min(5000);
    let items = st
        .svc
        .store
        .job_items(id, q.offset, limit)
        .map_err(ServiceError::from)?;
    Ok(Json(items).into_response())
}

async fn job_events(State(st): State<AppState>, Path(id): Path<i64>) -> ApiResult<Response> {
    st.svc.store.job(id).map_err(ServiceError::from)?;
    Ok(Json(st.svc.store.job_events(id).map_err(ServiceError::from)?).into_response())
}

async fn pause_queue(State(st): State<AppState>) -> ApiResult<Response> {
    st.svc.pause_queue()?;
    Ok(ok())
}

async fn resume_queue(State(st): State<AppState>) -> ApiResult<Response> {
    st.svc.resume_queue()?;
    Ok(ok())
}

async fn events(
    State(st): State<AppState>,
    Extension(t): Extension<Transport>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let rx = st.svc.subscribe();
    // The middleware checked the session once; a stream outlives that check, so every later
    // snapshot re-checks it and the stream ends once the session is logged out or replaced.
    let token = (t != Transport::Local)
        .then(|| session_token(&headers))
        .flatten();
    let svc = st.svc.clone();
    // The first poll sends the current snapshot; later ones wait for a change or the next
    // heartbeat. Waking on a heartbeat bounds how long a revoked session's stream stays open when
    // the daemon is idle. The heartbeat is a named event with data, because `EventSource` hides
    // comments and drops events without data, and the UI must tell a stalled stream from an idle one.
    let stream = futures_util::stream::unfold((rx, true), move |(mut rx, first)| {
        let (svc, token) = (svc.clone(), token.clone());
        async move {
            let mut heartbeat = false;
            if !first {
                match tokio::time::timeout(HEARTBEAT_EVERY, rx.changed()).await {
                    Ok(changed) => changed.ok()?,
                    Err(_) => heartbeat = true,
                }
                if let Some(token) = token {
                    let live = blocking(move || Ok(svc.auth.session_live(&token))).await;
                    if !matches!(live, Ok(true)) {
                        return None;
                    }
                }
            }
            let event = if heartbeat {
                Ok(Event::default().event("heartbeat").data("1"))
            } else {
                let snap = rx.borrow_and_update().clone();
                Event::default()
                    .event("snapshot")
                    .json_data(&*snap)
                    .map_err(axum::Error::new)
            };
            Some((event, (rx, false)))
        }
    });
    // Ends with the daemon, or graceful shutdown would wait for every open browser tab.
    let stream = futures_util::StreamExt::take_until(stream, st.svc.stopping());
    Sse::new(stream)
}

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<u64>,
}

async fn audit(State(st): State<AppState>, Query(q): Query<LimitQuery>) -> ApiResult<Response> {
    let rows = st
        .svc
        .store
        .audit_log(q.limit.unwrap_or(200).min(5000))
        .map_err(ServiceError::from)?;
    let rows: Vec<_> = rows
        .into_iter()
        .map(|(at, event, detail)| json!({ "at": at, "event": event, "detail": detail }))
        .collect();
    Ok(Json(rows).into_response())
}

async fn get_settings(State(st): State<AppState>) -> ApiResult<Response> {
    Ok(Json(st.svc.settings()?).into_response())
}

async fn put_settings(
    State(st): State<AppState>,
    body: Result<Json<SettingsUpdate>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Response> {
    let Json(u) = body.map_err(|e| err(StatusCode::BAD_REQUEST, e.body_text()))?;
    Ok(Json(st.svc.update_settings(&u)?).into_response())
}

/// One file of the UI, compiled into the binary (Decision log 2026-11-04): never read from disk or
/// fetched. `build.rs` generates the table from `web/dist`.
struct Asset {
    /// Path without the leading slash; the page itself is "".
    name: &'static str,
    ctype: &'static str,
    body: &'static [u8],
    gzip: Option<&'static [u8]>,
}

include!(concat!(env!("OUT_DIR"), "/ui_assets.rs"));

/// The browser accepts gzip: named in `Accept-Encoding` and not refused with `q=0`.
fn accepts_gzip(headers: &HeaderMap) -> bool {
    header_str(headers, header::ACCEPT_ENCODING).is_some_and(|v| {
        v.split(',').any(|part| {
            let mut bits = part.split(';').map(str::trim);
            let named = bits.next().is_some_and(|c| c.eq_ignore_ascii_case("gzip"));
            named
                && !bits.any(|p| {
                    p.strip_prefix("q=")
                        .and_then(|q| q.parse::<f32>().ok())
                        .is_some_and(|q| q == 0.0)
                })
        })
    })
}

fn asset_response(name: &str, headers: &HeaderMap, cache: Option<&'static str>) -> Response {
    let Some(a) = ASSETS.iter().find(|a| a.name == name) else {
        return err(StatusCode::NOT_FOUND, "not found").into_response();
    };
    let mut r = match a.gzip.filter(|_| accepts_gzip(headers)) {
        Some(gz) => {
            let mut r = gz.into_response();
            r.headers_mut()
                .insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
            r
        }
        None => a.body.into_response(),
    };
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(a.ctype));
    if a.gzip.is_some() {
        h.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
    }
    if let Some(c) = cache {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static(c));
    }
    r
}

async fn asset(file: Option<Path<String>>, headers: HeaderMap) -> Response {
    let name = file.map(|Path(f)| f).unwrap_or_default();
    // Files under `assets/` get the route below, with its caching.
    asset_response(&name, &headers, None)
}

/// Vite puts a content hash in these names, so a cached copy can never be stale.
async fn hashed_asset(Path(file): Path<String>, headers: HeaderMap) -> Response {
    asset_response(
        &format!("assets/{file}"),
        &headers,
        Some("public, max-age=31536000, immutable"),
    )
}

#[cfg(test)]
mod tests {
    use super::{ASSETS, loopback_host};

    /// Removes `//` line comments and `/* */` block comments' lines: comments may name the rules.
    fn without_comment_lines(text: &str) -> String {
        text.lines()
            .filter(|l| {
                let l = l.trim_start();
                !l.starts_with("//") && !l.starts_with("/*") && !l.starts_with('*')
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn source_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read web/src") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                source_files(&path, out);
            } else {
                out.push(path);
            }
        }
    }

    /// The UI rules from the Phase 3 plan, checked on the code we write. A bundle holds React's own
    /// internals (`innerHTML`, namespace URLs), so these words can no longer be checked on it; the
    /// next test checks what can be checked on the shipped bytes, and the browser run (`web/e2e`)
    /// checks for CSP violations and shows a hostile file name as text.
    #[test]
    fn ui_source_keeps_the_safety_rules() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/src");
        let mut files = Vec::new();
        source_files(&src, &mut files);
        assert!(!files.is_empty(), "no UI source in {}", src.display());
        for file in files {
            if file.to_string_lossy().contains(".test.") {
                continue;
            }
            let text = std::fs::read_to_string(&file).expect("read source");
            let code = without_comment_lines(&text);
            let forbidden: &[&str] = if file.extension().is_some_and(|e| e == "css") {
                &["@import url", "http://", "https://"]
            } else {
                &[
                    "dangerouslySetInnerHTML",
                    "innerHTML",
                    "outerHTML",
                    "insertAdjacentHTML",
                    "document.write",
                    "eval(",
                    "new Function",
                    "setAttribute(\"style",
                    "setAttribute(\"on",
                    "javascript:",
                    "http://",
                    "https://",
                    "import(",
                    // Both inject <style> elements, which the CSP blocks (Decision log).
                    "popLayout",
                    "AnimateView",
                ]
            };
            for word in forbidden {
                assert!(!code.contains(word), "{} contains {word:?}", file.display());
            }
        }
    }

    /// What ships: nothing inline in the page, nothing fetched from elsewhere by the page or CSS,
    /// and no code built from strings (the CSP has no `unsafe-eval`, so it could not run anyway).
    #[test]
    fn shipped_assets_keep_the_safety_rules() {
        for a in ASSETS {
            if a.ctype.starts_with("font/") {
                continue;
            }
            let text = std::str::from_utf8(a.body).expect("utf-8");
            let forbidden: &[&str] = match a.ctype {
                t if t.starts_with("text/html") => &[
                    "style=",
                    " onload=",
                    " onclick=",
                    "<script>",
                    "http://",
                    "https://",
                ],
                t if t.starts_with("text/css") => &["@import", "url(http", "url(//"],
                t if t.starts_with("text/javascript") => &["new Function(", "eval("],
                _ => &[],
            };
            for word in forbidden {
                assert!(!text.contains(word), "{:?} contains {word:?}", a.name);
            }
        }
    }

    #[test]
    fn gzip_needs_the_browser_to_accept_it() {
        use super::{HeaderMap, HeaderValue, accepts_gzip, header};
        let with = |v: &'static str| {
            let mut h = HeaderMap::new();
            h.insert(header::ACCEPT_ENCODING, HeaderValue::from_static(v));
            accepts_gzip(&h)
        };
        assert!(with("gzip"));
        assert!(with("br, gzip;q=0.5"));
        assert!(with("GZIP"));
        assert!(!with("gzip;q=0"));
        assert!(!with("br"));
        assert!(!with("x-gzip-not"));
        assert!(!accepts_gzip(&HeaderMap::new()));
    }

    #[test]
    fn loopback_hosts() {
        for h in [
            "127.0.0.1:7890",
            "127.0.0.1",
            "localhost",
            "LOCALHOST:1",
            "[::1]:7890",
            "127.1.2.3:9",
        ] {
            assert!(loopback_host(h), "{h}");
        }
        for h in [
            "evil.example:7890",
            "localhost.evil.example",
            "10.0.0.1:7890",
            "[::2]:1",
            "",
            "localhost:x",
        ] {
            assert!(!loopback_host(h), "{h}");
        }
    }
}
