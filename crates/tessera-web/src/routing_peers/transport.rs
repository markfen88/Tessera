//! One bounded HTTP/1 exchange over pinned TLS. No redirects, proxies or retries.
use super::{Envelope, LIMIT, PeerTransport, TransportError, config::Peer};
use axum::body::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::rt::TokioIo;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tessera_control_types::{Reply, RoutingPeerRequest};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

pub(super) fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// Credentials and keys are never included in errors, tracing or Debug output.
pub(super) fn protected(path: &Path, max: usize) -> Result<Vec<u8>, String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| "cannot open routing credential/key file")?;
    let metadata = file
        .metadata()
        .map_err(|_| "cannot inspect routing credential/key file")?;
    if !metadata.is_file()
        || metadata.len() > max as u64
        || metadata.permissions().mode() & 0o037 != 0
        || (metadata.uid() != 0 && metadata.uid() != nix::unistd::geteuid().as_raw())
    {
        return Err("routing credential/key must be a bounded protected regular file owned by root or this service".into());
    }
    let mut bytes = Vec::new();
    file.take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read routing credential/key file")?;
    if bytes.len() > max {
        return Err("routing credential/key file too large".into());
    }
    Ok(bytes)
}

fn certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
    let metadata =
        std::fs::metadata(path).map_err(|_| "cannot inspect routing certificate file")?;
    if !metadata.is_file() || metadata.len() > 64 * 1024 {
        return Err("routing certificate file too large".into());
    }
    let certs = CertificateDer::pem_file_iter(path)
        .map_err(|_| "cannot read routing certificate")?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "invalid routing certificate")?;
    if certs.is_empty() {
        return Err("empty routing certificate".into());
    }
    Ok(certs)
}

#[derive(Debug)]
struct Pinned {
    leaf: String,
    verifier: Arc<dyn rustls::client::danger::ServerCertVerifier>,
}
impl rustls::client::danger::ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if digest(end.as_ref()) != self.leaf {
            return Err(rustls::Error::General(
                "routing server certificate pin mismatch".into(),
            ));
        }
        self.verifier
            .verify_server_cert(end, intermediates, name, ocsp, now)
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.verifier.verify_tls12_signature(message, cert, dss)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.verifier.verify_tls13_signature(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.verifier.supported_verify_schemes()
    }
}

fn connector(peer: &Peer) -> Result<TlsConnector, String> {
    let certs = certificates(&peer.certificate_file)?;
    if digest(certs[0].as_ref()) != peer.server_sha256 {
        return Err("routing certificate file does not match configured pin".into());
    }
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(certs[0].clone())
        .map_err(|_| "invalid routing trust certificate")?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
        Arc::new(roots),
        provider.clone(),
    )
    .build()
    .map_err(|_| "invalid routing verifier")?;
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| "TLS unavailable")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned {
            leaf: peer.server_sha256.clone(),
            verifier,
        }))
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

/// Production peer transport; one fresh connection and at most one HTTP send.
#[derive(Debug, Default)]
pub struct Https;
impl PeerTransport for Https {
    fn exchange<'a>(
        &'a self,
        peer: &'a Peer,
        request: RoutingPeerRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Reply, TransportError>> + Send + 'a>> {
        Box::pin(async move {
            tokio::time::timeout(Duration::from_secs(6), exchange(peer, request))
                .await
                .unwrap_or(Err(TransportError::Unavailable))
        })
    }
}

