//! Receiver-host federation. This adapter translates references and never opens
//! ARC sockets, retries mutations, or owns saved routing state.
pub mod config;
mod forward;
pub mod transport;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tessera_control_types::{CommandError, Reply, RequestBody, RoutingPeerRequest};
use tesseractl::client::ClientError;

const LIMIT: usize = 1024 * 1024;

#[derive(Clone, Debug)]
struct Tls;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    v: u8,
    result: Result<Reply, CommandError>,
}

/// A typed refusal or loss of trustworthy transport evidence. Neither retries.
#[derive(Debug)]
pub enum TransportError {
    /// The receiver daemon returned a typed refusal.
    Refused(CommandError),
    /// No trustworthy complete reply was received. Mutation outcome is unknown.
    Unavailable,
}

/// Injectable bounded peer transport; the production implementation uses HTTPS.
pub trait PeerTransport: Send + Sync + std::fmt::Debug {
    /// Make exactly one exchange. Implementations must never retry or redirect.
    fn exchange<'a>(
        &'a self,
        peer: &'a config::Peer,
        request: RoutingPeerRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Reply, TransportError>> + Send + 'a>>;
}

/// Explicit peers and bounded, ephemeral correlation. The target actor owns
/// expectation consumption, durable uncertainty and every receiver mutation.
#[derive(Debug)]
pub struct Federation {
    config: config::Config,
    transport: Arc<dyn PeerTransport>,
    records: Mutex<BTreeMap<String, forward::Record>>,
}
impl Default for Federation {
    fn default() -> Self {
        Self {
            config: Default::default(),
            transport: Arc::new(transport::Https),
            records: Default::default(),
        }
    }
}
impl Federation {
    /// Validate explicit configuration. Does not open sockets or read secrets.
    pub fn new(config: config::Config) -> Result<Self, String> {
        Self::with_transport(config, Arc::new(transport::Https))
    }
    /// Construct with an injected exchange for offline verification.
    pub fn with_transport(
        config: config::Config,
        transport: Arc<dyn PeerTransport>,
    ) -> Result<Self, String> {
        config.validate()?;
        Ok(Self {
            config,
            transport,
            records: Default::default(),
        })
    }
}

