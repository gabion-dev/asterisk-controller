// crates/asterisk-controller/src/application.rs

//! The way to the application.
//!
//! Every connection between the node and the application is opened from
//! here, to the one address the launch gives (`--application`):
//! conversation connections to `<address>/conversation`, the service
//! connection to `<address>/service`. Each of them proves the node by its
//! name and secret, as HTTP Basic authentication — the scheme every web
//! stack checks without help, once per connection. The secret is the node's
//! alone; whoever runs the node puts it into the state directory, and the
//! application keeps the same one for the node's name.
//!
//! Across a network the secret travels only inside TLS: an address other
//! than the loopback one must be `wss://` ([`crate::config`]). TLS is rustls
//! with the ring provider, compiled into the controller — the same on every
//! platform, with no TLS library of the machine involved. The application's
//! certificate is judged against the roots the machine trusts.
//!
//! The same address and credentials serve the node's plain requests: the
//! audio of a prompt is fetched from `<address>/prompts/<identifier>`, over
//! HTTPS when the address is `wss://`.
//!
//! Liveness is the protocol's: each side sends a ping at least every
//! [`PING_EVERY`] and counts a connection lost after [`SILENCE_LIMIT`] with
//! nothing from the other side. Opening a connection is held to the same
//! limit: an instance that takes the connection and never answers is as
//! good as none.

use std::{fs, io::ErrorKind, path::Path, sync::Arc, time::Duration};

use data_encoding::BASE64;
use http_body_util::{BodyExt, Empty, Limited};
use hyper::{
    Request, Uri,
    body::Bytes,
    client::conn::http1,
    header::{CONNECTION, HOST},
};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tokio_rustls::TlsConnector;
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
    tungstenite::{
        client::IntoClientRequest,
        http::{HeaderValue, header::AUTHORIZATION},
    },
};

use crate::{config::Config, state_files};

/// A connection to the application.
pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The file of the state directory that holds the node's secret.
pub const SECRET_FILE: &str = "node-secret";

/// How often, at the least, each side of a connection sends a ping.
pub const PING_EVERY: Duration = Duration::from_secs(10);

/// How long a connection may be silent before it is counted lost.
pub const SILENCE_LIMIT: Duration = Duration::from_secs(30);

/// What a connection is opened for: the end of the application it goes to.
#[derive(Clone, Copy, Debug)]
pub enum Endpoint {
    /// One conversation.
    Conversation,
    /// The node's one service connection.
    Service,
}

impl Endpoint {
    const fn path(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Service => "service",
        }
    }
}

/// Where the application is and how the node proves itself to it.
pub struct Application {
    address: String,
    authorization: HeaderValue,
    /// TLS towards the application; `None` on the loopback address.
    tls: Option<Arc<rustls::ClientConfig>>,
}

impl Application {
    /// The way to the application, for the node the controller was started
    /// as.
    ///
    /// # Errors
    ///
    /// The node's secret is missing, empty or readable by others than its
    /// owner, or the machine's trusted roots cannot be read for a `wss://`
    /// address: the node does not reach for the application without being
    /// able to prove itself, or to judge who answers.
    pub fn new(config: &Config) -> Result<Self, String> {
        let secret = secret(&config.state)?;
        let credentials = BASE64.encode(format!("{}:{secret}", config.node).as_bytes());
        let mut authorization = HeaderValue::from_str(&format!("Basic {credentials}"))
            .map_err(|error| format!("the node's credentials cannot be sent: {error}"))?;
        authorization.set_sensitive(true);
        let tls = if config.application_url.starts_with("wss://") {
            Some(Arc::new(tls()?))
        } else {
            None
        };
        Ok(Self {
            address: config.application_url.clone(),
            authorization,
            tls,
        })
    }