async fn exchange(peer: &Peer, request: RoutingPeerRequest) -> Result<Reply, TransportError> {
    // Configuration validation and credential reads precede even DNS/TCP.
    let connector = connector(peer).map_err(|_| TransportError::Unavailable)?;
    let credential =
        protected(&peer.credential_file, 256).map_err(|_| TransportError::Unavailable)?;
    let credential = std::str::from_utf8(&credential)
        .map_err(|_| TransportError::Unavailable)?
        .trim();
    if !super::bearer_valid(credential) {
        return Err(TransportError::Unavailable);
    }
    let uri: axum::http::Uri = peer
        .origin
        .parse()
        .map_err(|_| TransportError::Unavailable)?;
    let host = uri
        .host()
        .ok_or(TransportError::Unavailable)?
        .trim_matches(['[', ']']);
    let server_name =
        ServerName::try_from(host.to_owned()).map_err(|_| TransportError::Unavailable)?;
    let tcp = TcpStream::connect((host, uri.port_u16().unwrap_or(443)))
        .await
        .map_err(|_| TransportError::Unavailable)?;
    let tls = connector
        .connect(server_name, tcp)
        .await
        .map_err(|_| TransportError::Unavailable)?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .map_err(|_| TransportError::Unavailable)?;
    let task = tokio::spawn(async move {
        let _ = connection.await;
    });
    // Aborting the HTTP driver on every return also closes the socket on a timeout.
    struct Driver(tokio::task::JoinHandle<()>);
    impl Drop for Driver {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _driver = Driver(task);
    let bytes = serde_json::to_vec(&request).map_err(|_| TransportError::Unavailable)?;
    if bytes.len() > 65536 {
        return Err(TransportError::Unavailable);
    }
    let mut authorization = axum::http::HeaderValue::from_str(&format!("Bearer {credential}"))
        .map_err(|_| TransportError::Unavailable)?;
    authorization.set_sensitive(true);
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v1/routing/peer")
        .header(
            "host",
            uri.authority().ok_or(TransportError::Unavailable)?.as_str(),
        )
        .header("authorization", authorization)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(bytes)))
        .map_err(|_| TransportError::Unavailable)?;
    let response = sender
        .send_request(request)
        .await
        .map_err(|_| TransportError::Unavailable)?;
    if response.status() != axum::http::StatusCode::OK {
        return Err(TransportError::Unavailable);
    }
    let bytes = Limited::new(response.into_body(), LIMIT)
        .collect()
        .await
        .map_err(|_| TransportError::Unavailable)?
        .to_bytes();
    let envelope: Envelope =
        serde_json::from_slice(&bytes).map_err(|_| TransportError::Unavailable)?;
    if envelope.v != 1 {
        return Err(TransportError::Unavailable);
    }
    envelope.result.map_err(TransportError::Refused)
}

/// Prepared optional machine listener. The marker is set only after TLS succeeds.
pub struct Listener {
    listener: TcpListener,
    acceptor: TlsAcceptor,
}
impl std::fmt::Debug for Listener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener")
            .field("listener", &self.listener)
            .finish_non_exhaustive()
    }
}
impl Listener {
    /// Bound address (including the selected port when binding port zero in tests).
    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }
    /// Read bounded key material and bind the configured TLS address.
    pub async fn bind(config: &super::config::Config) -> Result<Option<Self>, String> {
        let Some(address) = config.listen else {
            return Ok(None);
        };
        config.validate()?;
        let certs = certificates(
            config
                .certificate_file
                .as_ref()
                .ok_or("missing routing certificate")?,
        )?;
        let bytes = protected(
            config
                .private_key_file
                .as_ref()
                .ok_or("missing routing key")?,
            64 * 1024,
        )?;
        let key =
            PrivateKeyDer::from_pem_slice(&bytes).map_err(|_| "invalid routing private key")?;
        let server = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| "TLS unavailable")?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|_| "invalid routing certificate/key pair")?;
        let listener = TcpListener::bind(address)
            .await
            .map_err(|_| "cannot bind routing TLS listener")?;
        Ok(Some(Self {
            listener,
            acceptor: TlsAcceptor::from(Arc::new(server)),
        }))
    }
    /// Serve bounded, nonpersistent TLS/HTTP1 connections until cancelled.
    pub async fn serve(self, app: axum::Router) -> std::io::Result<()> {
        let permits = Arc::new(tokio::sync::Semaphore::new(16));
        loop {
            let (tcp, _) = self.listener.accept().await?;
            let Ok(permit) = permits.clone().try_acquire_owned() else {
                continue;
            };
            let acceptor = self.acceptor.clone();
            let app = app.clone().layer(axum::Extension(super::Tls));
            tokio::spawn(async move {
                let _permit = permit;
                let Ok(Ok(tls)) =
                    tokio::time::timeout(Duration::from_secs(3), acceptor.accept(tcp)).await
                else {
                    return;
                };
                let service = hyper_util::service::TowerToHyperService::new(app);
                let _ = tokio::time::timeout(
                    Duration::from_secs(10),
                    hyper::server::conn::http1::Builder::new()
                        .keep_alive(false)
                        .serve_connection(TokioIo::new(tls), service),
                )
                .await;
            });
        }
    }
}
