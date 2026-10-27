//! The dashboard and its API, on the LAN only.
//!
//! | Route | |
//! |---|---|
//! | `GET /healthz` | `ok`, no login |
//! | `GET /` | the dashboard (one self-contained HTML page) |
//! | `GET /api/status` | state, zones and devices, presence |
//! | `GET /api/events?limit=50` | the event log, newest first (max 500) |
//! | `POST /api/arm` | `{"mode": "away" \| "home" \| "night"}` |
//! | `POST /api/disarm` | `{"pin": "..."}` |
//! | `GET /api/snapshot/<frigate event id>` | the snapshot, proxied from Frigate |
//!
//! Everything but `/healthz` needs the dashboard password (HTTP Basic; any
//! user name). Disarming also needs the PIN. Wrong passwords and PINs lock
//! the client out for a while. POST bodies must be JSON and a browser's
//! `Origin` must match, which stops other web pages from submitting forms
//! with the cached login (CSRF).

pub mod auth;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{ConnectInfo, Extension, Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tracing::{info, warn};

use crate::core::{Command, Mode, Ts};
use crate::db::EventLog;
use crate::frigate::FrigateClient;
use crate::notify::safe_path_segment;
use crate::run::{Clock, CoreHandle};
use auth::{basic_auth_ok, constant_time_eq, Throttle};

const DASHBOARD: &str = include_str!("dashboard.html");

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WebConfig {
    /// Address to listen on, unless the variable named by `bind_env` is set
    /// (Compose sets HOMEGUARD_BIND to HUB_LAN_IP:8080).
    pub bind: String,
    pub bind_env: String,
    /// Environment variable holding the dashboard password.
    pub password_env: String,
    /// Environment variable holding the disarm PIN.
    pub pin_env: String,
}

impl Default for WebConfig {
    fn default() -> Self {
        WebConfig {
            bind: "127.0.0.1:8080".into(),
            bind_env: "HOMEGUARD_BIND".into(),
            password_env: "HOMEGUARD_DASHBOARD_PASSWORD".into(),
            pin_env: "HOMEGUARD_DISARM_PIN".into(),
        }
    }
}

pub struct WebState {
    pub core: CoreHandle,
    pub log: EventLog,
    pub password: String,
    pub pin: Option<String>,
    pub frigate: FrigateClient,
    pub clock: Clock,
    /// Wrong dashboard passwords.
    logins: Mutex<Throttle>,
    /// Wrong disarm PINs, counted separately so a correct password does
    /// not reset them.
    pins: Mutex<Throttle>,
}

impl WebState {
    pub fn new(
        core: CoreHandle,
        log: EventLog,
        password: String,
        pin: Option<String>,
        frigate_url: &str,
    ) -> anyhow::Result<WebState> {
        Ok(WebState {
            core,
            log,
            password,
            pin,
            frigate: FrigateClient::new(frigate_url)?,
            clock: Clock::start(),
            logins: Mutex::new(lockout()),
            pins: Mutex::new(lockout()),
        })
    }

    fn check(throttle: &Mutex<Throttle>, client: &str, now: Ts) -> bool {
        throttle
            .lock()
            .map(|mut t| t.allowed(client, now))
            .unwrap_or(false)
    }

    fn fail(throttle: &Mutex<Throttle>, client: &str, now: Ts) {
        if let Ok(mut t) = throttle.lock() {
            t.failure(client, now);
        }
    }
}

/// 5 failures within 10 minutes: locked out for 15 minutes.
fn lockout() -> Throttle {
    Throttle::new(5, Duration::from_secs(600), Duration::from_secs(900))
}

pub fn router(state: Arc<WebState>) -> Router {
    let protected = Router::new()
        .route("/", get(dashboard))
        .route("/api/status", get(status))
        .route("/api/events", get(events))
        .route("/api/arm", post(arm))
        .route("/api/disarm", post(disarm))
        .route("/api/snapshot/{id}", get(snapshot))
        .layer(middleware::from_fn_with_state(state.clone(), guard));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(protected)
        .with_state(state)
}

/// Binds and serves until the process exits.
pub async fn serve(state: Arc<WebState>, bind: &str) -> anyhow::Result<()> {
    let addr: SocketAddr = bind
        .parse()
        .map_err(|_| anyhow::anyhow!("dashboard address {bind:?} is not IP:port"))?;
    if addr.ip().is_unspecified() {
        warn!(%addr, "the dashboard listens on every interface; set HUB_LAN_IP to bind it to the LAN only");
    }
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("binding the dashboard to {addr}: {e}"))?;
    info!(%addr, "dashboard listening");
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

fn client_key(request: &Request) -> String {
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip().to_string())
        .unwrap_or_else(|| "local".into())
}