    /// Open a connection to the application.
    ///
    /// # Errors
    ///
    /// The connection cannot be opened, the application refuses it, or it
    /// is not open within [`SILENCE_LIMIT`]; the reason, in words.
    pub async fn open(&self, endpoint: Endpoint) -> Result<Socket, String> {
        let url = format!("{}/{}", self.address, endpoint.path());
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|error| format!("{url}: {error}"))?;
        request
            .headers_mut()
            .insert(AUTHORIZATION, self.authorization.clone());
        let connector = self
            .tls
            .as_ref()
            .map_or(Connector::Plain, |tls| Connector::Rustls(Arc::clone(tls)));
        let opening = connect_async_tls_with_config(request, None, false, Some(connector));
        match tokio::time::timeout(SILENCE_LIMIT, opening).await {
            Ok(Ok((socket, _response))) => Ok(socket),
            Ok(Err(error)) => Err(format!("cannot open a connection to {url}: {error}")),
            Err(_) => Err(format!(
                "{url} did not open a connection within {} seconds",
                SILENCE_LIMIT.as_secs()
            )),
        }
    }

    /// Fetch what the application serves at `<address>/<path>`: at most
    /// `largest` bytes, and within [`SILENCE_LIMIT`].
    ///
    /// # Errors
    ///
    /// The application cannot be reached, does not answer 200, sends more
    /// than `largest` bytes, or takes too long; the reason, in words.
    pub async fn fetch(&self, path: &str, largest: usize) -> Result<Vec<u8>, String> {
        let url = format!("{}/{path}", self.address);
        let failed = |problem: &dyn std::fmt::Display| format!("{url}: {problem}");
        let address: Uri = self.address.parse().map_err(|error| failed(&error))?;
        let host = address.host().ok_or_else(|| failed(&"no host"))?.to_owned();
        let port = address
            .port_u16()
            .unwrap_or(if self.tls.is_some() { 443 } else { 80 });
        let target = format!("{}/{path}", address.path().trim_end_matches('/'));
        let fetching = async {
            let host_only = host.trim_start_matches('[').trim_end_matches(']');
            let stream = TcpStream::connect((host_only, port))
                .await
                .map_err(|error| failed(&error))?;
            match &self.tls {
                Some(tls) => {
                    let name = ServerName::try_from(host_only.to_owned())
                        .map_err(|error| failed(&error))?;
                    let stream = TlsConnector::from(Arc::clone(tls))
                        .connect(name, stream)
                        .await
                        .map_err(|error| failed(&error))?;
                    self.get(stream, &host, &target, largest).await
                }
                None => self.get(stream, &host, &target, largest).await,
            }
            .map_err(|problem| failed(&problem))
        };
        match tokio::time::timeout(SILENCE_LIMIT, fetching).await {
            Ok(fetched) => fetched,
            Err(_) => Err(failed(&format!(
                "no answer within {} seconds",
                SILENCE_LIMIT.as_secs()
            ))),
        }
    }

    /// One GET request on an open stream.
    async fn get<S>(
        &self,
        stream: S,
        host: &str,
        target: &str,
        largest: usize,
    ) -> Result<Vec<u8>, String>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|error| error.to_string())?;
        // The connection is driven beside the request and ends with it.
        tokio::spawn(connection);
        let request = Request::builder()
            .method("GET")
            .uri(target)
            .header(HOST, host)
            .header(AUTHORIZATION, self.authorization.clone())
            .header(CONNECTION, "close")
            .body(Empty::<Bytes>::new())
            .map_err(|error| error.to_string())?;
        let response = sender
            .send_request(request)
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        if status != hyper::StatusCode::OK {
            return Err(format!("the application answered {status}"));
        }
        let body = Limited::new(response.into_body(), largest)
            .collect()
            .await
            .map_err(|error| format!("{error} (more than {largest} bytes?)"))?;
        Ok(body.to_bytes().to_vec())
    }
}

/// Read the node's secret from the state directory.
fn secret(state: &Path) -> Result<String, String> {
    let path = state.join(SECRET_FILE);
    let failed = |problem: String| {
        format!(
            "{}: {problem}; whoever runs the node puts the node's secret there, readable by \
             its owner alone, and gives the application the same one for this node",
            path.display()
        )
    };
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(failed("there is no such file".into()));
        }
        Err(error) => return Err(failed(error.to_string())),
    };
    if state_files::open_to_others(&metadata) {
        return Err(failed("others than its owner may read it".into()));
    }
    let secret = fs::read_to_string(&path).map_err(|error| failed(error.to_string()))?;
    let secret = secret.trim();
    if secret.is_empty() {
        return Err(failed("it is empty".into()));
    }
    Ok(secret.to_owned())
}

/// TLS towards the application: rustls with the ring provider, judging the
/// application's certificate against the roots the machine trusts.
fn tls() -> Result<rustls::ClientConfig, String> {
    let found = rustls_native_certs::load_native_certs();
    let mut roots = rustls::RootCertStore::empty();
    let (added, _ignored) = roots.add_parsable_certificates(found.certs);
    if added == 0 {
        return Err(format!(
            "the machine trusts no root certificate this controller can read, so the \
             application's certificate cannot be judged{}",
            if found.errors.is_empty() {
                String::new()
            } else {
                format!(": {:?}", found.errors)
            }
        ));
    }
    rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("TLS cannot be set up: {error}"))
        .map(|builder| builder.with_root_certificates(roots).with_no_client_auth())
}
