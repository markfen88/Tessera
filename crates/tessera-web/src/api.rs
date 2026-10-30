//! The JSON API, `/api/v1/` (ADR 0013): the `control-types` queries and
//! commands over HTTP, events as Server-Sent Events, and sessions.

use std::convert::Infallible;
use std::time::{Duration, Instant};

use axum::extract::{Path, Query as Params, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::{self, Stream};
use serde::Deserialize;
use serde_json::json;
use tessera_control_types::{
    Command, CommandError, DeviceKey, Direction, EndpointRef, Query, Reply, RequestBody,
    RoutingChannelsQuery, RoutingCursor, RoutingDevicesQuery, RoutingReceiverQuery,
};
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
        .route("/routing/peer", post(crate::routing_peers::receive))
        .route(
            "/routing/peer-operation",
            get(crate::routing_peers::operation).post(crate::routing_peers::retire),
        )
        .route("/routing/devices", get(routing_devices))
        .route("/routing/carrier-recovery", get(routing_carriers))
        .route("/routing/channels", get(routing_channels))
        .route("/routing/receiver", get(routing_receiver))
        .route("/routing/operation", get(routing_operation))
        .route("/commands", post(command))
        .route("/events", get(events))
        .route("/session", post(login).delete(logout))
}

pub(crate) fn error(code: StatusCode, msg: impl Into<String>) -> Response {
    let message = msg.into();
    let code = match code {
        StatusCode::UNPROCESSABLE_ENTITY
        | StatusCode::UNSUPPORTED_MEDIA_TYPE
        | StatusCode::PAYLOAD_TOO_LARGE => StatusCode::BAD_REQUEST,
        other if other.is_server_error() => StatusCode::INTERNAL_SERVER_ERROR,
        other => other,
    };
    let kind = match code {
        StatusCode::UNAUTHORIZED => json!("unauthorized"),
        StatusCode::FORBIDDEN => json!("permission_denied"),
        StatusCode::TOO_MANY_REQUESTS => json!("rate_limited"),
        StatusCode::NOT_FOUND => json!({ "not_found": { "what": message } }),
        StatusCode::BAD_REQUEST => {
            json!({ "invalid": { "field": "request", "reason": message } })
        }
        _ => json!({ "internal": { "message": message } }),
    };
    (code, Json(json!({ "error": kind, "message": message }))).into_response()
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
        CommandError::RoutingSnapshotExpired => StatusCode::CONFLICT,
        CommandError::ReceiverReadTimeout => StatusCode::GATEWAY_TIMEOUT,
        CommandError::ReceiverReadUnavailable { .. } => StatusCode::BAD_GATEWAY,
        // The command restarts something and was sent unconfirmed; the
        // error carries the impact. Nothing was changed.
        CommandError::ConfirmationRequired { .. } => StatusCode::CONFLICT,
        // The daemon would not send its reply: its frame limit is too small.
        CommandError::ResponseTooLarge { .. } => StatusCode::BAD_GATEWAY,
        CommandError::NotFound { .. } => StatusCode::NOT_FOUND,
        CommandError::PermissionDenied => StatusCode::FORBIDDEN,
        CommandError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        CommandError::Unsupported { .. } => StatusCode::NOT_IMPLEMENTED,
        // The device access lock refuses the change; a wrong PIN.
        CommandError::AccessLocked { .. } => StatusCode::CONFLICT,
        CommandError::IncorrectPin => StatusCode::FORBIDDEN,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

pub(crate) fn daemon_error(e: &ClientError) -> Response {
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
        Ok(Reply::RoutingDevices(page)) => Json(page).into_response(),
        Ok(Reply::RoutingChannels(page)) => Json(page).into_response(),
        Ok(Reply::RoutingReceiver(page)) => Json(page).into_response(),
        Ok(Reply::ReceiverRecovery(review)) => Json(review).into_response(),
        Ok(Reply::RuntimeCarrierRecovery(review)) => Json(review).into_response(),
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

async fn routing_carriers(State(st): State<Shared>, headers: HeaderMap) -> Response {
    query(&st, &headers, Query::InspectRuntimeCarriers).await
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

#[derive(Debug, Deserialize)]
struct DeviceParams {
    cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChannelParams {
    endpoint: String,
    generation: String,
    direction: Direction,
    cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReceiverParams {
    endpoint: String,
    generation: String,
    first: Option<u16>,
}

#[derive(Deserialize)]
struct OperationParams {
    operation: Option<String>,
}

async fn routing_operation(
    State(st): State<Shared>,
    headers: HeaderMap,
    params: Result<Params<OperationParams>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Params(params) = match params {
        Ok(p) => p,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    query(
        &st,
        &headers,
        Query::InspectReceiverOperation {
            operation: params
                .operation
                .map(tessera_control_types::ReceiverExpectation),
        },
    )
    .await
}

async fn routing_receiver(
    State(st): State<Shared>,
    headers: HeaderMap,
    params: Result<Params<ReceiverParams>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if session_of(&st, &headers).is_none() {
        return unauthorized();
    }
    let Params(params) = match params {
        Ok(p) => p,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    query(
        &st,
        &headers,
        Query::RoutingReceiver(RoutingReceiverQuery::new(
            EndpointRef::new(params.endpoint, params.generation),
            params.first.unwrap_or(1),
        )),
    )
    .await
}

async fn routing_devices(
    State(st): State<Shared>,
    headers: HeaderMap,
    params: Result<Params<DeviceParams>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if session_of(&st, &headers).is_none() {
        return unauthorized();
    }
    let Params(params) = match params {
        Ok(p) => p,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    query(
        &st,
        &headers,
        Query::RoutingDevices(RoutingDevicesQuery::new(params.cursor.map(RoutingCursor))),
    )
    .await
}

async fn routing_channels(
    State(st): State<Shared>,
    headers: HeaderMap,
    params: Result<Params<ChannelParams>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if session_of(&st, &headers).is_none() {
        return unauthorized();
    }
    let Params(params) = match params {
        Ok(p) => p,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    query(
        &st,
        &headers,
        Query::RoutingChannels(RoutingChannelsQuery::new(
            EndpointRef::new(params.endpoint, params.generation),
            params.direction,
            params.cursor.map(RoutingCursor),
        )),
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
    // A lock is reset without its PIN only by an administrator on the
    // device (`device-lock.md` §9.1 L2); the daemon refuses it the portal too.
    if matches!(cmd, Command::ResetAccessLock { .. }) {
        return error(
            StatusCode::FORBIDDEN,
            "reset the access lock on the device: tesseractl access reset",
        );
    }
    let kind = cmd.kind();
    let r = call(&st, RequestBody::Command(cmd)).await;
    tracing::info!(session = %s.id, command = kind, ok = r.is_ok(), "api command");
    match r {
        Ok(reply) => Json(json!({ "ok": true, "reply": reply })).into_response(),
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
    body: Result<Json<LoginBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(body) => body,
        Err(e) => return error(e.status(), e.body_text()),
    };
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