/// The client's IP, found by the guard, for the handlers' own throttling.
#[derive(Debug, Clone)]
struct ClientKey(String);

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// Login, lockout and cross-site checks for every protected route.
async fn guard(State(state): State<Arc<WebState>>, mut request: Request, next: Next) -> Response {
    let client = client_key(&request);
    request.extensions_mut().insert(ClientKey(client.clone()));
    if !WebState::check(&state.logins, &client, state.clock.now()) {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "too many failed attempts; try again later",
        );
    }
    let header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok());
    if !basic_auth_ok(header, &state.password) {
        if header.is_some() {
            warn!(%client, "dashboard: wrong password");
            WebState::fail(&state.logins, &client, state.clock.now());
        }
        return (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                "Basic realm=\"homeguard\", charset=\"UTF-8\"",
            )],
            "login required",
        )
            .into_response();
    }
    if let Ok(mut t) = state.logins.lock() {
        t.success(&client);
    }
    if request.method() == Method::POST && !same_origin(request.headers()) {
        warn!(%client, "dashboard: cross-site request refused");
        return error(StatusCode::FORBIDDEN, "cross-site request refused");
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; img-src 'self'; style-src 'unsafe-inline'; \
             script-src 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'",
        ),
    );
    response
}

/// A browser sends `Origin` on POST; it must name this host. Requests
/// without one (curl, scripts) are fine: they cannot ride on a browser's
/// cached login.
pub fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()) else {
        return true;
    };
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return false;
    };
    let origin_host = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .unwrap_or("");
    origin_host.eq_ignore_ascii_case(host)
}

async fn dashboard() -> Html<&'static str> {
    Html(DASHBOARD)
}

async fn status(State(state): State<Arc<WebState>>) -> Response {
    Json(state.core.snapshot().status).into_response()
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    limit: Option<u32>,
}

