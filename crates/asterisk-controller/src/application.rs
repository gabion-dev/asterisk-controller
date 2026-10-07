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
//! Liveness is the protocol's: each side sends a ping at least every
//! [`PING_EVERY`] and counts a connection lost after [`SILENCE_LIMIT`] with
//! nothing from the other side. Opening a connection is held to the same
//! limit: an instance that takes the connection and never answers is as
//! good as none.

use std::{fs, io::ErrorKind, os::unix::fs::PermissionsExt, path::Path, sync::Arc, time::Duration};

use data_encoding::BASE64;
use tokio::net::TcpStream;
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
    tungstenite::{
        client::IntoClientRequest,
        http::{HeaderValue, header::AUTHORIZATION},
    },
};

use crate::config::Config;

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
    connector: Connector,
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
        let connector = if config.application_url.starts_with("wss://") {
            Connector::Rustls(Arc::new(tls()?))
        } else {
            Connector::Plain
        };
        Ok(Self {
            address: config.application_url.clone(),
            authorization,
            connector,
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
        let opening =
            connect_async_tls_with_config(request, None, false, Some(self.connector.clone()));
        match tokio::time::timeout(SILENCE_LIMIT, opening).await {
            Ok(Ok((socket, _response))) => Ok(socket),
            Ok(Err(error)) => Err(format!("cannot open a connection to {url}: {error}")),
            Err(_) => Err(format!(
                "{url} did not open a connection within {} seconds",
                SILENCE_LIMIT.as_secs()
            )),
        }
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
    if metadata.permissions().mode() & 0o077 != 0 {
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
