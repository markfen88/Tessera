//! The JSON API, `/api/v1/` (ADR 0013): the `control-types` queries and
//! commands over HTTP, events as Server-Sent Events, and sessions.

use std::convert::Infallible;
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::{self, Stream};
use serde::Deserialize;
use serde_json::json;
use tessera_control_types::{Command, CommandError, DeviceKey, Query, Reply, RequestBody};
use tesseractl::client::ClientError;
use tokio::sync::broadcast::error::RecvError;

use crate::auth::{Stored, same, verify_password};
use crate::backend::HubMsg;
use crate::{Shared, call, clear_cookie, peer, session_cookie, session_of};

/// The API routes, relative to `/api/v1`.
pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/status", get(status))
        .route("/metrics", get(metrics))
        .route("/config", get(config))
        .route("/devices/{key}", get(device))
        .route("/commands", post(command))
        .route("/events", get(events))
        .route("/session", post(login).delete(logout))
}

fn error(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({ "error": { "message": msg.into() } }))).into_response()
}

fn unauthorized() -> Response {
    error(
        StatusCode::UNAUTHORIZED,
        "log in first (POST /api/v1/session)",
    )
}

/// HTTP status for a refusal of the daemon.
fn code_of(e: &CommandError) -> StatusCode {
    match e {
        CommandError::Invalid { .. } => StatusCode::BAD_REQUEST,
        CommandError::Locked { .. } => StatusCode::CONFLICT,
        // The command restarts something and was sent unconfirmed; the
        // error carries the impact. Nothing was changed.
        CommandError::ConfirmationRequired { .. } => StatusCode::CONFLICT,
        // The daemon would not send its reply: its frame limit is too small.
        CommandError::ResponseTooLarge { .. } => StatusCode::BAD_GATEWAY,
        CommandError::NotFound { .. } => StatusCode::NOT_FOUND,
        CommandError::PermissionDenied => StatusCode::FORBIDDEN,
        CommandError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        CommandError::Unsupported { .. } => StatusCode::NOT_IMPLEMENTED,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn daemon_error(e: &ClientError) -> Response {
    match e {
        ClientError::Command(c) => (
            code_of(c),
            Json(json!({ "error": c, "message": c.to_string() })),
        )
            .into_response(),
        other => error(StatusCode::BAD_GATEWAY, other.to_string()),
    }
}

async fn query(st: &Shared, headers: &HeaderMap, q: Query) -> Response {
    if session_of(st, headers).is_none() {
        return unauthorized();
    }
    match call(st, RequestBody::Query(q)).await {
        Ok(Reply::Status(s)) => Json(s).into_response(),
        Ok(Reply::Metrics(m)) => Json(m).into_response(),
        Ok(Reply::Device(d)) => Json(d).into_response(),
        Ok(Reply::EffectiveConfig(t)) => (
            [(header::CONTENT_TYPE, "application/toml; charset=utf-8")],
            t,
        )
            .into_response(),
        Ok(other) => error(
            StatusCode::BAD_GATEWAY,
            format!("unexpected reply from the daemon: {other:?}"),
        ),
        Err(e) => daemon_error(&e),
    }
}

async fn status(State(st): State<Shared>, headers: HeaderMap) -> Response {
    query(&st, &headers, Query::Status).await
}

async fn metrics(State(st): State<Shared>, headers: HeaderMap) -> Response {
    query(&st, &headers, Query::Metrics).await
}

async fn config(State(st): State<Shared>, headers: HeaderMap) -> Response {
    query(&st, &headers, Query::EffectiveConfig).await
}

async fn device(State(st): State<Shared>, headers: HeaderMap, Path(key): Path<String>) -> Response {
    query(
        &st,
        &headers,
        Query::Device {
            device: DeviceKey(key),
        },
    )
    .await
}

/// `POST /commands`: one `Command` (JSON), with the `X-CSRF-Token` header.
async fn command(State(st): State<Shared>, headers: HeaderMap, body: String) -> Response {
    let Some((_, s)) = session_of(&st, &headers) else {
        return unauthorized();
    };
    let csrf = headers
        .get("x-csrf-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !same(csrf, &s.csrf) {
        return error(StatusCode::FORBIDDEN, "missing or wrong X-CSRF-Token");
    }
    let cmd: Command = match serde_json::from_str(&body) {
        Ok(c) => c,
        Err(e) => return error(StatusCode::BAD_REQUEST, format!("not a command: {e}")),
    };
    let kind = cmd.kind();
    let r = call(&st, RequestBody::Command(cmd)).await;
    tracing::info!(session = %s.id, command = kind, ok = r.is_ok(), "api command");
    match r {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => daemon_error(&e),
    }
}

/// `GET /events`: Server-Sent Events. Messages: `event` (an `Event` as JSON)
/// and `resync` (events were lost; re-read the status).
async fn events(State(st): State<Shared>, headers: HeaderMap) -> Response {
    if session_of(&st, &headers).is_none() {
        return unauthorized();
    }
    let rx = st.hub.subscribe();
    let s = stream::unfold(rx, |mut rx| async move {
        let ev = match rx.recv().await {
            Ok(HubMsg::Event(e)) => SseEvent::default()
                .event("event")
                .json_data(&e)
                .unwrap_or_else(|_| SseEvent::default().event("resync").data("")),
            Ok(HubMsg::Resync) | Err(RecvError::Lagged(_)) => {
                SseEvent::default().event("resync").data("")
            }
            Err(RecvError::Closed) => return None,
        };
        Some((Ok::<_, Infallible>(ev), rx))
    });
    sse(s).into_response()
}

fn sse<S>(s: S) -> impl IntoResponse
where
    S: Stream<Item = Result<SseEvent, Infallible>> + Send + 'static,
{
    Sse::new(s).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

#[derive(Debug, Deserialize)]
struct LoginBody {
    password: String,
}

/// `POST /session` with `{"password": ...}`: sets the cookie, returns the
/// CSRF token for later state-changing requests.
async fn login(
    State(st): State<Shared>,
    ext: axum::http::Extensions,
    Json(body): Json<LoginBody>,
) -> Response {
    let addr = peer(&ext);
    let now = Instant::now();
    if !st.limiter.allowed(addr, now) {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "too many failed logins; wait a minute",
        );
    }
    let ok = match st.password.read() {
        Stored::Hash(h) => {
            let pw = body.password;
            tokio::task::spawn_blocking(move || verify_password(&pw, &h))
                .await
                .unwrap_or(false)
        }
        Stored::Missing => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "no password is set: run `sudo tessera-web set-password` on the device",
            );
        }
        Stored::Unreadable(m) => return error(StatusCode::INTERNAL_SERVER_ERROR, m),
    };
    if !ok {
        st.limiter.failed(addr, now);
        tracing::warn!(%addr, "failed api login");
        return error(StatusCode::UNAUTHORIZED, "wrong password");
    }
    let (token, s) = st.sessions.create(now);
    tracing::info!(session = %s.id, %addr, "api login");
    (
        [(header::SET_COOKIE, session_cookie(&token, st.cookie_secure))],
        Json(json!({ "csrf": s.csrf })),
    )
        .into_response()
}

async fn logout(State(st): State<Shared>, headers: HeaderMap) -> Response {
    if let Some((token, s)) = session_of(&st, &headers) {
        let csrf = headers
            .get("x-csrf-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !same(csrf, &s.csrf) {
            return error(StatusCode::FORBIDDEN, "missing or wrong X-CSRF-Token");
        }
        st.sessions.remove(&token);
    }
    (
        [(header::SET_COOKIE, clear_cookie(st.cookie_secure))],
        StatusCode::NO_CONTENT,
    )
        .into_response()
}