async fn events(State(state): State<Arc<WebState>>, Query(q): Query<EventsQuery>) -> Response {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    match state.log.latest(limit).await {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => {
            warn!(error = %format!("{e:#}"), "reading the event log failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, "event log unavailable")
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArmRequest {
    mode: String,
}

async fn arm(State(state): State<Arc<WebState>>, Json(body): Json<ArmRequest>) -> Response {
    let Some(mode) = Mode::parse(&body.mode) else {
        return error(StatusCode::BAD_REQUEST, "mode must be away, home or night");
    };
    match state
        .core
        .command(Command::Arm(mode), "dashboard".into())
        .await
    {
        Ok(snapshot) => Json(snapshot.status).into_response(),
        Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, &e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DisarmRequest {
    pin: String,
}

async fn disarm(
    State(state): State<Arc<WebState>>,
    Extension(ClientKey(client)): Extension<ClientKey>,
    Json(body): Json<DisarmRequest>,
) -> Response {
    let Some(pin) = &state.pin else {
        return error(
            StatusCode::FORBIDDEN,
            "disarming from the dashboard needs HOMEGUARD_DISARM_PIN to be set",
        );
    };
    if !WebState::check(&state.pins, &client, state.clock.now()) {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "too many wrong PINs; try again later",
        );
    }
    if !constant_time_eq(body.pin.trim().as_bytes(), pin.as_bytes()) {
        warn!(%client, "dashboard: wrong disarm PIN");
        WebState::fail(&state.pins, &client, state.clock.now());
        return error(StatusCode::FORBIDDEN, "wrong PIN");
    }
    if let Ok(mut t) = state.pins.lock() {
        t.success(&client);
    }
    match state
        .core
        .command(Command::Disarm, "dashboard".into())
        .await
    {
        Ok(snapshot) => Json(snapshot.status).into_response(),
        Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, &e.to_string()),
    }
}

async fn snapshot(State(state): State<Arc<WebState>>, Path(id): Path<String>) -> Response {
    if !safe_path_segment(&id) {
        return error(StatusCode::BAD_REQUEST, "bad snapshot id");
    }
    let fetched = state.frigate.event_snapshot(&id).await.ok();
    match fetched {
        Some(bytes) => ([(header::CONTENT_TYPE, "image/jpeg")], Body::from(bytes)).into_response(),
        None => error(StatusCode::NOT_FOUND, "snapshot unavailable"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::Snapshot;
    use crate::status::Status;
    use axum::body::to_bytes;
    use base64::Engine as _;
    use tower::ServiceExt;

    const TEST_PIN: &str = "4321";
    const WRONG_PIN: &str = "0000";

    fn pin_body(pin: &str) -> String {
        format!(r#"{{"pin":"{pin}"}}"#)
    }

    fn status() -> Status {
        Status {
            state: "armed".into(),
            mode: Some(Mode::Away),
            cause: None,
            snapshot_ref: None,
            zones: vec![],
            presence: vec![],
        }
    }

    async fn make_app(
        pin: Option<&str>,
    ) -> (
        Router,
        tokio::sync::mpsc::Receiver<(Command, String)>,
        EventLog,
    ) {
        let (core, commands) = CoreHandle::for_tests(Snapshot::new(status(), vec![]));
        let log = EventLog::in_memory().await.unwrap();
        let state = WebState::new(
            core,
            log.clone(),
            "s3cret".into(),
            pin.map(str::to_string),
            "http://127.0.0.1:9",
        )
        .unwrap();
        (router(Arc::new(state)), commands, log)
    }

    fn auth(password: &str) -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("admin:{password}"))
        )
    }

    fn get(path: &str, password: Option<&str>) -> Request {
        let mut builder = axum::http::Request::get(path);
        if let Some(p) = password {
            builder = builder.header(header::AUTHORIZATION, auth(p));
        }
        builder.body(Body::empty()).unwrap()
    }

    fn post(path: &str) -> axum::http::request::Builder {
        axum::http::Request::post(path)
            .header(header::AUTHORIZATION, auth("s3cret"))
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::HOST, "192.168.1.10:8080")
    }

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn send(builder: axum::http::request::Builder, body: impl Into<String>) -> Request {
        builder.body(Body::from(body.into())).unwrap()
    }

    #[tokio::test]
    async fn health_is_public_everything_else_needs_the_password() {
        let (app, _, _) = make_app(None).await;
        let r = app.clone().oneshot(get("/healthz", None)).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let r = app.clone().oneshot(get("/api/status", None)).await.unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        assert!(r.headers().contains_key(header::WWW_AUTHENTICATE));
        let r = app.clone().oneshot(get("/", Some("wrong"))).await.unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        let r = app.clone().oneshot(get("/", Some("s3cret"))).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let html = to_bytes(r.into_body(), 1 << 20).await.unwrap();
        let html = String::from_utf8(html.to_vec()).unwrap();
        assert!(html.contains("<title>homeguard</title>"));
        // Self-contained: no external scripts or styles.
        assert!(!html.contains("src=\"http"));
        assert!(!html.contains("href=\"http"));
    }

    #[tokio::test]
    async fn status_and_events() {
        let (app, _, log) = make_app(None).await;
        let r = app
            .clone()
            .oneshot(get("/api/status", Some("s3cret")))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(body_json(r).await["state"], "armed");

        let rows: Vec<_> = (0..60)
            .map(|i| crate::db::NewEvent::action(i, &crate::core::Action::log(format!("e{i}"))))
            .collect();
        log.insert(&rows).await.unwrap();
        let r = app
            .clone()
            .oneshot(get("/api/events", Some("s3cret")))
            .await
            .unwrap();
        let events = body_json(r).await;
        assert_eq!(events.as_array().unwrap().len(), 50);
        assert_eq!(events[0]["text"], "e59");
        let r = app
            .clone()
            .oneshot(get("/api/events?limit=3", Some("s3cret")))
            .await
            .unwrap();
        assert_eq!(body_json(r).await.as_array().unwrap().len(), 3);
        let r = app
            .clone()
            .oneshot(get("/api/events?limit=0", Some("s3cret")))
            .await
            .unwrap();
        assert_eq!(body_json(r).await.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn arm_sends_the_command() {
        let (app, mut commands, _) = make_app(None).await;
        let r = app
            .clone()
            .oneshot(send(post("/api/arm"), r#"{"mode":"night"}"#))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(
            commands.recv().await.unwrap(),
            (Command::Arm(Mode::Night), "dashboard".to_string())
        );
        let r = app
            .clone()
            .oneshot(send(post("/api/arm"), r#"{"mode":"party"}"#))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn disarm_needs_the_pin() {
        let (app, mut commands, _) = make_app(Some(TEST_PIN)).await;
        let r = app
            .clone()
            .oneshot(send(post("/api/disarm"), pin_body(WRONG_PIN)))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert!(commands.try_recv().is_err());
        let r = app
            .clone()
            .oneshot(send(post("/api/disarm"), pin_body(TEST_PIN)))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(commands.recv().await.unwrap().0, Command::Disarm);

        let (no_pin, mut commands, _) = make_app(None).await;
        let r = no_pin
            .oneshot(send(post("/api/disarm"), pin_body(TEST_PIN)))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert!(commands.try_recv().is_err());
    }

    #[tokio::test]
    async fn repeated_failures_lock_the_client_out() {
        let (app, _, _) = make_app(Some(TEST_PIN)).await;
        for _ in 0..5 {
            let r = app
                .clone()
                .oneshot(get("/api/status", Some("guess")))
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        }
        // Even the right password is refused during the lockout.
        let r = app
            .clone()
            .oneshot(get("/api/status", Some("s3cret")))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn wrong_pins_lock_the_client_out() {
        let (app, mut commands, _) = make_app(Some(TEST_PIN)).await;
        for _ in 0..5 {
            app.clone()
                .oneshot(send(post("/api/disarm"), pin_body(WRONG_PIN)))
                .await
                .unwrap();
        }
        let r = app
            .clone()
            .oneshot(send(post("/api/disarm"), pin_body(TEST_PIN)))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(commands.try_recv().is_err());
    }

    #[tokio::test]
    async fn cross_site_posts_and_non_json_bodies_are_refused() {
        let (app, mut commands, _) = make_app(Some(TEST_PIN)).await;
        let evil = post("/api/arm").header(header::ORIGIN, "http://evil.example");
        let r = app
            .clone()
            .oneshot(send(evil, r#"{"mode":"away"}"#))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let form = axum::http::Request::post("/api/disarm")
            .header(header::AUTHORIZATION, auth("s3cret"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("pin={TEST_PIN}")))
            .unwrap();
        let r = app.clone().oneshot(form).await.unwrap();
        assert_eq!(r.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let same = post("/api/arm").header(header::ORIGIN, "http://192.168.1.10:8080");
        let r = app
            .clone()
            .oneshot(send(same, r#"{"mode":"away"}"#))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(commands.recv().await.unwrap().0, Command::Arm(Mode::Away));
    }

    #[tokio::test]
    async fn snapshot_ids_are_validated() {
        let (app, _, _) = make_app(None).await;
        let r = app
            .clone()
            .oneshot(get("/api/snapshot/..%2F..%2Fetc", Some("s3cret")))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        // Frigate is not running in tests: a clean 404, not an error page.
        let r = app
            .clone()
            .oneshot(get("/api/snapshot/1793-abc", Some("s3cret")))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn origin_check() {
        let mut h = HeaderMap::new();
        assert!(same_origin(&h));
        h.insert(header::HOST, "10.0.0.2:8080".parse().unwrap());
        h.insert(header::ORIGIN, "http://10.0.0.2:8080".parse().unwrap());
        assert!(same_origin(&h));
        h.insert(header::ORIGIN, "http://10.0.0.2:8081".parse().unwrap());
        assert!(!same_origin(&h));
        h.insert(header::ORIGIN, "null".parse().unwrap());
        assert!(!same_origin(&h));
    }
}