fn bearer_valid(value: &str) -> bool {
    value.rsplit_once('.').is_some_and(|(id, token)| {
        config::name(id) && token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

/// Only the routing machine API is served on the dedicated HTTPS listener.
pub fn machine_router(st: crate::Shared) -> axum::Router {
    axum::Router::new()
        .route("/api/v1/routing/peer", axum::routing::post(receive))
        .layer(axum::middleware::from_fn(crate::security_headers))
        .with_state(st)
}

/// Machine API: only accepted on the TLS listener. Credentials are re-read for
/// immediate revocation; browser sessions and forwarding headers grant no access.
pub(crate) async fn receive(State(st): State<crate::Shared>, request: Request) -> Response {
    if request.extensions().get::<Tls>().is_none()
        || request.headers().contains_key("origin")
        || request.headers().contains_key("cookie")
    {
        return crate::api::error(
            StatusCode::FORBIDDEN,
            "routing peers require the machine TLS endpoint",
        );
    }
    let headers = request.headers();
    let bearer = if headers.get_all("authorization").iter().count() == 1 {
        headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
    } else {
        None
    };
    let Some(bearer) = bearer.filter(|value| bearer_valid(value)) else {
        return crate::api::error(StatusCode::UNAUTHORIZED, "routing credential required");
    };
    let Some((id, _)) = bearer.rsplit_once('.') else {
        unreachable!()
    };
    let Some(credential) = st
        .routing_peers
        .config
        .credential
        .iter()
        .find(|c| c.id == id)
    else {
        return crate::api::error(StatusCode::UNAUTHORIZED, "routing credential refused");
    };
    let hash = match transport::protected(&credential.hash_file, 128) {
        Ok(hash) => hash,
        Err(_) => return crate::api::error(StatusCode::UNAUTHORIZED, "routing credential refused"),
    };
    if !std::str::from_utf8(&hash).is_ok_and(|expected| {
        crate::auth::same(expected.trim(), &transport::digest(bearer.as_bytes()))
    }) {
        return crate::api::error(StatusCode::UNAUTHORIZED, "routing credential refused");
    }
    let id = credential.id.clone();
    let receiver_wire_id = credential.receiver_wire_id.clone();
    let bytes = match axum::body::to_bytes(request.into_body(), 65536).await {
        Ok(bytes) => bytes,
        Err(_) => return crate::api::error(StatusCode::BAD_REQUEST, "routing request too large"),
    };
    let request: RoutingPeerRequest = match serde_json::from_slice(&bytes) {
        Ok(request) => request,
        Err(_) => {
            return crate::api::error(StatusCode::BAD_REQUEST, "invalid routing peer request");
        }
    };
    if matches!(
        request,
        RoutingPeerRequest::Receiver(_) | RoutingPeerRequest::Route { .. }
    ) {
        // The gateway scopes the credential; the daemon scopes the resulting
        // expectation to this named principal and revalidates its own binding.
        let allowed = match st.routing_peers.devices(&st, None).await {
            Ok(devices) => {
                devices
                    .iter()
                    .filter(|d| d.wire_id.as_deref() == Some(receiver_wire_id.as_str()))
                    .count()
                    == 1
                    && devices.iter().any(|d| {
                        d.local_device.is_some()
                            && d.wire_id.as_deref() == Some(receiver_wire_id.as_str())
                            && match &request {
                                RoutingPeerRequest::Receiver(q) => q.endpoint == d.endpoint,
                                _ => true,
                            }
                    })
            }
            _ => false,
        };
        if !allowed {
            return crate::api::error(
                StatusCode::FORBIDDEN,
                "credential does not cover this receiver identity",
            );
        }
    }
    let result = crate::local_call(
        &st,
        RequestBody::RoutingPeer {
            principal: id.clone(),
            request,
        },
    )
    .await;
    tracing::info!(principal = %id, ok = result.is_ok(), "authenticated routing peer");
    let result = result.map_err(|error| match error {
        ClientError::Command(error) => error,
        _ => CommandError::Internal {
            message: "receiver daemon reply unavailable; inspect operation before continuing"
                .into(),
        },
    });
    let bytes = match serde_json::to_vec(&Envelope { v: 1, result }) {
        Ok(bytes) if bytes.len() <= LIMIT => bytes,
        _ => {
            return crate::api::error(
                StatusCode::BAD_GATEWAY,
                "routing reply exceeds frame budget",
            );
        }
    };
    ([("content-type", "application/json")], bytes).into_response()
}

/// Return the latest receiver-host accounting without submitting a mutation.
pub(crate) async fn operation(
    State(st): State<crate::Shared>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(params): axum::extract::Query<std::collections::BTreeMap<String, String>>,
) -> Response {
    if crate::session_of(&st, &headers).is_none() {
        return crate::api::error(StatusCode::UNAUTHORIZED, "log in first");
    }
    let Some(token) = params.get("operation") else {
        return crate::api::error(StatusCode::BAD_REQUEST, "operation is required");
    };
    match st.routing_peers.operation(&st, token).await {
        Ok(reply) => axum::Json(reply).into_response(),
        Err(error) => crate::api::daemon_error(&error),
    }
}

pub(crate) async fn retire(
    State(st): State<crate::Shared>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(params): axum::extract::Query<std::collections::BTreeMap<String, String>>,
) -> Response {
    let Some((_, session)) = crate::session_of(&st, &headers) else {
        return crate::api::error(StatusCode::UNAUTHORIZED, "log in first");
    };
    if !headers
        .get("x-csrf-token")
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| crate::auth::same(h, &session.csrf))
    {
        return crate::api::error(StatusCode::FORBIDDEN, "missing or wrong X-CSRF-Token");
    }
    let Some(token) = params.get("operation") else {
        return crate::api::error(StatusCode::BAD_REQUEST, "operation is required");
    };
    match st.routing_peers.retire(token).await {
        Ok(reply) => axum::Json(reply).into_response(),
        Err(error) => crate::api::daemon_error(&error),
    }
}
