// crates/asterisk-controller/tests/application_over_tls.rs

//! The controller reaches the application over TLS — and only an
//! application whose certificate the machine trusts.
//!
//! No Asterisk is needed: the service connection is opened whether Asterisk
//! is there or not, so the controller is started on a tree that is nothing
//! but its build record. The application of the test serves TLS with a
//! certificate made for the test, and the controller is told to trust it
//! the way any program on the machine is told of roots other than the
//! system's: `SSL_CERT_FILE`. Started with another root there, it must
//! refuse the application's certificate and send it nothing.

use std::{
    error::Error,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use data_encoding::BASE64;
use futures_util::StreamExt;
use node_protocol::messages::NodeMessage;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::TcpListener,
    process::{Child, Command},
    sync::Mutex,
    time::timeout,
};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        Message as Frame,
        handshake::server::{ErrorResponse, Request, Response},
        http::{StatusCode, header::AUTHORIZATION},
    },
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// How long any single step may take before the test gives up on it.
const STEP: Duration = Duration::from_secs(15);
const NODE: &str = "tls-node";
const NODE_SECRET: &str = "the secret of the node over TLS";

/// A controller started for this test, with everything it has printed.
struct Controller {
    _child: Child,
    output: Arc<Mutex<Vec<String>>>,
}

impl Controller {
    /// Start a controller that reaches the application at `application`
    /// and trusts the roots in `roots` alone.
    fn start(root: &Path, application: &str, roots: &Path) -> TestResult<Self> {
        let free = || -> TestResult<u16> {
            Ok(std::net::TcpListener::bind("127.0.0.1:0")?
                .local_addr()?
                .port())
        };
        let mut child = Command::new(env!("CARGO_BIN_EXE_asterisk-controller"))
            .args(["--node", NODE, "--listen"])
            .arg(format!("127.0.0.1:{}", free()?))
            .arg("--application")
            .arg(application)
            .arg("--asterisk-tree")
            .arg(root.join("tree"))
            .arg("--state")
            .arg(root.join("node"))
            .arg("--asterisk-http")
            .arg(format!("127.0.0.1:{}", free()?))
            .args(["--audio-ports", "40000-40019"])
            .env("SSL_CERT_FILE", roots)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let output = Arc::new(Mutex::new(Vec::new()));
        for stream in [
            child
                .stdout
                .take()
                .map(|out| Box::new(out) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
            child
                .stderr
                .take()
                .map(|err| Box::new(err) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
        ]
        .into_iter()
        .flatten()
        {
            let kept = Arc::clone(&output);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stream).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    kept.lock().await.push(line);
                }
            });
        }
        Ok(Self {
            _child: child,
            output,
        })
    }

    /// Wait until the controller prints a line containing every one of `words`.
    async fn comes_to_say(&self, words: &[&str]) -> TestResult {
        let said = timeout(STEP, async {
            loop {
                if self
                    .output
                    .lock()
                    .await
                    .iter()
                    .any(|line| words.iter().all(|word| line.contains(word)))
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        if said.is_ok() {
            Ok(())
        } else {
            Err(format!(
                "the controller never said {words:?}; it said:\n{}",
                self.output.lock().await.join("\n")
            )
            .into())
        }
    }
}

/// Write the state of a node with its secret, and a tree that is only its
/// build record.
fn prepare(root: &Path) -> TestResult {
    let _ = fs::remove_dir_all(root);
    let tree = root.join("tree");
    fs::create_dir_all(&tree)?;
    fs::write(
        tree.join("BUILD-INFO.txt"),
        "asterisk-version: 22.11.0\nmodules:\n  res_ari.so\n",
    )?;
    let state = root.join("node");
    fs::create_dir_all(&state)?;
    let secret = state.join("node-secret");
    fs::write(&secret, NODE_SECRET)?;
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// A certificate for `localhost` and its key, the certificate also in PEM
/// at `pem`.
fn certificate(
    pem: &PathBuf,
) -> TestResult<(
    rustls::pki_types::CertificateDer<'static>,
    PrivateKeyDer<'static>,
)> {
    let made = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])?;
    fs::write(pem, made.cert.pem())?;
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(made.signing_key.serialize_der()));
    Ok((made.cert.der().clone(), key))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_application_is_reached_over_tls_and_only_with_a_trusted_certificate() -> TestResult {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("application-over-tls");
    prepare(&root)?;
    let (served, key) = certificate(&root.join("application.pem"))?;
    certificate(&root.join("another.pem"))?;

    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(vec![served], key)?;
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let application = format!("wss://localhost:{}", listener.local_addr()?.port());

    // Trusting the application's certificate, the controller opens the
    // service connection, proves the node and says hello.
    let trusting = Controller::start(&root, &application, &root.join("application.pem"))?;
    let (stream, _) = timeout(STEP, listener.accept())
        .await
        .map_err(|_| "the controller did not reach the application")??;
    let encrypted = acceptor.accept(stream).await?;
    let expected = format!(
        "Basic {}",
        BASE64.encode(format!("{NODE}:{NODE_SECRET}").as_bytes())
    );
    #[expect(
        clippy::result_large_err,
        reason = "the callback's signature is the WebSocket library's"
    )]
    let check = move |request: &Request, response: Response| {
        let proved = request
            .headers()
            .get(AUTHORIZATION)
            .is_some_and(|given| given.as_bytes() == expected.as_bytes());
        if proved && request.uri().path() == "/service" {
            Ok(response)
        } else {
            let mut refusal = ErrorResponse::new(None);
            *refusal.status_mut() = StatusCode::UNAUTHORIZED;
            Err(refusal)
        }
    };
    let mut connection = accept_hdr_async(encrypted, check).await?;
    let first = timeout(STEP, connection.next())
        .await
        .map_err(|_| "the controller said nothing over TLS")?
        .ok_or("the controller closed the connection")??;
    let Frame::Text(text) = first else {
        return Err(format!("the controller's first frame was {first:?}").into());
    };
    let NodeMessage::Hello { node, .. } = node_protocol::decode(text.as_str())? else {
        return Err(format!("the controller's first message was {text}").into());
    };
    assert_eq!(node.as_str(), NODE);
    drop(connection);
    drop(trusting);

    // Trusting another root, it refuses the application's certificate.
    let doubting = Controller::start(&root, &application, &root.join("another.pem"))?;
    let (stream, _) = timeout(STEP, listener.accept())
        .await
        .map_err(|_| "the controller did not try to reach the application")??;
    assert!(
        acceptor.accept(stream).await.is_err(),
        "a controller that does not trust the certificate completed the handshake"
    );
    doubting
        .comes_to_say(&["no service connection", "certificate"])
        .await
}
