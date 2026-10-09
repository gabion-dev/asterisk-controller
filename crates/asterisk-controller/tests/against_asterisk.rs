// crates/asterisk-controller/tests/against_asterisk.rs

//! The controller against a real Asterisk.
//!
//! Nothing here is imitated except the application: Asterisk is the build
//! the controller is pinned to, started from its tree; the controller is the
//! built binary; calls are real channels of that Asterisk. What the test
//! plays is the application's side of the conversation connection, and it
//! reads and writes it with the protocol library — a message the library
//! refuses fails the test.
//!
//! Asterisk's configuration is the controller's work too: the test gives the
//! controller a state directory with the node's settings in it, starts it,
//! and starts Asterisk on what the controller wrote. Asterisk must load that
//! without a single warning. Test calls enter the way calls from the
//! telephone network do — through the context and the number the controller
//! made out of the settings.
//!
//! One more Asterisk plays the telephone network: the operator the node's
//! settings name, and a stranger at another address. Its configuration is
//! the test's — it is the world outside the node. Calls it places reach the
//! node the way an operator's do, over SIP.
//!
//! The tree is taken from `ASTERISK_TREE` or from `target/asterisk-server`,
//! where `scripts/fetch-asterisk.sh` puts it. Without a tree the test fails
//! and says how to get one: a check that quietly skipped itself would report
//! a controller nobody ran.

use std::{
    collections::VecDeque,
    error::Error,
    f64::consts::TAU,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use data_encoding::BASE64;
use futures_util::{SinkExt, StreamExt};
use node_protocol::{
    audio::AudioFrame,
    messages::{
        ApplicationMessage, ApplicationServiceMessage, Command, CommandOutcome, CommandRejection,
        ControllerMessage, DeclineReason, Departure, EndReason, Event, FallbackFailure,
        FallbackKind, NodeMessage, Opening, Origin, ParticipantId, ParticipantMedium,
        ParticipantSnapshot, ParticipantState, RejectReason, Report, Request as ApplicationRequest,
        RequestOutcome, RequestRejection, SegmentId, Settings,
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    process::{Child, Command as Process},
    sync::{Mutex, mpsc},
    time::timeout,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, accept_hdr_async, connect_async,
    tungstenite::{
        Message as Frame,
        client::IntoClientRequest,
        handshake::server::{ErrorResponse, Request, Response},
        http::{
            HeaderValue, StatusCode,
            header::{AUTHORIZATION, SEC_WEBSOCKET_PROTOCOL},
        },
    },
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// How long any single step may take before the test gives up on it. A
/// limit of the test, not of the controller: without it a failure would
/// show as a run that never ends.
const STEP: Duration = Duration::from_secs(15);

/// The name the node of the test is started with.
const NODE: &str = "test-node";
/// The node's secret: what it proves itself with to the application.
const NODE_SECRET: &str = "the secret of the test node";
/// How long a connection to the application may be silent before the
/// controller counts it lost — the protocol's rule.
const SILENCE_LIMIT: Duration = Duration::from_secs(30);

/// The tone of the test's prompt, Hz.
const PROMPT_SAYS: u32 = 600;

/// The number of the node's one entry.
const DIALED: &str = "+19715870050";
/// The same, as it is written inside an address.
const DIALED_IN_URL: &str = "%2B19715870050";
/// The password the node presents to its operator: everything Asterisk's
/// configuration format treats specially inside a value, beginning with the
/// `>` that `=>` would swallow.
const OPERATOR_PASSWORD: &str = ">pass;word #1 = [x], \\>";
/// The number of the node's other entry, whose fallback is a transfer.
const DIALED_WITH_TRANSFER: &str = "+19715870051";
/// The number of an entry the application adds while the node runs.
const DIALED_LATER: &str = "+19715870052";
/// The number of an entry whose fallback transfers to a busy number.
const DIALED_TO_BUSY: &str = "+19715870053";
/// The number the operator's caller calls from.
const CALLER: &str = "+15035550100";
const CALLER_IN_URL: &str = "%2B15035550100";
/// Numbers of the telephone network of the test, by what happens to a call:
/// it is answered, rings unanswered, finds the line busy, or fails with a
/// reason of the network's own.
const ANSWERS: &str = "+15035550101";
const RINGS: &str = "+15035550102";
const BUSY: &str = "+15035550103";
const FAILS: &str = "+15035550104";
/// The network's reason for the call that fails: no circuit available.
const FAILS_WITH: i64 = 34;
/// A Stasis application of the test's own. The far end of a call that only
/// rings waits in it, so that the call stays up until the test ends it.
const FAR_END: &str = "the-far-end";
/// The node's own Stasis application, which the controller subscribes to.
const NODE_APPLICATION: &str = "gabion";
/// An address of this machine that is not the operator's. The stranger of
/// the test calls from it.
const STRANGER_ADDRESS: &str = "127.0.0.2";
/// The address of the operator the node reaches over TLS: an operator is
/// known by the address its calls come from, so it has one of its own.
const SECURE_ADDRESS: &str = "127.0.0.3";

fn asterisk_tree() -> TestResult<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let tree = std::env::var_os("ASTERISK_TREE")
        .map_or_else(|| root.join("target/asterisk-server"), PathBuf::from);
    if tree.join("sbin/asterisk").is_file() {
        Ok(tree.canonicalize()?)
    } else {
        Err(format!(
            "no Asterisk tree at {}: run scripts/fetch-asterisk.sh, or point ASTERISK_TREE at one",
            tree.display()
        )
        .into())
    }
}

/// A port nothing listens on right now.
async fn free_port() -> TestResult<u16> {
    Ok(TcpListener::bind("127.0.0.1:0").await?.local_addr()?.port())
}

/// Where the operators of the test's telephone network are: the plain one
/// and the one reached over TLS.
#[derive(Clone, Copy)]
struct OperatorPorts {
    plain: u16,
    secure: u16,
}

/// The settings the node of the test starts on: three entries, and the
/// operators whose trunks are the test's telephone network — one over UDP,
/// one over TLS — with lines through them.
fn settings(operators: OperatorPorts) -> serde_json::Value {
    serde_json::json!({
        "operators": [{
            "id": "the test's operator",
            "host": "127.0.0.1",
            "port": operators.plain,
            "transport": "udp",
            "credentials": { "username": "node", "password": OPERATOR_PASSWORD },
            "source_networks": ["127.0.0.1/32"],
            "countries": ["US"]
        }, {
            "id": "the test's secure operator",
            "host": SECURE_ADDRESS,
            "port": operators.secure,
            "transport": "tls",
            "credentials": { "username": "node", "password": OPERATOR_PASSWORD },
            "source_networks": [format!("{SECURE_ADDRESS}/32")],
            "countries": ["US"]
        }],
        "lines": [{
            "id": "main",
            "number": DIALED,
            "operator": "the test's operator",
            // Canada is allowed by the line and carried by no operator.
            "allowed_countries": ["US", "CA"],
            "concurrent_outbound_limit": 1
        }, {
            // A line that carries two calls at once: a conversation of three.
            "id": "wide",
            "number": DIALED,
            "operator": "the test's operator",
            "allowed_countries": ["US"],
            "concurrent_outbound_limit": 2
        }, {
            "id": "secure",
            "number": DIALED,
            "operator": "the test's secure operator",
            "allowed_countries": ["US"],
            "concurrent_outbound_limit": 1
        }],
        // Entries for the two things a fallback can be, and a transfer that
        // cannot connect the caller.
        "entries": [
            {
                "key": { "type": "dialed_number", "number": DIALED },
                "fallback": { "type": "message", "prompt": "closed" }
            },
            {
                "key": { "type": "dialed_number", "number": DIALED_WITH_TRANSFER },
                "fallback": {
                    "type": "transfer", "number": ANSWERS, "line": "main",
                    "answer_limit_ms": 5000
                }
            },
            {
                "key": { "type": "dialed_number", "number": DIALED_TO_BUSY },
                "fallback": {
                    "type": "transfer", "number": BUSY, "line": "main",
                    "answer_limit_ms": 5000
                }
            }
        ],
        "prompts": [{ "id": "closed", "sha256": node_protocol::sha256_hex(&prompt_audio()) }]
    })
}

/// What whoever runs the node puts into its state directory: the settings
/// it starts on and its secret — each readable by its owner alone, or the
/// node does not start on it (the settings carry the operators' passwords).
fn what_the_node_is_given(state: &Path, operators: OperatorPorts) -> TestResult {
    let stored_settings = state.join("settings.json");
    fs::write(&stored_settings, settings(operators).to_string())?;
    fs::set_permissions(
        &stored_settings,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )?;
    let secret = state.join("node-secret");
    fs::write(&secret, NODE_SECRET)?;
    fs::set_permissions(&secret, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    Ok(())
}

/// Ports of the telephone network of the test.
struct NetworkPorts {
    http: u16,
    /// Where the operator's trunk is: what the node's settings name.
    operator: u16,
    /// Where the stranger sends from.
    stranger: u16,
    /// Where the operator's trunk over TLS is.
    secure: u16,
    audio: u16,
    /// Where the node listens for operators.
    node: u16,
    /// Where the node listens for operators over TLS.
    node_secure: u16,
}

/// The certificates of the test: the node's, which the network judges, and
/// the network's, which the node judges — each made for the address it is
/// reached at.
struct Certificates {
    node: PathBuf,
    node_key: PathBuf,
    /// The node's certificate itself, as a TLS connection presents it.
    node_der: Vec<u8>,
    network: PathBuf,
    network_key: PathBuf,
}

impl Certificates {
    fn make(directory: &Path) -> TestResult<Self> {
        fs::create_dir_all(directory)?;
        let made = |address: &str, name: &str| -> TestResult<(PathBuf, PathBuf, Vec<u8>)> {
            let made = rcgen::generate_simple_self_signed(vec![address.to_owned()])?;
            let (certificate, key) = (
                directory.join(format!("{name}.pem")),
                directory.join(format!("{name}-key.pem")),
            );
            fs::write(&certificate, made.cert.pem())?;
            fs::write(&key, made.signing_key.serialize_pem())?;
            fs::set_permissions(&key, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
            Ok((certificate, key, made.cert.der().to_vec()))
        };
        let (node, node_key, node_der) = made("127.0.0.1", "node")?;
        let (network, network_key, _) = made(SECURE_ADDRESS, "network")?;
        Ok(Self {
            node,
            node_key,
            node_der,
            network,
            network_key,
        })
    }
}

/// Write the configuration of the telephone network and return its main
/// file. To it the node is a customer reached at the node's address; the
/// operator calls from the address the node's settings accept, the stranger
/// from another one.
fn write_network_configuration(
    tree: &Path,
    state: &Path,
    ports: &NetworkPorts,
    certificates: &Certificates,
) -> TestResult<PathBuf> {
    let etc = state.join("etc");
    for directory in ["etc", "db", "keys/keys", "spool", "run", "log"] {
        fs::create_dir_all(state.join(directory))?;
    }
    let lib = tree.join("var/lib/asterisk");
    let modules: Vec<String> = fs::read_to_string(tree.join("BUILD-INFO.txt"))?
        .lines()
        .skip_while(|line| line.trim() != "modules:")
        .skip(1)
        .take_while(|line| line.starts_with(' '))
        .map(|line| format!("require = {}", line.trim()))
        .collect();
    let files = [
        (
            "asterisk.conf",
            format!(
                "[directories]\nastetcdir => {}\nastmoddir => {}\nastvarlibdir => {lib}\nastdatadir => {lib}\n\
                 astagidir => {lib}/agi-bin\nastsbindir => {}\nastdbdir => {}\nastkeydir => {}\n\
                 astspooldir => {}\nastrundir => {}\nastlogdir => {}\n",
                etc.display(),
                tree.join("lib/asterisk/modules").display(),
                tree.join("sbin").display(),
                state.join("db").display(),
                state.join("keys").display(),
                state.join("spool").display(),
                state.join("run").display(),
                state.join("log").display(),
                lib = lib.display(),
            ),
        ),
        ("modules.conf", format!("[modules]\nautoload = no\n{}\n", modules.join("\n"))),
        ("logger.conf", "[general]\n[logfiles]\nconsole => notice,warning,error\n".into()),
        (
            "http.conf",
            format!("[general]\nenabled = yes\nbindaddr = 127.0.0.1\nbindport = {}\n", ports.http),
        ),
        (
            "rtp.conf",
            format!("[general]\nrtpstart = {}\nrtpend = {}\n", ports.audio, ports.audio.saturating_add(19)),
        ),
        ("manager.conf", "[general]\nenabled = no\n".into()),
        (
            "indications.conf",
            "[general]\ncountry = us\n\n[us]\ndescription = United States\nringcadence = 2000,4000\n\
             dial = 350+440\nbusy = 480+620/500,0/500\nring = 440+480/2000,0/4000\n\
             congestion = 480+620/250,0/250\n"
                .into(),
        ),
        (
            "ari.conf",
            "[general]\nenabled = yes\n\n[network]\ntype = user\nread_only = no\npassword = network\n"
                .into(),
        ),
        ("extensions.conf", network_dialplan()),
        ("pjsip.conf", network_sides(ports, certificates)),
    ];
    for (name, content) in files {
        fs::write(etc.join(name), content)?;
    }
    for name in [
        "acl.conf",
        "ccss.conf",
        "cdr.conf",
        "cel.conf",
        "chan_websocket.conf",
        "features.conf",
        "pjproject.conf",
        "stasis.conf",
        "udptl.conf",
        "websocket_client.conf",
    ] {
        fs::write(etc.join(name), "")?;
    }
    Ok(etc.join("asterisk.conf"))
}

/// What the network does with a call the node places.
fn network_dialplan() -> String {
    format!(
        "[from-the-node]\nexten = {ANSWERS},1,Answer()\n same = n,Stasis({FAR_END})\n \
         same = n,Hangup()\nexten = {RINGS},1,Ringing()\n same = n,Wait(600)\n\
         exten = {BUSY},1,Busy()\nexten = {FAILS},1,Hangup({FAILS_WITH})\n"
    )
}

/// The network's two sides towards the node: the operator, at the address
/// the node's settings accept, and the stranger at another one. To both the
/// node is a customer reached at the node's address.
///
/// A real network sends audio for as long as a call lasts, packets of
/// silence included; a channel of this test network that nobody speaks into
/// sends nothing. So the operator's calls keep their audio alive with a
/// packet a second (`rtp_keepalive`) — except through `the-node-quiet`, the
/// same operator's side for a call whose audio is to stop, and which puts
/// the call on hold as a telephone does: by telling the node
/// (`moh_passthrough`), not by playing music of its own.
///
/// The secure operator reaches the node over TLS, at its own address, with
/// encrypted audio, and judges the node's certificate. Calls the node places
/// through it come from the node's address and are taken by `the-node`,
/// which takes encrypted audio when it is offered and plain audio otherwise.
fn network_sides(ports: &NetworkPorts, certificates: &Certificates) -> String {
    let side = |name: &str, transport: &str, more: &str| {
        format!(
            "[{name}]\ntype = endpoint\ntransport = {transport}\ncontext = from-the-node\n\
             disallow = all\nallow = ulaw\naors = {name}\ndirect_media = no\n{more}\n\
             [{name}]\ntype = aor\ncontact = sip:127.0.0.1:{}\n\n",
            ports.node,
        )
    };
    let secure_side = format!(
        "[the-node-secure]\ntype = endpoint\ntransport = secure\ncontext = from-the-node\n\
         disallow = all\nallow = ulaw\naors = the-node-secure\ndirect_media = no\n\
         media_encryption = sdes\nmedia_encryption_optimistic = no\nrtp_keepalive = 1\n\n\
         [the-node-secure]\ntype = aor\ncontact = sip:127.0.0.1:{}\\;transport=tls\n\n",
        ports.node_secure,
    );
    format!(
        "[global]\ntype = global\nendpoint_identifier_order = ip\n\n\
         [operator]\ntype = transport\nprotocol = udp\nbind = 127.0.0.1:{}\n\n\
         [stranger]\ntype = transport\nprotocol = udp\nbind = {STRANGER_ADDRESS}:{}\n\n\
         [secure]\ntype = transport\nprotocol = tls\nbind = {SECURE_ADDRESS}:{}\n\
         method = tlsv1_2\ncert_file = {}\npriv_key_file = {}\nca_list_file = {}\n\
         verify_server = yes\n\n\
         {}{}{}{secure_side}\
         [the-node]\ntype = identify\nendpoint = the-node\nmatch = 127.0.0.1/32\n\n\
         [the-node-credentials]\ntype = auth\nauth_type = userpass\nusername = node\n\
         password = {}\n",
        ports.operator,
        ports.stranger,
        ports.secure,
        certificates.network.display(),
        certificates.network_key.display(),
        certificates.node.display(),
        // The operator asks the node who it is, and knows the same password
        // the node's settings carry.
        side(
            "the-node",
            "operator",
            "auth = the-node-credentials\nrtp_keepalive = 1\n\
             media_encryption = sdes\nmedia_encryption_optimistic = yes\n"
        ),
        side("the-node-from-elsewhere", "stranger", ""),
        side("the-node-quiet", "operator", "moh_passthrough = yes\n"),
        OPERATOR_PASSWORD.replace(';', "\\;"),
    )
}

/// Read what an Asterisk says of itself over its control interface.
#[expect(
    clippy::disallowed_methods,
    reason = "Asterisk's own description of its channels, not a protocol message"
)]
fn read_asterisk(text: &str) -> TestResult<serde_json::Value> {
    Ok(serde_json::from_str(text)?)
}

/// Start the controller and wait until it is ready for Asterisk. It trusts
/// the roots in `roots` alone — told so the way any program on the machine
/// is told of roots other than the system's, `SSL_CERT_FILE`.
async fn start_controller(arguments: &[OsString], roots: &Path) -> TestResult<Running> {
    let mut controller = Process::new(env!("CARGO_BIN_EXE_asterisk-controller"));
    controller.args(arguments).env("SSL_CERT_FILE", roots);
    start(controller, "asterisk-controller ready", "the controller").await
}

/// Start an Asterisk of the tree on a configuration and wait until it has booted.
async fn start_asterisk(tree: &Path, main_file: &Path, what: &'static str) -> TestResult<Running> {
    let mut asterisk = Process::new(tree.join("sbin/asterisk"));
    asterisk.arg("-C").arg(main_file).args(["-f", "-n"]);
    if cfg!(target_os = "linux") {
        asterisk.env("LD_LIBRARY_PATH", tree.join("lib"));
    }
    start(asterisk, "Asterisk Ready.", what).await
}

/// A started process, with everything it has printed so far.
struct Running {
    what: &'static str,
    child: Child,
    output: Arc<Mutex<Vec<String>>>,
}

impl Running {
    /// Whether the process has printed a line containing `words`.
    async fn said(&self, words: &str) -> bool {
        self.output
            .lock()
            .await
            .iter()
            .any(|line| line.contains(words))
    }

    /// Wait until the process has printed a line containing `words`.
    ///
    /// What a process prints reaches the test a moment after it is printed,
    /// so words about what has only just happened are waited for.
    async fn comes_to_say(&self, words: &str) -> bool {
        self.comes_to_say_within(words, STEP).await
    }

    /// The same, for words that may take longer than a step to come.
    async fn comes_to_say_within(&self, words: &str, limit: Duration) -> bool {
        timeout(limit, async {
            while !self.said(words).await {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .is_ok()
    }

    /// Say whether the process is still alive and show what it printed.
    ///
    /// Called when the test has failed: a failure that follows the death of
    /// Asterisk is a different finding from one Asterisk survived, and its
    /// own words are the first evidence of either.
    async fn report(&mut self) {
        // A process that died a moment ago may not have been reaped yet. The
        // wait is a limit of this report only: past it the process is called
        // running, which is what it then is.
        let fate = match timeout(Duration::from_millis(500), self.child.wait()).await {
            Ok(Ok(status)) => format!("is DEAD — {status}"),
            Ok(Err(error)) => format!("could not be asked for its state: {error}"),
            Err(_) => "is still running".to_owned(),
        };
        eprintln!("\n===== {} {fate}; its output:", self.what);
        for line in self.output.lock().await.iter() {
            eprintln!("{line}");
        }
        eprintln!("===== end of the output of {}", self.what);
    }
}

/// Start a process and wait until it prints `ready` on a line of its own
/// output; its death before that is the failure.
///
/// Both of its streams are kept, line by line, for the report of a failed
/// test — and read to their end, so the process never blocks on a full pipe.
async fn start(mut process: Process, ready: &str, what: &'static str) -> TestResult<Running> {
    let mut child = process
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let (lines, mut printed) = mpsc::unbounded_channel::<String>();
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let stderr = child.stderr.take().ok_or("no stderr")?;
    tokio::spawn(forward_lines(stdout, lines.clone()));
    tokio::spawn(forward_lines(stderr, lines));

    let output = Arc::new(Mutex::new(Vec::new()));
    let waited = timeout(STEP, async {
        while let Some(line) = printed.recv().await {
            let is_ready = line.contains(ready);
            output.lock().await.push(line);
            if is_ready {
                return true;
            }
        }
        false
    })
    .await;
    let kept = Arc::clone(&output);
    tokio::spawn(async move {
        while let Some(line) = printed.recv().await {
            kept.lock().await.push(line);
        }
    });

    let mut running = Running {
        what,
        child,
        output,
    };
    match waited {
        Ok(true) => Ok(running),
        Ok(false) => {
            running.report().await;
            Err(format!("{what} exited before it was ready").into())
        }
        Err(_) => {
            running.report().await;
            Err(format!("{what} did not become ready").into())
        }
    }
}

/// Pass a stream on, line by line, until it ends.
async fn forward_lines(stream: impl AsyncRead + Unpin, lines: mpsc::UnboundedSender<String>) {
    let mut stream = BufReader::new(stream).lines();
    while let Ok(Some(line)) = stream.next_line().await {
        if lines.send(line).is_err() {
            return;
        }
    }
}

/// Asterisk's control interface, as the test uses it: to place its calls
/// and to look at what Asterisk has.
struct ControlInterface {
    port: u16,
    /// The `Authorization` header of the control user.
    authorization: String,
    /// Channels of the test's own application that have ended, each with
    /// the telephone network's reason for its end.
    ended: Arc<Mutex<Vec<(String, i64)>>>,
    /// Tone digits channels of the test's own application have received.
    digits: Arc<Mutex<Vec<(String, String)>>>,
}

impl ControlInterface {
    /// The control interface of the Asterisk whose state directory this is,
    /// as its user `user`. The password is read from the configuration
    /// written for Asterisk: the node's is the controller's control secret,
    /// kept in the state directory and written into that file.
    fn of(port: u16, state: &Path, user: &str) -> TestResult<Self> {
        let written = fs::read_to_string(state.join("etc/ari.conf"))?;
        let password = written
            .lines()
            .find_map(|line| line.strip_prefix("password = "))
            .ok_or("no password of a control user in the configuration")?;
        let credentials = format!("{user}:{password}");
        Ok(Self {
            port,
            authorization: format!("Basic {}", BASE64.encode(credentials.as_bytes())),
            ended: Arc::default(),
            digits: Arc::default(),
        })
    }

    /// One request; returns status and body.
    ///
    /// The answer is read by its stated length, not until the connection
    /// ends: Asterisk may end the connection with a reset once it has
    /// answered, and a reset after a complete answer is not a failure of
    /// the request.
    async fn request(&self, method: &str, path: &str) -> TestResult<(u16, String)> {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).await?;
        let request = format!(
            "{method} /ari/{path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: {}\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n",
            self.authorization,
        );
        stream.write_all(request.as_bytes()).await?;

        let mut reader = BufReader::new(stream);
        let mut status_line = String::new();
        timeout(STEP, reader.read_line(&mut status_line)).await??;
        let status = status_line
            .split_whitespace()
            .nth(1)
            .ok_or("no status line")?
            .parse()?;

        let mut length = 0_usize;
        loop {
            let mut header = String::new();
            timeout(STEP, reader.read_line(&mut header)).await??;
            if header.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse()?;
            }
        }
        let mut body = vec![0_u8; length];
        timeout(STEP, reader.read_exact(&mut body)).await??;
        Ok((status, String::from_utf8(body)?))
    }

    /// Have Asterisk create a channel; `how` says what the channel is and
    /// where it goes. Returns the channel's identifier.
    async fn create_channel(&self, how: &str) -> TestResult<String> {
        let (status, body) = self.request("POST", &format!("channels?{how}")).await?;
        if status != 200 {
            return Err(format!("Asterisk refused the test call: {status} {body}").into());
        }
        body.split("\"id\":\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .map(str::to_owned)
            .ok_or_else(|| format!("no channel identifier in {body}").into())
    }

    /// This Asterisk ends up with nothing of a kind — `channels` or
    /// `bridges`.
    ///
    /// The controller reports a participant gone when the channel leaves
    /// the application; Asterisk takes a moment more to destroy it. So this
    /// waits for the state itself, and fails if it never comes.
    async fn expect_none(&self, kind: &str, after: &str) -> TestResult {
        let mut still = String::new();
        let emptied = timeout(STEP, async {
            loop {
                still = self.request("GET", kind).await?.1;
                if still == "[]" {
                    return Ok::<(), Box<dyn Error>>(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        match emptied {
            Ok(result) => result,
            Err(_) => Err(format!("{after}: Asterisk still has {kind} {still}").into()),
        }
    }

    /// Open the connection on which Asterisk serves the test's own Stasis
    /// application, and keep it open: the application exists for as long as
    /// the connection does.
    async fn serve_far_ends(&self) -> TestResult {
        let mut events = self.events_of(FAR_END).await?;
        let ended = Arc::clone(&self.ended);
        let digits = Arc::clone(&self.digits);
        tokio::spawn(async move {
            while let Some(Ok(frame)) = events.next().await {
                let Frame::Text(text) = frame else { continue };
                let Ok(event) = read_asterisk(text.as_str()) else {
                    continue;
                };
                let kind = event.get("type").and_then(serde_json::Value::as_str);
                let channel = event.pointer("/channel/id").and_then(|id| id.as_str());
                if kind == Some("ChannelDestroyed")
                    && let Some(channel) = channel
                    && let Some(cause) = event.get("cause").and_then(serde_json::Value::as_i64)
                {
                    ended.lock().await.push((channel.to_owned(), cause));
                }
                if kind == Some("ChannelDtmfReceived")
                    && let Some(channel) = channel
                    && let Some(digit) = event.get("digit").and_then(serde_json::Value::as_str)
                {
                    digits
                        .lock()
                        .await
                        .push((channel.to_owned(), digit.to_owned()));
                }
            }
        });
        Ok(())
    }

    /// Open the event connection of a Stasis application, as this control
    /// interface's user — what subscribes to the application.
    async fn events_of(
        &self,
        application: &str,
    ) -> TestResult<WebSocketStream<MaybeTlsStream<TcpStream>>> {
        let credentials = self
            .authorization
            .strip_prefix("Basic ")
            .map(|encoded| BASE64.decode(encoded.as_bytes()))
            .ok_or("no credentials")??;
        let url = format!(
            "ws://127.0.0.1:{}/ari/events?app={application}&api_key={}",
            self.port,
            String::from_utf8(credentials)?,
        );
        let (events, _response) = connect_async(url).await?;
        Ok(events)
    }

    /// The tone digits a channel of the test's own application has
    /// received, once there are `count` of them — waited for, since they
    /// travel in real time.
    async fn digits_heard(&self, channel: &str, count: usize) -> TestResult<String> {
        let heard = timeout(STEP, async {
            loop {
                let heard: String = self
                    .digits
                    .lock()
                    .await
                    .iter()
                    .filter(|(on, _)| on == channel)
                    .map(|(_, digit)| digit.as_str())
                    .collect();
                if heard.len() >= count {
                    return heard;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        heard.map_err(|_| format!("channel {channel} did not hear {count} digits").into())
    }

    /// The telephone network's reason for the end of a channel that was in
    /// the test's own application — waited for, since the end takes a moment.
    async fn reason_it_ended(&self, channel: &str) -> TestResult<i64> {
        let found = timeout(STEP, async {
            loop {
                let known = self
                    .ended
                    .lock()
                    .await
                    .iter()
                    .find(|(ended, _)| ended == channel)
                    .map(|(_, cause)| *cause);
                if let Some(cause) = known {
                    return cause;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        found.map_err(|_| format!("channel {channel} did not end").into())
    }
}

/// Samples of a sine of amplitude 8000, starting at sample `from`.
fn tone(frequency: u32, rate: u32, from: u32, samples: u32) -> Vec<i16> {
    (from..from + samples)
        .map(|n| {
            let phase = f64::from(n) * f64::from(frequency) / f64::from(rate) * TAU;
            #[expect(
                clippy::cast_possible_truncation,
                reason = "a sine scaled to 8000 fits sixteen bits"
            )]
            let sample = (phase.sin() * 8000.0) as i16;
            sample
        })
        .collect()
}

/// How strongly one frequency is present in audio: about the amplitude of a
/// sine of that frequency filling all of it, about zero when it is absent.
fn level(samples: &[i16], rate: u32, frequency: u32) -> f64 {
    let coefficient = 2.0 * (TAU * f64::from(frequency) / f64::from(rate)).cos();
    let (mut previous, mut before_it, mut count) = (0.0, 0.0, 0.0);
    for &sample in samples {
        let current = f64::from(sample) + coefficient * previous - before_it;
        before_it = previous;
        previous = current;
        count += 1.0;
    }
    let power = previous * previous + before_it * before_it - coefficient * previous * before_it;
    if count == 0.0 {
        0.0
    } else {
        2.0 * power.max(0.0).sqrt() / count
    }
}

/// Audio as the application sends and receives it: sixteen bits, little-endian.
fn to_bytes(samples: &[i16]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect()
}

fn from_bytes(audio: &[u8]) -> Vec<i16> {
    audio
        .as_chunks::<2>()
        .0
        .iter()
        .copied()
        .map(i16::from_le_bytes)
        .collect()
}

/// One sample as the telephone network carries it (G.711 μ-law).
fn ulaw_encode(sample: i16) -> u8 {
    let magnitude = i32::from(sample).abs().min(32_635) + 0x84;
    let exponent = (0..8)
        .rev()
        .find(|exponent| magnitude & (0x80 << exponent) != 0)
        .unwrap_or(0);
    let mantissa = (magnitude >> (exponent + 3)) & 0x0F;
    let sign = if sample < 0 { 0x80 } else { 0 };
    u8::try_from(!(sign | (exponent << 4) | mantissa) & 0xFF).unwrap_or(0xFF)
}

fn ulaw_decode(byte: u8) -> i16 {
    let byte = i32::from(!byte);
    let magnitude = ((((byte & 0x0F) << 3) + 0x84) << ((byte & 0x70) >> 4)) - 0x84;
    i16::try_from(if byte & 0x80 == 0 {
        magnitude
    } else {
        -magnitude
    })
    .unwrap_or(0)
}

/// The telephone of a caller who speaks and listens: a media channel of
/// Asterisk that the test connects to, in the telephone network's own audio
/// format. From the moment it is connected it says a steady tone, and it
/// keeps everything it hears.
struct Phone {
    heard: mpsc::UnboundedReceiver<Vec<u8>>,
    /// Whether it sends its frames; a silent telephone sends none at all.
    speaking: Arc<AtomicBool>,
}

impl Phone {
    /// The frequency the caller says, Hz.
    const SAYS: u32 = 400;
    /// The telephone network's sampling rate, Hz.
    const RATE: u32 = 8000;
    /// Samples of twenty milliseconds at that rate.
    const FRAME: u32 = 160;

    /// Connect a telephone to the media channel Asterisk made for it. From
    /// then on it says a tone of `says` Hz.
    async fn connect(control: &ControlInterface, channel: &str, says: u32) -> TestResult<Self> {
        let (_, connection) = control
            .request(
                "GET",
                &format!("channels/{channel}/variable?variable=MEDIA_WEBSOCKET_CONNECTION_ID"),
            )
            .await?;
        let connection = connection
            .split("\"value\":\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .ok_or_else(|| format!("Asterisk named no media connection: {connection}"))?;
        let mut request =
            format!("ws://127.0.0.1:{}/media/{connection}", control.port).into_client_request()?;
        request
            .headers_mut()
            .insert(SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("media"));
        let (mut socket, _response) = connect_async(request).await?;
        let (earpiece, heard) = mpsc::unbounded_channel();
        let speaking = Arc::new(AtomicBool::new(true));
        let sends = Arc::clone(&speaking);
        tokio::spawn(async move {
            // A telephone speaks in real time: one frame every twenty milliseconds.
            let mut pace = tokio::time::interval(Duration::from_millis(20));
            let mut said = 0;
            loop {
                tokio::select! {
                    _ = pace.tick() => {
                        if !sends.load(Ordering::Relaxed) {
                            continue;
                        }
                        let frame: Vec<u8> = tone(says, Self::RATE, said, Self::FRAME)
                            .into_iter()
                            .map(ulaw_encode)
                            .collect();
                        said += Self::FRAME;
                        if socket.send(Frame::binary(frame)).await.is_err() {
                            return;
                        }
                    }
                    frame = socket.next() => match frame {
                        Some(Ok(Frame::Binary(audio))) => {
                            if earpiece.send(audio.to_vec()).is_err() {
                                return;
                            }
                        }
                        Some(Ok(Frame::Close(_)) | Err(_)) | None => return,
                        Some(Ok(_)) => {}
                    },
                }
            }
        });
        Ok(Self { heard, speaking })
    }

    /// Stop sending, or start again: a telephone whose line went dead, or
    /// came back.
    fn speaks(&self, speaking: bool) {
        self.speaking.store(speaking, Ordering::Relaxed);
    }

    /// What the caller hears next: at least `samples` of it.
    async fn hear(&mut self, samples: usize) -> TestResult<Vec<i16>> {
        let mut heard = Vec::new();
        while heard.len() < samples {
            let audio = timeout(STEP, self.heard.recv())
                .await
                .map_err(|_| format!("the caller heard {} samples, not {samples}", heard.len()))?
                .ok_or("the caller's telephone was disconnected")?;
            heard.extend(audio.into_iter().map(ulaw_decode));
        }
        Ok(heard)
    }

    /// Forget what the telephone has heard so far: what it hears next is
    /// what arrives from now on.
    fn forget(&mut self) {
        while self.heard.try_recv().is_ok() {}
    }

    /// Listen until a tone of `frequency` Hz fills what the telephone hears
    /// — or, with `present` false, until there is none of it.
    ///
    /// A telephone nobody speaks to is sent nothing at all, so for a tone
    /// that should be gone a pause in what arrives is an answer too.
    async fn hears(&mut self, frequency: u32, present: bool) -> TestResult {
        /// Audio arrives every twenty milliseconds while there is any.
        const PAUSE: Duration = Duration::from_millis(300);
        self.forget();
        let mut now = None;
        let waited = timeout(STEP, async {
            loop {
                // A fifth of a second at a time.
                let mut heard = Vec::new();
                while heard.len() < 1600 {
                    match timeout(PAUSE, self.heard.recv()).await {
                        Ok(Some(audio)) => heard.extend(audio.into_iter().map(ulaw_decode)),
                        Ok(None) => return Err("a telephone was disconnected".into()),
                        Err(_) if present => now = None,
                        Err(_) => return Ok::<(), Box<dyn Error>>(()),
                    }
                }
                let level = level(&heard, Self::RATE, frequency);
                now = Some(level);
                if (present && level > 7000.0) || (!present && level < 500.0) {
                    return Ok(());
                }
            }
        })
        .await;
        match waited {
            Ok(result) => result,
            Err(_) => Err(format!(
                "a telephone that should hear {} {frequency} Hz hears {}",
                if present { "all of" } else { "none of" },
                now.map_or_else(
                    || "nothing at all".to_owned(),
                    |level| format!("it at {level:.0}")
                ),
            )
            .into()),
        }
    }
}

/// A connection the controller opened to the application of the test.
type Connection = WebSocketStream<TcpStream>;

/// The two queues connections wait in, by what they are for.
#[derive(Clone)]
struct Queues {
    conversations: mpsc::UnboundedSender<Connection>,
    services: mpsc::UnboundedSender<Connection>,
}

/// The application of the test: the place the controller opens its
/// connections to.
///
/// A connection must carry the node's name and secret, as the application
/// checks them; one that does not is refused with 401. What a connection is
/// for is told by its path, and each kind waits in its own queue for the
/// scenario that takes it.
struct Application {
    port: u16,
    conversations: Mutex<mpsc::UnboundedReceiver<Connection>>,
    services: Mutex<mpsc::UnboundedReceiver<Connection>>,
    queues: Queues,
    accepting: tokio::task::JoinHandle<()>,
}

impl Application {
    async fn open() -> TestResult<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let (conversations, conversations_waiting) = mpsc::unbounded_channel();
        let (services, services_waiting) = mpsc::unbounded_channel();
        let queues = Queues {
            conversations,
            services,
        };
        let accepting = tokio::spawn(accept_connections(listener, queues.clone()));
        Ok(Self {
            port,
            conversations: Mutex::new(conversations_waiting),
            services: Mutex::new(services_waiting),
            queues,
            accepting,
        })
    }

    /// The application goes away: nothing listens at its address any more.
    async fn go_away(&mut self) {
        self.accepting.abort();
        let _ = (&mut self.accepting).await;
    }

    /// The application is there again, at the same address.
    async fn come_back(&mut self) -> TestResult {
        let listener = TcpListener::bind(("127.0.0.1", self.port)).await?;
        self.accepting = tokio::spawn(accept_connections(listener, self.queues.clone()));
        Ok(())
    }

    /// The next conversation connection the controller opens.
    async fn conversation(&self) -> TestResult<Connection> {
        timeout(STEP, self.conversations.lock().await.recv())
            .await
            .map_err(|_| "the controller did not open a conversation connection")?
            .ok_or_else(|| "the application stopped taking connections".into())
    }

    /// The next service connection the controller opens.
    async fn service(&self) -> TestResult<Connection> {
        timeout(STEP, self.services.lock().await.recv())
            .await
            .map_err(|_| "the controller did not open a service connection")?
            .ok_or_else(|| "the application stopped taking connections".into())
    }
}

/// Take the connections the controller opens, check that each proves the
/// node, and queue it by its path.
async fn accept_connections(listener: TcpListener, queues: Queues) {
    let expected = format!(
        "Basic {}",
        BASE64.encode(format!("{NODE}:{NODE_SECRET}").as_bytes())
    );
    while let Ok((stream, _)) = listener.accept().await {
        let queues = queues.clone();
        let expected = expected.clone();
        tokio::spawn(async move {
            // A request that is not an upgrade to WebSocket is the node
            // fetching a prompt.
            let Some(head) = request_head(&stream).await else {
                return;
            };
            if !head.to_ascii_lowercase().contains("upgrade: websocket") {
                serve_plain(stream, &head, &expected).await;
                return;
            }
            let path = Arc::new(std::sync::Mutex::new(String::new()));
            let seen = Arc::clone(&path);
            #[expect(
                clippy::result_large_err,
                reason = "the callback's signature is the WebSocket library's"
            )]
            let check = move |request: &Request, response: Response| {
                let refuse = |status: StatusCode| {
                    let mut refusal = ErrorResponse::new(None);
                    *refusal.status_mut() = status;
                    Err(refusal)
                };
                let proved = request
                    .headers()
                    .get(AUTHORIZATION)
                    .is_some_and(|given| given.as_bytes() == expected.as_bytes());
                if !proved {
                    return refuse(StatusCode::UNAUTHORIZED);
                }
                let path = request.uri().path().to_owned();
                if path != "/conversation" && path != "/service" {
                    return refuse(StatusCode::NOT_FOUND);
                }
                if let Ok(mut seen) = seen.lock() {
                    *seen = path;
                }
                Ok(response)
            };
            let Ok(connection) = accept_hdr_async(stream, check).await else {
                return;
            };
            let path = path.lock().map(|path| path.clone()).unwrap_or_default();
            let queue = if path == "/service" {
                &queues.services
            } else {
                &queues.conversations
            };
            let _ = queue.send(connection);
        });
    }
}

/// The head of the request on a connection, looked at without taking it off.
async fn request_head(stream: &TcpStream) -> Option<String> {
    let mut seen = vec![0_u8; 8192];
    loop {
        let length = timeout(STEP, stream.peek(&mut seen)).await.ok()?.ok()?;
        let text = String::from_utf8_lossy(seen.get(..length)?).into_owned();
        if let Some(end) = text.find("\r\n\r\n") {
            return text.get(..end + 4).map(str::to_owned);
        }
        if length == seen.len() {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Answer a plain request: the audio of the test's prompt to the node that
/// proves itself, nothing to anyone else.
async fn serve_plain(mut stream: TcpStream, head: &str, expected: &str) {
    let mut taken = vec![0_u8; head.len()];
    if stream.read_exact(&mut taken).await.is_err() {
        return;
    }
    let proved = head.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("authorization") && value.trim() == expected
        })
    });
    let path = head.split_whitespace().nth(1).unwrap_or_default();
    let (status, body) = match (proved, path) {
        (false, _) => ("401 Unauthorized", Vec::new()),
        (true, "/prompts/closed") => ("200 OK", prompt_audio()),
        (true, _) => ("404 Not Found", Vec::new()),
    };
    let answer = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(answer.as_bytes()).await;
    let _ = stream.write_all(&body).await;
}

/// The audio of the test's one prompt: a second of a steady tone, in the
/// protocol's format.
fn prompt_audio() -> Vec<u8> {
    to_bytes(&tone(PROMPT_SAYS, 16_000, 0, 16_000))
}

/// What arrives on a conversation connection, as the test reads it.
enum Incoming {
    Message(ControllerMessage),
    Audio(AudioFrame),
    /// The controller sent what the protocol library refuses.
    Broken(String),
}

/// The application's end of one conversation connection.
///
/// The connection is served beside the test: pings are answered as they
/// come, as an instance of the application answers them, whatever the
/// scenario is doing meanwhile. Everything else waits for the scenario.
struct Owner {
    incoming: mpsc::UnboundedReceiver<Incoming>,
    outgoing: mpsc::UnboundedSender<Frame>,
    /// Messages that arrived while the test was waiting for audio.
    unread: VecDeque<ControllerMessage>,
    /// Audio frames that arrived, in order.
    heard: Vec<AudioFrame>,
}

impl Owner {
    /// Wait for the controller to open a conversation connection.
    async fn accept(application: &Application) -> TestResult<Self> {
        let connection = application.conversation().await?;
        let (to_test, incoming) = mpsc::unbounded_channel();
        let (outgoing, to_controller) = mpsc::unbounded_channel();
        tokio::spawn(serve_conversation(connection, to_test, to_controller));
        Ok(Self {
            incoming,
            outgoing,
            unread: VecDeque::new(),
            heard: Vec::new(),
        })
    }

    /// Take one item off the connection: a message goes to `unread`, an
    /// audio frame — decoded by the protocol library — to `heard`.
    async fn read(&mut self) -> TestResult {
        let item = timeout(STEP, self.incoming.recv())
            .await
            .map_err(|_| "the controller said nothing")?
            .ok_or("the controller closed the conversation connection")?;
        match item {
            Incoming::Message(message) => self.unread.push_back(message),
            Incoming::Audio(frame) => self.heard.push(frame),
            Incoming::Broken(problem) => return Err(problem.into()),
        }
        Ok(())
    }

    async fn next(&mut self) -> TestResult<ControllerMessage> {
        loop {
            if let Some(message) = self.unread.pop_front() {
                return Ok(message);
            }
            self.read().await?;
        }
    }

    /// Wait until `frames` audio frames have arrived, and take them all.
    async fn hear(&mut self, frames: usize) -> TestResult<Vec<AudioFrame>> {
        while self.heard.len() < frames {
            self.read().await?;
        }
        Ok(std::mem::take(&mut self.heard))
    }

    fn put(&self, frame: Frame) -> TestResult {
        self.outgoing
            .send(frame)
            .map_err(|_| "the conversation connection is closed".into())
    }

    /// Send audio of a queued segment, as one playback frame.
    fn audio(
        &self,
        participant: &ParticipantId,
        segment: &SegmentId,
        samples: &[i16],
        last: bool,
    ) -> TestResult {
        let frame = AudioFrame::Playback {
            participant: participant.clone(),
            segment: segment.clone(),
            last,
            audio: to_bytes(samples),
        };
        self.put(Frame::binary(frame.encode()?))
    }

    /// The next event; an immediate answer to a command may come first.
    async fn next_event(&mut self) -> TestResult<Event> {
        loop {
            if let ControllerMessage::Event { event } = self.next().await? {
                return Ok(event);
            }
        }
    }

    /// The outcome of command `id`; events that arrive first are returned with it.
    async fn outcome_of(&mut self, id: u64) -> TestResult<(CommandOutcome, Vec<Event>)> {
        let mut events = Vec::new();
        loop {
            match self.next().await? {
                ControllerMessage::CommandResult { id: got, outcome } if got == id => {
                    return Ok((outcome, events));
                }
                ControllerMessage::Event { event } => events.push(event),
                other @ (ControllerMessage::Hello { .. }
                | ControllerMessage::CommandResult { .. }
                | ControllerMessage::Ping { .. }
                | ControllerMessage::Pong { .. }) => {
                    return Err(format!("unexpected {other:?}").into());
                }
            }
        }
    }

    fn send(&self, message: &ApplicationMessage) -> TestResult {
        self.put(Frame::text(node_protocol::encode(message)?))
    }

    /// Send a text as it is, protocol or not.
    fn send_text(&self, text: &'static str) -> TestResult {
        self.put(Frame::text(text))
    }

    fn command(&self, id: u64, command: Command) -> TestResult {
        self.send(&ApplicationMessage::Command { id, command })
    }

    /// The connection is closed by the controller, with nothing more said.
    async fn closed(&mut self) -> TestResult {
        loop {
            match timeout(STEP, self.incoming.recv())
                .await
                .map_err(|_| "the connection stayed open")?
            {
                None => return Ok(()),
                Some(Incoming::Message(message)) => {
                    return Err(format!("unexpected message {message:?}").into());
                }
                Some(Incoming::Broken(problem)) => return Err(problem.into()),
                Some(Incoming::Audio(_)) => {}
            }
        }
    }
}

/// Serve one conversation connection beside the test: answer pings, pass
/// everything else on, and send what the test puts out. When the test lets
/// go of its end, the connection is dropped with nothing said — as an
/// instance that dies drops it.
async fn serve_conversation(
    mut connection: Connection,
    to_test: mpsc::UnboundedSender<Incoming>,
    mut to_controller: mpsc::UnboundedReceiver<Frame>,
) {
    loop {
        tokio::select! {
            frame = connection.next() => {
                let item = match frame {
                    Some(Ok(Frame::Text(text))) => {
                        match node_protocol::decode::<ControllerMessage>(text.as_str()) {
                            Ok(ControllerMessage::Ping { n }) => {
                                let Ok(pong) = node_protocol::encode(&ApplicationMessage::Pong { n })
                                else {
                                    return;
                                };
                                if connection.send(Frame::text(pong)).await.is_err() {
                                    return;
                                }
                                continue;
                            }
                            Ok(message) => Incoming::Message(message),
                            Err(error) => Incoming::Broken(format!("the controller sent {text}: {error}")),
                        }
                    }
                    Some(Ok(Frame::Binary(bytes))) => match AudioFrame::decode(&bytes) {
                        Ok(frame) => Incoming::Audio(frame),
                        Err(error) => Incoming::Broken(format!("the controller sent a frame: {error}")),
                    },
                    Some(Ok(Frame::Close(_)) | Err(_)) | None => return,
                    Some(Ok(_)) => continue,
                };
                if to_test.send(item).is_err() {
                    return;
                }
            }
            frame = to_controller.recv() => match frame {
                Some(frame) => {
                    if connection.send(frame).await.is_err() {
                        return;
                    }
                }
                None => return,
            },
        }
    }
}

/// The application's end of the service connection, served beside the test
/// the same way: pings are answered as they come.
struct ServiceEnd {
    incoming: mpsc::UnboundedReceiver<Result<NodeMessage, String>>,
    outgoing: mpsc::UnboundedSender<Frame>,
}

impl ServiceEnd {
    /// Wait for the controller to open its service connection.
    async fn accept(application: &Application) -> TestResult<Self> {
        let mut connection = application.service().await?;
        let (to_test, incoming) = mpsc::unbounded_channel();
        let (outgoing, mut to_controller) = mpsc::unbounded_channel::<Frame>();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    frame = connection.next() => {
                        let message = match frame {
                            Some(Ok(Frame::Text(text))) => {
                                node_protocol::decode::<NodeMessage>(text.as_str())
                                    .map_err(|error| format!("the controller sent {text}: {error}"))
                            }
                            Some(Ok(Frame::Close(_)) | Err(_)) | None => return,
                            Some(Ok(_)) => continue,
                        };
                        if let Ok(NodeMessage::Ping { n }) = message {
                            let Ok(pong) = node_protocol::encode(&ApplicationServiceMessage::Pong { n })
                            else {
                                return;
                            };
                            if connection.send(Frame::text(pong)).await.is_err() {
                                return;
                            }
                            continue;
                        }
                        if to_test.send(message).is_err() {
                            return;
                        }
                    }
                    frame = to_controller.recv() => match frame {
                        Some(frame) => {
                            if connection.send(frame).await.is_err() {
                                return;
                            }
                        }
                        None => return,
                    },
                }
            }
        });
        Ok(Self { incoming, outgoing })
    }

    async fn next(&mut self) -> TestResult<NodeMessage> {
        Ok(timeout(STEP, self.incoming.recv())
            .await
            .map_err(|_| "the controller said nothing on the service connection")?
            .ok_or("the controller closed the service connection")??)
    }

    fn send(&self, message: &ApplicationServiceMessage) -> TestResult {
        self.outgoing
            .send(Frame::text(node_protocol::encode(message)?))
            .map_err(|_| "the service connection is closed".into())
    }

    /// Send a text as it is — one the protocol description may not allow.
    fn send_text(&self, text: String) -> TestResult {
        self.outgoing
            .send(Frame::text(text))
            .map_err(|_| "the service connection is closed".into())
    }

    /// Give the node settings and return what it answered.
    async fn give(&mut self, settings: Settings) -> TestResult<NodeMessage> {
        self.send(&ApplicationServiceMessage::Settings { settings })?;
        self.next().await
    }

    /// The next message must be a report that `expected` holds for — what
    /// the node did on its own. It is acknowledged when `acknowledge` says
    /// so; its identifier is returned.
    async fn reported(
        &mut self,
        what: &str,
        acknowledge: bool,
        expected: impl Fn(&Report) -> bool,
    ) -> TestResult<String> {
        let NodeMessage::Report { id, report } = self.next().await? else {
            return Err(format!("{what}: the node said something other than a report").into());
        };
        if !expected(&report) {
            return Err(format!("{what}: the node reported {report:?}").into());
        }
        if acknowledge {
            self.send(&ApplicationServiceMessage::ReportReceived { id: id.clone() })?;
        }
        Ok(id.as_str().to_owned())
    }
}

fn is_hang_up(report: &Report) -> bool {
    matches!(report, Report::ParticipantHungUp { .. })
}

fn is_message(report: &Report) -> bool {
    matches!(
        report,
        Report::FallbackApplied {
            fallback: node_protocol::messages::FallbackKind::Message,
            ..
        }
    )
}

/// The telephone network of the test: a second Asterisk.
struct Network {
    control: ControlInterface,
    asterisk: Running,
}

impl Network {
    /// Give the caller of a call that the node has answered a telephone:
    /// a media channel of the network, joined to the caller's end there.
    /// From then on the caller says the telephone's tone and the telephone
    /// keeps what the caller is sent — over the trunk, as real calls go.
    async fn give_a_phone(&self, caller: &str) -> TestResult<(String, Phone)> {
        self.give_a_phone_that_says(caller, Phone::SAYS, "the-callers-phone")
            .await
    }

    /// The same with a telephone that says a tone of `says` Hz, joined to
    /// the caller's end in the bridge `bridge` of the network.
    async fn give_a_phone_that_says(
        &self,
        caller: &str,
        says: u32,
        bridge: &str,
    ) -> TestResult<(String, Phone)> {
        let channel = self
            .control
            .create_channel(&format!(
                "endpoint=WebSocket/INCOMING/c(ulaw)&app={FAR_END}"
            ))
            .await?;
        let phone = Phone::connect(&self.control, &channel, says).await?;
        // A bridge of that name still there is a telephone a scenario before
        // left behind, still saying its tone: joined to it, this caller would
        // send the node audio that is not theirs.
        let (status, body) = self
            .control
            .request("POST", &format!("bridges?type=mixing&bridgeId={bridge}"))
            .await?;
        if status != 200 {
            return Err(format!(
                "the network could not make the bridge {bridge} for a telephone — one left \
                 behind by a scenario before? {status} {body}"
            )
            .into());
        }
        // Both ends enter the test's application a moment after they are
        // answered; Asterisk joins them only once they are there.
        let joined = timeout(STEP, async {
            loop {
                let (status, _) = self
                    .control
                    .request(
                        "POST",
                        &format!("bridges/{bridge}/addChannel?channel={caller},{channel}"),
                    )
                    .await?;
                if status == 204 {
                    return Ok::<(), Box<dyn Error>>(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        match joined {
            Ok(result) => result.map(|()| (channel, phone)),
            Err(_) => Err("the network could not join the caller and their telephone".into()),
        }
    }
}

/// A running Asterisk with its controller, the application the controller
/// opens its connections to, and the telephone network around them.
struct Stand {
    control: ControlInterface,
    application: Application,
    /// The application's end of the service connection, once a scenario has
    /// welcomed the node.
    service: Mutex<Option<ServiceEnd>>,
    /// A report left unacknowledged on purpose: a controller started again
    /// must send it again.
    unacknowledged: Mutex<Option<String>>,
    /// Where the operators' trunks are.
    operators: OperatorPorts,
    /// The state directory of the node.
    state: PathBuf,
    /// What the controller is started with.
    controller_arguments: Vec<OsString>,
    /// The certificates of the node and of the network.
    certificates: Certificates,
    controller: Running,
    asterisk: Running,
    network: Network,
}

impl Stand {
    async fn start() -> TestResult<Self> {
        let tree = asterisk_tree()?;
        // The node's state directory holds the socket of Asterisk's console,
        // whose path the controller holds to what a Unix socket address
        // takes: a directory inside the clone's `target` is too long on a
        // build machine. So the stand lives in the system's temporary
        // directory, under a name of this clone's own — stands of two clones
        // never meet; two runs in one clone still do.
        let clone = node_protocol::sha256_hex(env!("CARGO_TARGET_TMPDIR").as_bytes());
        let root = std::env::temp_dir().join(format!(
            "against-asterisk-{}",
            clone.get(..12).ok_or("a digest is shorter than 12")?
        ));
        let _ = fs::remove_dir_all(&root);
        let state = root.join("node");
        fs::create_dir_all(&state)?;

        let http_port = free_port().await?;
        let controller_port = free_port().await?;
        let sip_port = free_port().await?;
        // Audio ports go in pairs that begin on an even one.
        let audio_port = free_port().await? & !1;
        let application = Application::open().await?;
        let application_port = application.port;
        let network_ports = NetworkPorts {
            http: free_port().await?,
            operator: free_port().await?,
            stranger: free_port().await?,
            secure: free_port().await?,
            audio: free_port().await? & !1,
            node: sip_port,
            node_secure: free_port().await?,
        };
        let operators = OperatorPorts {
            plain: network_ports.operator,
            secure: network_ports.secure,
        };
        let certificates = Certificates::make(&root.join("certificates"))?;
        what_the_node_is_given(&state, operators)?;

        let controller_arguments: Vec<OsString> = [
            "--node".into(),
            NODE.into(),
            "--listen".into(),
            format!("127.0.0.1:{controller_port}").into(),
            "--application".into(),
            format!("ws://127.0.0.1:{application_port}").into(),
            "--asterisk-tree".into(),
            tree.clone().into_os_string(),
            "--state".into(),
            state.clone().into_os_string(),
            "--asterisk-http".into(),
            format!("127.0.0.1:{http_port}").into(),
            "--audio-ports".into(),
            format!("{audio_port}-{}", audio_port.saturating_add(19)).into(),
            "--sip".into(),
            format!("127.0.0.1:{sip_port}").into(),
            "--sip-tls".into(),
            format!("127.0.0.1:{}", network_ports.node_secure).into(),
            "--sip-tls-certificate".into(),
            certificates.node.clone().into_os_string(),
            "--sip-tls-key".into(),
            certificates.node_key.clone().into_os_string(),
        ]
        .into();
        let controller = start_controller(&controller_arguments, &certificates.network).await?;

        // Asterisk is started on what the controller wrote, and on nothing else.
        let mut asterisk =
            start_asterisk(&tree, &state.join("etc/asterisk.conf"), "Asterisk").await?;
        if asterisk.said("WARNING[").await || asterisk.said("ERROR[").await {
            asterisk.report().await;
            return Err("Asterisk did not load the controller's configuration cleanly".into());
        }
        let control = ControlInterface::of(http_port, &state, "gabion-controller")?;
        control.serve_far_ends().await?;

        let network_state = root.join("network");
        let network_file =
            write_network_configuration(&tree, &network_state, &network_ports, &certificates)?;
        let mut network = start_asterisk(&tree, &network_file, "the telephone network").await?;
        if network.said("ERROR[").await {
            network.report().await;
            return Err(format!(
                "the telephone network of the test did not start cleanly; it needs \
                 {STRANGER_ADDRESS} and {SECURE_ADDRESS} to be addresses of this machine"
            )
            .into());
        }
        let network_control = ControlInterface::of(network_ports.http, &network_state, "network")?;
        network_control.serve_far_ends().await?;

        Ok(Self {
            control,
            application,
            service: Mutex::new(None),
            unacknowledged: Mutex::new(None),
            operators,
            state,
            controller_arguments,
            certificates,
            controller,
            asterisk,
            network: Network {
                control: network_control,
                asterisk: network,
            },
        })
    }

    /// The next report on the service connection, as [`ServiceEnd::reported`].
    async fn reported(
        &self,
        what: &str,
        acknowledge: bool,
        expected: impl Fn(&Report) -> bool,
    ) -> TestResult<String> {
        let mut service = self.service.lock().await;
        let service = service
            .as_mut()
            .ok_or("no service connection has been welcomed")?;
        service.reported(what, acknowledge, expected).await
    }

    /// Ask the node something on the service connection, and wait for the
    /// answer.
    async fn ask(&self, id: u64, request: ApplicationRequest) -> TestResult<RequestOutcome> {
        let mut service = self.service.lock().await;
        let service = service
            .as_mut()
            .ok_or("no service connection has been welcomed")?;
        service.send(&ApplicationServiceMessage::Request { id, request })?;
        match service.next().await? {
            NodeMessage::RequestResult {
                id: answered,
                outcome,
            } if answered == id => Ok(outcome),
            other @ (NodeMessage::Hello { .. }
            | NodeMessage::SettingsApplied { .. }
            | NodeMessage::SettingsRefused { .. }
            | NodeMessage::RequestResult { .. }
            | NodeMessage::Report { .. }
            | NodeMessage::Ping { .. }
            | NodeMessage::Pong { .. }) => {
                Err(format!("request {id} was answered with {other:?}").into())
            }
        }
    }

    /// The value the controller was started with for `name`.
    fn controller_argument(&self, name: &str) -> TestResult<String> {
        let mut arguments = self.controller_arguments.iter();
        arguments
            .find(|argument| argument.as_os_str() == name)
            .and_then(|_| arguments.next())
            .and_then(|value| value.to_str())
            .map(str::to_owned)
            .ok_or_else(|| format!("the controller was started without {name}").into())
    }

    /// Where the controller listens for Asterisk's media connections.
    fn controller_address(&self) -> TestResult<String> {
        let mut arguments = self.controller_arguments.iter();
        arguments
            .find(|argument| argument.as_os_str() == "--listen")
            .and_then(|_| arguments.next())
            .and_then(|address| address.to_str())
            .map(str::to_owned)
            .ok_or_else(|| "the controller was started without --listen".into())
    }

    /// Kill the node's Asterisk as a crash would. Its calls end with it.
    async fn kill_asterisk(&mut self) -> TestResult {
        self.asterisk.child.kill().await?;
        Ok(())
    }

    /// Start the node's Asterisk again, on what the controller wrote.
    async fn start_asterisk_again(&mut self) -> TestResult {
        self.asterisk = start_asterisk(
            &asterisk_tree()?,
            &self.state.join("etc/asterisk.conf"),
            "Asterisk",
        )
        .await?;
        Ok(())
    }

    /// Kill the controller as a crash would. Asterisk goes on running.
    async fn kill_the_controller(&mut self) -> TestResult {
        self.controller.child.kill().await?;
        Ok(())
    }

    /// Start a controller in place of the one that was killed.
    async fn start_the_controller(&mut self) -> TestResult {
        self.controller =
            start_controller(&self.controller_arguments, &self.certificates.network).await?;
        Ok(())
    }

    /// What both processes have to say about a failed test.
    async fn report(&mut self) {
        self.asterisk.report().await;
        self.controller.report().await;
        self.network.asterisk.report().await;
    }

    /// The controller and Asterisk stayed connected all along: neither says
    /// it lost the other, and nothing Asterisk was connected to went away
    /// without closing its connection.
    async fn expect_no_lost_control_connection(&self) -> TestResult {
        if self.controller.said("LOST").await {
            return Err("the controller lost its connection to Asterisk".into());
        }
        if self.asterisk.said("closed abruptly").await {
            return Err("Asterisk lost a connection that was not closed properly".into());
        }
        Ok(())
    }

    /// Place a call from the telephone network — a channel that arrives
    /// where an operator's calls do, for the number of the entry, ringing —
    /// and take the conversation the controller opens for it. Returns the
    /// identifier of the caller's end.
    async fn call(&self) -> TestResult<(String, Owner)> {
        let caller = self
            .control
            .create_channel(&format!(
                "endpoint=Local/{DIALED_IN_URL}@gabion-from-network&app={FAR_END}"
            ))
            .await?;
        Ok((caller, Owner::accept(&self.application).await?))
    }

    /// The same from a caller who speaks and listens. Their telephone is
    /// itself the channel that arrives, and it arrives answered: Asterisk
    /// runs the dialplan for it only once the test has connected to it.
    async fn call_from_a_phone(&self) -> TestResult<(String, Phone, Owner)> {
        let (caller, phone) = self.a_phone_calls(DIALED).await?;
        Ok((caller, phone, Owner::accept(&self.application).await?))
    }

    /// A caller who speaks and listens calls `number`. Whether a
    /// conversation connection follows is for the scenario to say.
    async fn a_phone_calls(&self, number: &str) -> TestResult<(String, Phone)> {
        let caller = self
            .control
            .create_channel(&format!(
                "endpoint=WebSocket/INCOMING/c(ulaw)&extension={}\
                 &context=gabion-from-network&priority=1",
                number.replace('+', "%2B")
            ))
            .await?;
        let phone = Phone::connect(&self.control, &caller, Phone::SAYS).await?;
        Ok((caller, phone))
    }

    /// Asterisk read the operator of the settings as it was given: what the
    /// controller wrote into the configuration came out of Asterisk's own
    /// reading of it unchanged, special characters and all.
    async fn expect_the_operator_as_it_was_given(&self) -> TestResult {
        let written = fs::read_to_string(self.state.join("etc/pjsip.conf"))?;
        let name = written
            .lines()
            .find_map(|line| {
                line.strip_prefix("[gabion-operator-")
                    .and_then(|rest| rest.strip_suffix(']'))
            })
            .ok_or("the controller wrote no operator")?;
        let (status, read) = self
            .control
            .request(
                "GET",
                &format!("asterisk/config/dynamic/res_pjsip/auth/gabion-operator-{name}"),
            )
            .await?;
        let expected = serde_json::json!(OPERATOR_PASSWORD).to_string();
        if status == 200 && read.contains(&format!("\"value\":{expected}")) {
            Ok(())
        } else {
            Err(format!("Asterisk read the operator's credentials as {status} {read}").into())
        }
    }

    /// Asterisk ends up with no channels at all.
    ///
    /// The controller reports a participant gone when the channel leaves
    /// the application; Asterisk takes a moment more to destroy it. So this
    /// waits for the state itself, and fails if it never comes.
    async fn expect_no_channels(&self, after: &str) -> TestResult {
        self.expect_none("channels", after).await
    }

    /// Asterisk ends up with no bridges either: what the controller built
    /// for a participant's audio is gone with the participant.
    async fn expect_no_bridges(&self, after: &str) -> TestResult {
        self.expect_none("bridges", after).await
    }

    async fn expect_none(&self, kind: &str, after: &str) -> TestResult {
        self.control.expect_none(kind, after).await
    }

    /// Asterisk ends up with one bridge of a group, with `size` channels in
    /// it — or, for a size of zero, with no bridge of a group at all.
    ///
    /// The controller answers a command at once and Asterisk does what was
    /// asked a moment later, so this waits for the state itself.
    async fn expect_a_group_of(&self, size: usize, after: &str) -> TestResult {
        if size == 0 {
            self.expect_groups(&[], after).await
        } else {
            self.expect_groups(&[size], after).await
        }
    }

    /// Wait until the groups of the node are of exactly these sizes, in any
    /// order.
    async fn expect_groups(&self, sizes: &[usize], after: &str) -> TestResult {
        let mut wanted = sizes.to_vec();
        wanted.sort_unstable();
        let mut groups = Vec::new();
        let arranged = timeout(STEP, async {
            loop {
                let bridges = read_asterisk(&self.control.request("GET", "bridges").await?.1)?;
                groups = bridges
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|bridge| {
                        bridge
                            .get("id")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|id| id.contains(".group-"))
                    })
                    .map(|bridge| {
                        bridge
                            .get("channels")
                            .and_then(serde_json::Value::as_array)
                            .map_or(0, Vec::len)
                    })
                    .collect();
                groups.sort_unstable();
                if groups == wanted {
                    return Ok::<(), Box<dyn Error>>(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        match arranged {
            Ok(result) => result,
            Err(_) => Err(format!(
                "{after}: Asterisk has groups of {groups:?} channels, not {wanted:?}"
            )
            .into()),
        }
    }
}

/// Nobody is heard, and nothing is played to anybody, before they answer.
async fn refuses_to_listen_before_the_answer(
    owner: &mut Owner,
    participant: &ParticipantId,
) -> TestResult {
    owner.command(
        7,
        Command::Listen {
            participant: participant.clone(),
        },
    )?;
    let (outcome, _) = owner.outcome_of(7).await?;
    assert!(
        matches!(
            outcome,
            CommandOutcome::Rejected {
                reason: CommandRejection::ParticipantNotInRequiredState
            }
        ),
        "listening to a ringing participant was {outcome:?}"
    );
    Ok(())
}

/// A call rings, is answered, and the caller hangs up.
async fn answered_then_the_caller_hangs_up(stand: &Stand) -> TestResult {
    let (caller, mut owner) = stand.call().await?;
    let ControllerMessage::Hello {
        opening,
        node,
        protocol,
        ..
    } = owner.next().await?
    else {
        return Err("the first message was not a hello".into());
    };
    assert_eq!(node.as_str(), NODE);
    assert_eq!(protocol.get(), 1);
    let Opening::Started { origin, first } = opening else {
        return Err("the conversation did not open as a new one".into());
    };
    let Origin::DialedNumber { dialed } = origin else {
        return Err("the call did not arrive as a dialed number".into());
    };
    assert_eq!(dialed.as_str(), DIALED);
    assert_eq!(first.medium, ParticipantMedium::TelephoneNetwork);
    assert!(
        first.number.is_none(),
        "a caller without a number was given one"
    );
    let participant = first.id;

    owner.send(&ApplicationMessage::Accept)?;
    refuses_to_listen_before_the_answer(&mut owner, &participant).await?;

    owner.command(
        1,
        Command::Answer {
            participant: participant.clone(),
        },
    )?;
    let (outcome, mut events) = owner.outcome_of(1).await?;
    assert!(
        matches!(outcome, CommandOutcome::Accepted),
        "answer was {outcome:?}"
    );
    if events.is_empty() {
        events.push(owner.next_event().await?);
    }
    assert!(
        matches!(events.as_slice(), [Event::ParticipantAnswered { participant: who }] if *who == participant),
        "after the answer came {events:?}"
    );

    // A command about a participant that does not exist is rejected, with the reason.
    owner.command(
        2,
        Command::Answer {
            participant: "p-9".parse()?,
        },
    )?;
    let (outcome, _) = owner.outcome_of(2).await?;
    assert!(
        matches!(
            outcome,
            CommandOutcome::Rejected {
                reason: CommandRejection::UnknownParticipant
            }
        ),
        "a command for an unknown participant was {outcome:?}"
    );

    stand
        .control
        .request("DELETE", &format!("channels/{caller}"))
        .await?;
    let left = owner.next_event().await?;
    assert!(
        matches!(&left, Event::ParticipantLeft { participant: who, departure: Departure::HungUp } if *who == participant),
        "after the caller hung up came {left:?}"
    );
    let ended = owner.next_event().await?;
    assert!(
        matches!(
            ended,
            Event::ConversationEnded {
                reason: EndReason::LastParticipantLeft
            }
        ),
        "the conversation ended as {ended:?}"
    );
    owner.closed().await
}

/// The handler ends the conversation itself.
async fn ended_by_the_handler(stand: &Stand) -> TestResult {
    let (_caller, mut owner) = stand.call().await?;
    let ControllerMessage::Hello {
        opening: Opening::Started { first, .. },
        ..
    } = owner.next().await?
    else {
        return Err("the call did not open with a hello".into());
    };
    owner.send(&ApplicationMessage::Accept)?;
    owner.command(1, Command::End)?;
    let (outcome, mut events) = owner.outcome_of(1).await?;
    assert!(
        matches!(outcome, CommandOutcome::Accepted),
        "`end` was {outcome:?}"
    );
    while events.len() < 2 {
        events.push(owner.next_event().await?);
    }
    assert!(
        matches!(
            events.as_slice(),
            [
                Event::ParticipantLeft { participant: who, departure: Departure::Removed },
                Event::ConversationEnded { reason: EndReason::EndedByHandler },
            ] if *who == first.id
        ),
        "after `end` came {events:?}"
    );
    owner.closed().await?;
    stand.expect_no_channels("after `end`").await
}

/// The application declines the conversation: nobody is left on the line.
async fn declined(stand: &Stand) -> TestResult {
    let (_caller, mut phone, mut owner) = stand.call_from_a_phone().await?;
    owner.next().await?;
    owner.send(&ApplicationMessage::Decline {
        reason: DeclineReason::NoHandler,
    })?;
    owner.closed().await?;
    // The entry's fallback is a message: the caller hears the prompt, then
    // the call ends — and the node says it did that by itself.
    stand.reported("a declined call", true, is_message).await?;
    phone.hears(PROMPT_SAYS, true).await?;
    stand.expect_no_channels("after a declined call").await
}

/// The application breaks the protocol: nobody is left on the line either.
async fn the_owner_breaks_the_protocol(stand: &Stand) -> TestResult {
    let (_caller, mut owner) = stand.call().await?;
    owner.next().await?;
    // "accept" has no fields; a field on it is not the protocol.
    owner.send_text(r#"{"type":"accept","note":"x"}"#)?;
    owner.closed().await?;
    stand
        .expect_no_channels("after the owner broke the protocol")
        .await?;

    // Neither is a command identifier used twice on one connection: the
    // answers would be told apart by it.
    let (_caller, mut owner) = stand.call().await?;
    let first = hello(&mut owner).await?.first.id;
    owner.send(&ApplicationMessage::Accept)?;
    let once = Command::StopListening { participant: first };
    owner.command(1, once.clone())?;
    let (outcome, _) = owner.outcome_of(1).await?;
    assert!(
        matches!(outcome, CommandOutcome::Accepted),
        "`stop_listening` was {outcome:?}"
    );
    owner.command(1, once)?;
    owner.closed().await?;
    stand
        .expect_no_channels("after the owner used a command identifier twice")
        .await
}

/// The application hears what the caller says, as frames of twenty
/// milliseconds in the one format of the protocol.
async fn hears_the_caller(
    owner: &mut Owner,
    participant: &ParticipantId,
    first_id: u64,
) -> TestResult {
    owner.command(
        first_id,
        Command::Listen {
            participant: participant.clone(),
        },
    )?;
    let (outcome, _) = owner.outcome_of(first_id).await?;
    assert!(
        matches!(outcome, CommandOutcome::Accepted),
        "`listen` was {outcome:?}"
    );

    let frames = owner.hear(50).await?;
    let mut samples = Vec::new();
    let mut expected_position = None;
    for frame in &frames {
        let AudioFrame::Heard {
            participant: who,
            position_ms,
            audio,
        } = frame
        else {
            return Err(format!("the controller sent {frame:?}").into());
        };
        assert_eq!(who, participant);
        if let Some(expected) = expected_position {
            assert_eq!(*position_ms, expected, "a heard frame is out of its place");
        }
        expected_position = Some(position_ms + 20);
        samples.extend(from_bytes(audio));
    }
    // The caller says a steady tone, in the telephone network's format; the
    // application gets it at its own sampling rate.
    let recent = samples
        .get(samples.len().saturating_sub(8000)..)
        .unwrap_or(&[]);
    let (said, other) = (
        level(recent, 16_000, Phone::SAYS),
        level(recent, 16_000, 1000),
    );
    assert!(
        said > 7000.0 && other < 500.0,
        "the application heard the caller's tone at {said:.0} and another at {other:.0}"
    );

    owner.command(
        first_id + 1,
        Command::StopListening {
            participant: participant.clone(),
        },
    )?;
    let (outcome, _) = owner.outcome_of(first_id + 1).await?;
    assert!(
        matches!(outcome, CommandOutcome::Accepted),
        "`stop_listening` was {outcome:?}"
    );
    // Frames sent before the controller read the command are already here;
    // from its answer on there are none.
    owner.heard.clear();
    Ok(())
}

/// A segment the application queues is played to the caller, and its start
/// and its delivery are reported in that order.
async fn plays_to_the_caller(
    owner: &mut Owner,
    phone: &mut Phone,
    participant: &ParticipantId,
    id: u64,
) -> TestResult {
    let greeting: SegmentId = "greeting".parse()?;
    owner.command(
        id,
        Command::Play {
            participant: participant.clone(),
            segment: greeting.clone(),
        },
    )?;
    // Half a second, cut where no frame of twenty milliseconds ends.
    let audio = tone(1000, 16_000, 0, 8000);
    let (first, second) = audio.split_at(5000);
    owner.audio(participant, &greeting, first, false)?;
    owner.audio(participant, &greeting, second, true)?;
    let (outcome, mut events) = owner.outcome_of(id).await?;
    assert!(
        matches!(outcome, CommandOutcome::Accepted),
        "`play` was {outcome:?}"
    );
    while events.len() < 2 {
        events.push(owner.next_event().await?);
    }
    assert!(
        matches!(
            events.as_slice(),
            [
                Event::PlaybackStarted { participant: who, segment },
                Event::PlaybackDelivered { participant: whom, segment: delivered },
            ] if who == participant && whom == participant
                && *segment == greeting && *delivered == greeting
        ),
        "after `play` came {events:?}"
    );
    // Half a second has passed since the application stopped listening.
    assert!(
        owner.heard.is_empty(),
        "the application was sent audio it no longer listens to"
    );

    let heard = phone.hear(3600).await?;
    let (played, other) = (
        level(&heard, Phone::RATE, 1000),
        level(&heard, Phone::RATE, 700),
    );
    assert!(
        played > 7000.0 && other < 500.0,
        "the caller heard the played tone at {played:.0} and another at {other:.0}"
    );
    Ok(())
}

/// A flush drops what is queued and says how much of each segment the
/// caller was given; what is queued afterwards is played.
async fn flushes_the_queue(owner: &mut Owner, participant: &ParticipantId) -> TestResult {
    let long: SegmentId = "long".parse()?;
    let behind_it: SegmentId = "behind-it".parse()?;
    let play = |segment: &SegmentId| Command::Play {
        participant: participant.clone(),
        segment: segment.clone(),
    };

    owner.command(6, play(&long))?;
    // Five seconds, in frames as large as a playback frame may be.
    let audio = tone(1000, 16_000, 0, 80_000);
    let mut parts = audio.chunks(32_000).peekable();
    while let Some(part) = parts.next() {
        owner.audio(participant, &long, part, parts.peek().is_none())?;
    }
    owner.command(7, play(&behind_it))?;
    owner.audio(participant, &behind_it, &tone(1000, 16_000, 0, 3200), true)?;
    let (first, mut events) = owner.outcome_of(6).await?;
    let (second, more) = owner.outcome_of(7).await?;
    events.extend(more);
    assert!(
        matches!(
            (&first, &second),
            (CommandOutcome::Accepted, CommandOutcome::Accepted)
        ),
        "two `play` commands were {first:?} and {second:?}"
    );
    if events.is_empty() {
        events.push(owner.next_event().await?);
    }
    assert!(
        matches!(events.as_slice(), [Event::PlaybackStarted { segment, .. }] if *segment == long),
        "after two `play` commands came {events:?}"
    );

    owner.command(
        8,
        Command::FlushPlayback {
            participant: participant.clone(),
        },
    )?;
    let (outcome, mut events) = owner.outcome_of(8).await?;
    assert!(
        matches!(outcome, CommandOutcome::Accepted),
        "`flush_playback` was {outcome:?}"
    );
    while events.len() < 2 {
        events.push(owner.next_event().await?);
    }
    assert!(
        matches!(
            events.as_slice(),
            [
                Event::PlaybackDropped { segment: playing, delivered_ms: played, .. },
                Event::PlaybackDropped { segment: waiting, delivered_ms: 0, .. },
            ] if *playing == long && *waiting == behind_it && (20..5000).contains(played)
        ),
        "after `flush_playback` came {events:?}"
    );

    // What is queued after a flush is played: the flush did not stop playing.
    let afterwards: SegmentId = "afterwards".parse()?;
    owner.command(9, play(&afterwards))?;
    owner.audio(participant, &afterwards, &tone(1000, 16_000, 0, 3200), true)?;
    let (outcome, mut events) = owner.outcome_of(9).await?;
    assert!(
        matches!(outcome, CommandOutcome::Accepted),
        "`play` after a flush was {outcome:?}"
    );
    while events.len() < 2 {
        events.push(owner.next_event().await?);
    }
    assert!(
        matches!(
            events.as_slice(),
            [
                Event::PlaybackStarted { segment, .. },
                Event::PlaybackDelivered { segment: delivered, .. },
            ] if *segment == afterwards && *delivered == afterwards
        ),
        "after `play` that followed a flush came {events:?}"
    );
    Ok(())
}

/// The application hears a caller and plays to them; the queue is flushed;
/// the caller hangs up with a segment still queued.
async fn the_application_hears_and_speaks(stand: &Stand) -> TestResult {
    let (caller, mut phone, mut owner) = stand.call_from_a_phone().await?;
    let ControllerMessage::Hello {
        opening: Opening::Started { first, .. },
        ..
    } = owner.next().await?
    else {
        return Err("the call did not open with a hello".into());
    };
    let participant = first.id;
    owner.send(&ApplicationMessage::Accept)?;
    hears_the_caller(&mut owner, &participant, 3).await?;
    plays_to_the_caller(&mut owner, &mut phone, &participant, 5).await?;
    flushes_the_queue(&mut owner, &participant).await?;

    // A name still in the queue cannot be queued again.
    let pending: SegmentId = "pending".parse()?;
    for id in [10, 11] {
        owner.command(
            id,
            Command::Play {
                participant: participant.clone(),
                segment: pending.clone(),
            },
        )?;
    }
    let (first, _) = owner.outcome_of(10).await?;
    let (second, _) = owner.outcome_of(11).await?;
    assert!(
        matches!(
            (&first, &second),
            (
                CommandOutcome::Accepted,
                CommandOutcome::Rejected {
                    reason: CommandRejection::SegmentAlreadyQueued
                }
            )
        ),
        "queueing one name twice was {first:?} and {second:?}"
    );

    // The caller hangs up: what is still queued for them is dropped, and
    // that is said before the word that they are gone.
    stand
        .control
        .request("DELETE", &format!("channels/{caller}"))
        .await?;
    let mut events = Vec::new();
    while events.len() < 3 {
        events.push(owner.next_event().await?);
    }
    assert!(
        matches!(
            events.as_slice(),
            [
                Event::PlaybackDropped { segment, delivered_ms: 0, .. },
                Event::ParticipantLeft { departure: Departure::HungUp, .. },
                Event::ConversationEnded { reason: EndReason::LastParticipantLeft },
            ] if *segment == pending
        ),
        "after the caller hung up came {events:?}"
    );
    owner.closed().await?;
    stand.expect_no_channels("after a call with audio").await?;
    stand.expect_no_bridges("after a call with audio").await
}

/// The operator of the settings calls the node's number over SIP: the call
/// arrives as a conversation, with the number it came from.
async fn a_call_arrives_from_the_operator(stand: &Stand) -> TestResult {
    let caller = stand
        .network
        .control
        .create_channel(&format!(
            "endpoint=PJSIP/{DIALED_IN_URL}@the-node&app={FAR_END}&callerId={CALLER_IN_URL}"
        ))
        .await?;
    let mut owner = Owner::accept(&stand.application).await?;
    let ControlMessageHello { dialed, first, .. } = hello(&mut owner).await?;
    assert_eq!(dialed, DIALED);
    assert_eq!(first.medium, ParticipantMedium::TelephoneNetwork);
    assert_eq!(
        first.number.as_ref().map(|number| number.as_str()),
        Some(CALLER),
        "the caller's number did not arrive with the call"
    );

    owner.send(&ApplicationMessage::Accept)?;
    owner.command(
        1,
        Command::Answer {
            participant: first.id.clone(),
        },
    )?;
    let (outcome, mut events) = owner.outcome_of(1).await?;
    assert!(
        matches!(outcome, CommandOutcome::Accepted),
        "answer was {outcome:?}"
    );
    if events.is_empty() {
        events.push(owner.next_event().await?);
    }
    assert!(
        matches!(events.as_slice(), [Event::ParticipantAnswered { .. }]),
        "after the answer came {events:?}"
    );

    // The caller speaks and listens through the trunk: the application
    // hears them and they hear the application, as on any other call.
    let (telephone, mut phone) = stand.network.give_a_phone(&caller).await?;
    hears_the_caller(&mut owner, &first.id, 3).await?;
    plays_to_the_caller(&mut owner, &mut phone, &first.id, 5).await?;
    digits_go_both_ways(stand, &mut owner, &caller, &first.id, 6).await?;

    // The caller hangs up, on the network's side.
    for gone in [
        format!("channels/{caller}"),
        format!("channels/{telephone}"),
        "bridges/the-callers-phone".to_owned(),
    ] {
        stand.network.control.request("DELETE", &gone).await?;
    }
    let mut events = vec![owner.next_event().await?, owner.next_event().await?];
    assert!(
        matches!(
            events.as_mut_slice(),
            [
                Event::ParticipantLeft {
                    departure: Departure::HungUp,
                    ..
                },
                Event::ConversationEnded {
                    reason: EndReason::LastParticipantLeft
                },
            ]
        ),
        "after the operator's caller hung up came {events:?}"
    );
    owner.closed().await?;
    stand
        .expect_no_channels("after a call from the operator")
        .await
}

/// The node's Asterisk has a channel whose identifier holds `part`, or —
/// for an empty `part` — a channel of an operator's call that arrived:
/// waited for, since a call takes a moment to arrive.
async fn node_channel(stand: &Stand, part: &str) -> TestResult<String> {
    timeout(STEP, async {
        loop {
            let channels = read_asterisk(&stand.control.request("GET", "channels").await?.1)?;
            let found = channels
                .as_array()
                .into_iter()
                .flatten()
                .find_map(|channel| {
                    let id = channel.get("id").and_then(serde_json::Value::as_str)?;
                    let name = channel.get("name").and_then(serde_json::Value::as_str)?;
                    let wanted = if part.is_empty() {
                        name.starts_with("PJSIP/") && !id.contains(".participant-")
                    } else {
                        id.contains(part)
                    };
                    wanted.then(|| id.to_owned())
                });
            if let Some(found) = found {
                return Ok::<_, Box<dyn Error>>(found);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| format!("the node has no channel {part:?}"))?
}

/// The node's Asterisk says a channel's signalling travels over a secure
/// transport and its audio is encrypted.
async fn secured(stand: &Stand, channel: &str) -> TestResult {
    for (what, function) in [
        ("signalling", "CHANNEL%28pjsip%2Csecure%29"),
        ("audio", "CHANNEL%28rtp%2Csecure%2Caudio%29"),
    ] {
        let (status, body) = stand
            .control
            .request(
                "GET",
                &format!("channels/{channel}/variable?variable={function}"),
            )
            .await?;
        assert!(
            status == 200 && body.contains("\"value\":\"1\""),
            "the {what} of channel {channel} is not secured: {status} {body}"
        );
    }
    Ok(())
}

/// Calls over TLS, with encrypted audio: the operator reached over TLS calls
/// the node, and the node calls through it. Asterisk itself says each call's
/// signalling and audio are secured, and the application hears and speaks
/// as on any call.
async fn calls_go_over_tls(stand: &Stand) -> TestResult {
    let caller = stand
        .network
        .control
        .create_channel(&format!(
            "endpoint=PJSIP/{DIALED_IN_URL}@the-node-secure&app={FAR_END}&callerId={CALLER_IN_URL}"
        ))
        .await?;
    let mut owner = Owner::accept(&stand.application).await?;
    let first = hello(&mut owner).await?.first.id;
    owner.send(&ApplicationMessage::Accept)?;
    let participant = first.clone();
    let mut events = accepted(&mut owner, 1, Command::Answer { participant }).await?;
    if events.is_empty() {
        events.push(owner.next_event().await?);
    }
    let (telephone, mut phone) = stand.network.give_a_phone(&caller).await?;
    hears_the_caller(&mut owner, &first, 2).await?;
    plays_to_the_caller(&mut owner, &mut phone, &first, 4).await?;
    secured(stand, &node_channel(stand, "").await?).await?;

    let (outcome, mut events) = dial(&mut owner, 5, ANSWERS, "secure", 5000).await?;
    let CommandOutcome::AcceptedParticipant { participant } = outcome else {
        return Err(format!("dialling through the operator over TLS was {outcome:?}").into());
    };
    while events.len() < 2 {
        events.push(owner.next_event().await?);
    }
    assert!(
        matches!(
            events.as_slice(),
            [
                Event::ParticipantRinging { participant: ringing },
                Event::ParticipantAnswered { participant: answered },
            ] if ringing.id == participant && *answered == participant
        ),
        "a call through the operator over TLS came to {events:?}"
    );
    secured(stand, &node_channel(stand, ".participant-").await?).await?;
    ends_it(&mut owner, 6, 2).await?;
    the_telephone_is_put_down(stand, &telephone).await?;
    stand.expect_no_channels("after calls over TLS").await?;
    stand
        .network
        .control
        .expect_none("channels", "after calls over TLS")
        .await
}

/// Tell the controller the node's certificate was renewed, as whoever renews
/// it does — SIGHUP — and wait until it says Asterisk's configuration
/// follows: the `times`th time it says so.
async fn the_certificate_is_renewed(stand: &Stand, times: usize) -> TestResult {
    let pid = stand
        .controller
        .child
        .id()
        .ok_or("the controller is not running")?;
    let sent = std::process::Command::new("kill")
        .args(["-HUP", &pid.to_string()])
        .status()?;
    if !sent.success() {
        return Err("SIGHUP could not be sent to the controller".into());
    }
    let followed = timeout(STEP, async {
        loop {
            let said = stand
                .controller
                .output
                .lock()
                .await
                .iter()
                .filter(|line| line.contains("SIGHUP — Asterisk's configuration follows"))
                .count();
            if said >= times {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    if followed.is_err() {
        return Err("the controller did not follow a renewed certificate".into());
    }
    Ok(())
}

/// Open a TLS connection to where the node listens for operators, trusting
/// `certificate` alone: it succeeds only if that is the certificate the
/// node presents.
async fn presents(stand: &Stand, certificate: &[u8]) -> TestResult {
    let address = stand.controller_argument("--sip-tls")?;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(rustls::pki_types::CertificateDer::from(
        certificate.to_vec(),
    ))?;
    let trusting = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let stream = TcpStream::connect(&address).await?;
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1")?;
    timeout(
        STEP,
        tokio_rustls::TlsConnector::from(Arc::new(trusting)).connect(name, stream),
    )
    .await??;
    Ok(())
}

/// The node's certificate is renewed in place, and whoever renewed it says
/// so with SIGHUP: a new connection is presented the renewed one from then
/// on, with no restart — and one already open goes on as it was.
async fn a_renewed_certificate_is_taken_in(stand: &Stand) -> TestResult {
    let Certificates {
        node: certificate,
        node_key: key,
        node_der: before,
        ..
    } = &stand.certificates;
    let kept = (fs::read(certificate)?, fs::read(key)?);
    let renewed = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()])?;
    fs::write(certificate, renewed.cert.pem())?;
    fs::write(key, renewed.signing_key.serialize_pem())?;
    the_certificate_is_renewed(stand, 1).await?;
    presents(stand, renewed.cert.der())
        .await
        .map_err(|error| format!("the renewed certificate is not presented: {error}"))?;

    fs::write(certificate, &kept.0)?;
    fs::write(key, &kept.1)?;
    the_certificate_is_renewed(stand, 2).await?;
    presents(stand, before)
        .await
        .map_err(|error| format!("the certificate before is not presented again: {error}"))?;
    if stand.asterisk.said("Failed to restart TLS transport").await {
        return Err("Asterisk could not take a renewed certificate in".into());
    }
    Ok(())
}

/// The same call from an address that is no operator's is refused by
/// Asterisk itself: it never becomes a channel of the node.
async fn a_call_from_elsewhere_is_refused(stand: &Stand) -> TestResult {
    stand
        .network
        .control
        .create_channel(&format!(
            "endpoint=PJSIP/{DIALED_IN_URL}@the-node-from-elsewhere&app={FAR_END}"
        ))
        .await?;
    // The network is told no, and its own end of the call is gone.
    stand
        .network
        .control
        .expect_none("channels", "after a call the node refused")
        .await?;
    if !stand.asterisk.said("No matching endpoint found").await {
        return Err("the node's Asterisk did not refuse a call from a stranger".into());
    }
    stand
        .expect_no_channels("after a call from a stranger")
        .await
}

/// Send a `dial` command and return its outcome with the events that came first.
async fn dial(
    owner: &mut Owner,
    id: u64,
    number: &str,
    line: &str,
    answer_limit_ms: u64,
) -> TestResult<(CommandOutcome, Vec<Event>)> {
    owner.command(
        id,
        Command::Dial {
            number: number.parse()?,
            line: line.parse()?,
            answer_limit_ms,
        },
    )?;
    owner.outcome_of(id).await
}

/// Dial a number that will not be answered and return how the call ended.
/// On the way the participant is announced: accepted, then ringing.
async fn dial_in_vain(
    owner: &mut Owner,
    id: u64,
    number: &str,
    limit_ms: u64,
) -> TestResult<Departure> {
    let (outcome, mut events) = dial(owner, id, number, "main", limit_ms).await?;
    let CommandOutcome::AcceptedParticipant { participant } = outcome else {
        return Err(format!("dialling {number} was {outcome:?}").into());
    };
    while events.len() < 2 {
        events.push(owner.next_event().await?);
    }
    match events.as_slice() {
        [
            Event::ParticipantRinging {
                participant: ringing,
            },
            Event::ParticipantLeft {
                participant: left,
                departure,
            },
        ] if ringing.id == participant && *left == participant => Ok(departure.clone()),
        other => Err(format!("after dialling {number} came {other:?}").into()),
    }
}

/// The application adds participants by dialling: the node judges every
/// number itself, shows the line's number, keeps to the line's limit and
/// says truthfully how each call ended.
async fn the_application_dials(stand: &Stand) -> TestResult {
    let (caller, mut owner) = stand.call().await?;
    let first = hello(&mut owner).await?.first;
    owner.send(&ApplicationMessage::Accept)?;
    owner.command(
        1,
        Command::Answer {
            participant: first.id.clone(),
        },
    )?;
    // The word that the caller answered may come before or after the
    // answer to the command; either way it is one event.
    let (_, mut events) = owner.outcome_of(1).await?;
    if events.is_empty() {
        events.push(owner.next_event().await?);
    }
    assert!(
        matches!(events.as_slice(), [Event::ParticipantAnswered { .. }]),
        "after the answer came {events:?}"
    );

    refuses_numbers_it_may_not_dial(&mut owner).await?;
    a_dialled_number_answers(stand, &mut owner).await?;

    // Calls that are not answered, each for its own reason. Every one of
    // them finds the line free again: the place of the call before it was
    // given back.
    let busy = dial_in_vain(&mut owner, 9, BUSY, 5000).await?;
    assert!(
        matches!(busy, Departure::Busy),
        "a busy number ended as {busy:?}"
    );
    let unanswered = dial_in_vain(&mut owner, 10, RINGS, 1000).await?;
    assert!(
        matches!(unanswered, Departure::NotAnswered),
        "an unanswered call ended as {unanswered:?}"
    );
    let failed = dial_in_vain(&mut owner, 11, FAILS, 5000).await?;
    assert!(
        matches!(failed, Departure::DialFailed(FAILS_WITH)),
        "a call the network failed ended as {failed:?}"
    );

    the_dialled_one_outlives_the_caller(stand, &mut owner, &caller, &first.id).await?;
    stand
        .expect_no_channels("after a conversation with dialled participants")
        .await?;
    stand
        .network
        .control
        .expect_none("channels", "after a conversation with dialled participants")
        .await
}

/// The request with which the test asks the node to start a conversation.
const REQUEST: &str = "callback-1";
/// The handler the test names in it.
const HANDLER: &str = "secretary";

/// A request to start a conversation by dialling `number` on `line`.
fn start_request(number: &str, line: &str, answer_limit_ms: u64) -> TestResult<ApplicationRequest> {
    Ok(ApplicationRequest::StartConversation {
        request: REQUEST.parse()?,
        handler: HANDLER.parse()?,
        number: number.parse()?,
        line: line.parse()?,
        answer_limit_ms,
    })
}

/// Ask the node to start a conversation with `number`, take the
/// conversation it offers and see it opened as one started by code, its
/// first participant ringing. Returns the owner and that participant.
async fn a_started_conversation(
    stand: &Stand,
    id: u64,
    number: &str,
    answer_limit_ms: u64,
) -> TestResult<(Owner, ParticipantId)> {
    let outcome = stand
        .ask(id, start_request(number, "main", answer_limit_ms)?)
        .await?;
    let RequestOutcome::ConversationStarted { conversation } = outcome else {
        return Err(format!("a request to call {number} was answered {outcome:?}").into());
    };
    let mut owner = Owner::accept(&stand.application).await?;
    let ControllerMessage::Hello {
        conversation: offered,
        opening:
            Opening::Started {
                origin: Origin::StartedByCode { request, handler },
                first,
            },
        ..
    } = owner.next().await?
    else {
        return Err("the conversation was not offered as one started by code".into());
    };
    assert!(
        offered == conversation
            && request.as_str() == REQUEST
            && handler.as_str() == HANDLER
            && first.number.as_ref().map(|number| number.as_str()) == Some(number),
        "the conversation {conversation:?} was offered as {offered:?}, for {request:?} and \
         {handler:?}, with {first:?}"
    );
    owner.send(&ApplicationMessage::Accept)?;
    Ok((owner, first.id))
}

/// The application asks the node to start a conversation by dialling its
/// first participant. The node judges the number as it judges a `dial`, and
/// answers the request once Asterisk has placed the call; the conversation
/// is offered as one started by code, its first participant ringing, to
/// whichever instance takes it. A call nobody answers ends it.
async fn the_application_starts_a_conversation(stand: &Stand) -> TestResult {
    for (id, number, line, reason) in [
        (1, ANSWERS, "nowhere", RequestRejection::UnknownLine),
        (
            2,
            "+442071838750",
            "main",
            RequestRejection::DestinationNotAllowed,
        ),
    ] {
        let outcome = stand.ask(id, start_request(number, line, 5000)?).await?;
        assert!(
            matches!(outcome, RequestOutcome::Rejected { reason: given } if given == reason),
            "a request to call {number} on {line} was answered {outcome:?}"
        );
    }

    let (mut owner, first) = a_started_conversation(stand, 3, ANSWERS, 5000).await?;
    let answered = owner.next_event().await?;
    assert!(
        matches!(&answered, Event::ParticipantAnswered { participant } if *participant == first),
        "the first participant of a started conversation answered as {answered:?}"
    );
    ends_it(&mut owner, 1, 1).await?;

    let (mut owner, first) = a_started_conversation(stand, 4, RINGS, 1000).await?;
    let told = [owner.next_event().await?, owner.next_event().await?];
    assert!(
        matches!(
            &told,
            [
                Event::ParticipantLeft { participant, departure: Departure::NotAnswered },
                Event::ConversationEnded { reason: EndReason::LastParticipantLeft },
            ] if *participant == first
        ),
        "a started conversation nobody answered was told as {told:?}"
    );
    owner.closed().await?;
    stand
        .expect_no_channels("after conversations the application started")
        .await?;
    stand
        .network
        .control
        .expect_none("channels", "after conversations the application started")
        .await
}

/// A request identifier used twice on the service connection breaks the
/// protocol, as a command identifier does on a conversation's: the node
/// ends the connection, and opens it again.
async fn a_request_identifier_used_twice(stand: &Stand) -> TestResult {
    let mut kept = stand.service.lock().await;
    let service = kept
        .as_mut()
        .ok_or("no service connection has been welcomed")?;
    service.send(&ApplicationServiceMessage::Request {
        id: 3,
        request: start_request(ANSWERS, "main", 5000)?,
    })?;
    if service.next().await.is_ok() {
        return Err("a request identifier used twice was answered".into());
    }
    let mut again = ServiceEnd::accept(&stand.application).await?;
    let NodeMessage::Hello { .. } = again.next().await? else {
        return Err("the service connection opened again without a hello".into());
    };
    again.send(&ApplicationServiceMessage::Welcome)?;
    *kept = Some(again);
    if !stand
        .controller
        .comes_to_say("the request identifier 3 was used a second time")
        .await
    {
        return Err("the controller did not say why it ended the service connection".into());
    }
    stand
        .expect_no_channels("after a request identifier was used twice")
        .await
}

/// Numbers the node does not dial, each for its own reason.
async fn refuses_numbers_it_may_not_dial(owner: &mut Owner) -> TestResult {
    for (id, number, line, reason) in [
        (2, ANSWERS, "nowhere", CommandRejection::UnknownLine),
        // Another country; a premium-rate number of an allowed one.
        (
            3,
            "+442071838750",
            "main",
            CommandRejection::DestinationNotAllowed,
        ),
        (
            4,
            "+19002345678",
            "main",
            CommandRejection::DestinationNotAllowed,
        ),
        // A country the line allows and its operator does not carry.
        (
            5,
            "+15062345678",
            "main",
            CommandRejection::NoOperatorForDestination,
        ),
    ] {
        let (outcome, _) = dial(owner, id, number, line, 5000).await?;
        assert!(
            matches!(&outcome, CommandOutcome::Rejected { reason: given } if *given == reason),
            "dialling {number} on {line} was {outcome:?}"
        );
    }
    Ok(())
}

/// A call that is answered: the network sees the line's number, the line
/// is then full, the application hears and speaks to the one it dialled,
/// and removes them.
async fn a_dialled_number_answers(stand: &Stand, owner: &mut Owner) -> TestResult {
    let (outcome, mut events) = dial(owner, 6, ANSWERS, "main", 5000).await?;
    let CommandOutcome::AcceptedParticipant {
        participant: second,
    } = outcome
    else {
        return Err(format!("dialling a number that answers was {outcome:?}").into());
    };
    while events.len() < 2 {
        events.push(owner.next_event().await?);
    }
    assert!(
        matches!(
            events.as_slice(),
            [
                Event::ParticipantRinging { participant: ringing },
                Event::ParticipantAnswered { participant: answered },
            ] if ringing.id == second && *answered == second
                && ringing.number.as_ref().map(|number| number.as_str()) == Some(ANSWERS)
        ),
        "after dialling came {events:?}"
    );
    // The network was shown the line's number — and it asked the node who
    // it is before it took the call.
    let (_, at_the_network) = stand.network.control.request("GET", "channels").await?;
    let at_the_network = read_asterisk(&at_the_network)?;
    let network_end = at_the_network
        .as_array()
        .and_then(|channels| channels.first())
        .ok_or("the network has no channel of the call the node placed")?;
    assert_eq!(
        network_end.pointer("/caller/number"),
        Some(&serde_json::json!(DIALED))
    );
    let network_end = network_end
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or("a channel without an identifier")?
        .to_owned();

    // The line carries one call at a time.
    let (outcome, _) = dial(owner, 7, ANSWERS, "main", 5000).await?;
    assert!(
        matches!(
            outcome,
            CommandOutcome::Rejected {
                reason: CommandRejection::OutboundLimitReached
            }
        ),
        "a call over the line's limit was {outcome:?}"
    );

    let (telephone, mut phone) = stand.network.give_a_phone(&network_end).await?;
    hears_the_caller(owner, &second, 20).await?;
    plays_to_the_caller(owner, &mut phone, &second, 22).await?;

    owner.command(
        8,
        Command::Remove {
            participant: second.clone(),
        },
    )?;
    let (outcome, mut events) = owner.outcome_of(8).await?;
    if events.is_empty() {
        events.push(owner.next_event().await?);
    }
    assert!(
        matches!(outcome, CommandOutcome::Accepted)
            && matches!(
                events.as_slice(),
                [Event::ParticipantLeft { participant, departure: Departure::Removed }]
                    if *participant == second
            ),
        "removing a dialled participant was {outcome:?}, then {events:?}"
    );
    for gone in [
        format!("channels/{telephone}"),
        "bridges/the-callers-phone".to_owned(),
    ] {
        stand.network.control.request("DELETE", &gone).await?;
    }
    stand
        .network
        .control
        .expect_none("channels", "after a dialled participant was removed")
        .await
}

/// The one who called in hangs up; the one who was dialled stays, and the
/// conversation with them goes on under the application's control.
async fn the_dialled_one_outlives_the_caller(
    stand: &Stand,
    owner: &mut Owner,
    caller: &str,
    first: &ParticipantId,
) -> TestResult {
    let (outcome, mut events) = dial(owner, 12, ANSWERS, "main", 5000).await?;
    let CommandOutcome::AcceptedParticipant { participant: third } = outcome else {
        return Err(format!("dialling again was {outcome:?}").into());
    };
    while events.len() < 2 {
        events.push(owner.next_event().await?);
    }
    stand
        .control
        .request("DELETE", &format!("channels/{caller}"))
        .await?;
    let left = owner.next_event().await?;
    assert!(
        matches!(&left, Event::ParticipantLeft { participant, departure: Departure::HungUp } if participant == first),
        "after the caller hung up came {left:?}"
    );
    owner.command(
        13,
        Command::Remove {
            participant: third.clone(),
        },
    )?;
    let (outcome, mut events) = owner.outcome_of(13).await?;
    while events.len() < 2 {
        events.push(owner.next_event().await?);
    }
    assert!(
        matches!(outcome, CommandOutcome::Accepted)
            && matches!(
                events.as_slice(),
                [
                    Event::ParticipantLeft { participant, departure: Departure::Removed },
                    Event::ConversationEnded { reason: EndReason::LastParticipantLeft },
                ] if *participant == third
            ),
        "removing the last participant was {outcome:?}, then {events:?}"
    );
    owner.closed().await
}

/// The tone the second participant of a connected conversation says, Hz.
const SECOND_SAYS: u32 = 700;
/// How long the second participant is held when their audio is lost, as an
/// owner sets it before it is lost — or the controller is.
const SECOND_HOLD_LIMIT_MS: u64 = 15_000;

/// Send a command that only names participants and expect it accepted.
async fn accepted(owner: &mut Owner, id: u64, command: Command) -> TestResult<Vec<Event>> {
    let said = format!("{command:?}");
    owner.command(id, command)?;
    let (outcome, events) = owner.outcome_of(id).await?;
    if matches!(outcome, CommandOutcome::Accepted) {
        Ok(events)
    } else {
        Err(format!("{said} was {outcome:?}").into())
    }
}

/// Connections the node does not make, each for its own reason — and a
/// participant who is connected to nobody is taken out of nothing.
async fn refuses_connections_it_cannot_make(
    owner: &mut Owner,
    first: &ParticipantId,
) -> TestResult {
    let nobody: ParticipantId = "p-9".parse()?;
    for (id, participants, reason) in [
        (1, vec![first.clone()], CommandRejection::TooFewParticipants),
        (
            2,
            vec![first.clone(), first.clone()],
            CommandRejection::TooFewParticipants,
        ),
        (
            3,
            vec![first.clone(), nobody.clone()],
            CommandRejection::UnknownParticipant,
        ),
    ] {
        owner.command(id, Command::Connect { participants })?;
        let (outcome, _) = owner.outcome_of(id).await?;
        assert!(
            matches!(&outcome, CommandOutcome::Rejected { reason: given } if *given == reason),
            "a connection that cannot be made was {outcome:?}, not rejected as {reason:?}"
        );
    }

    // One who is being called and has not answered cannot be connected yet.
    let (outcome, mut events) = dial(owner, 4, RINGS, "wide", 1000).await?;
    let CommandOutcome::AcceptedParticipant { participant: rings } = outcome else {
        return Err(format!("dialling a number that rings was {outcome:?}").into());
    };
    owner.command(
        5,
        Command::Connect {
            participants: vec![first.clone(), rings.clone()],
        },
    )?;
    let (outcome, more) = owner.outcome_of(5).await?;
    events.extend(more);
    assert!(
        matches!(
            outcome,
            CommandOutcome::Rejected {
                reason: CommandRejection::ParticipantNotInRequiredState
            }
        ),
        "connecting a participant who still rings was {outcome:?}"
    );
    while !events.iter().any(
        |event| matches!(event, Event::ParticipantLeft { participant, .. } if *participant == rings),
    ) {
        events.push(owner.next_event().await?);
    }

    owner.command(
        6,
        Command::Separate {
            participant: nobody,
        },
    )?;
    let (outcome, _) = owner.outcome_of(6).await?;
    assert!(
        matches!(
            outcome,
            CommandOutcome::Rejected {
                reason: CommandRejection::UnknownParticipant
            }
        ),
        "taking an unknown participant out of a connection was {outcome:?}"
    );
    let participant = first.clone();
    accepted(owner, 7, Command::Separate { participant }).await?;
    Ok(())
}

/// The application hears each of two participants by themselves: in what
/// comes as the audio of one there is their tone and not the other's.
async fn hears_each_by_themselves(
    owner: &mut Owner,
    first: &ParticipantId,
    second: &ParticipantId,
) -> TestResult {
    owner.heard.clear();
    let (mut of_first, mut of_second) = (Vec::new(), Vec::new());
    // Half a second of each, after what was on its way when this began.
    while of_first.len() < 12_800 || of_second.len() < 12_800 {
        for frame in owner.hear(1).await? {
            let AudioFrame::Heard {
                participant, audio, ..
            } = frame
            else {
                return Err(format!("the controller sent {frame:?}").into());
            };
            if participant == *first {
                of_first.extend(from_bytes(&audio));
            } else if participant == *second {
                of_second.extend(from_bytes(&audio));
            } else {
                return Err(format!("audio of {participant:?}, whom nobody listens to").into());
            }
        }
    }
    for (heard, theirs, other) in [
        (&of_first, Phone::SAYS, SECOND_SAYS),
        (&of_second, SECOND_SAYS, Phone::SAYS),
    ] {
        let recent = heard.get(heard.len().saturating_sub(8000)..).unwrap_or(&[]);
        let (own, others) = (level(recent, 16_000, theirs), level(recent, 16_000, other));
        assert!(
            own > 7000.0 && others < 500.0,
            "of the participant who says {theirs} Hz the application heard that tone at {own:.0} \
             and the other's at {others:.0}"
        );
    }
    Ok(())
}

/// Queue a tone of a second and a half for a participant and wait until it
/// has begun: from then on the participant is hearing it.
async fn begins_to_play(owner: &mut Owner, id: u64, participant: &ParticipantId) -> TestResult {
    let segment: SegmentId = format!("said-{id}").parse()?;
    owner.command(
        id,
        Command::Play {
            participant: participant.clone(),
            segment: segment.clone(),
        },
    )?;
    owner.audio(participant, &segment, &tone(1000, 16_000, 0, 24_000), true)?;
    let (outcome, mut events) = owner.outcome_of(id).await?;
    assert!(
        matches!(outcome, CommandOutcome::Accepted),
        "`play` was {outcome:?}"
    );
    if events.is_empty() {
        events.push(owner.next_event().await?);
    }
    assert!(
        matches!(events.as_slice(), [Event::PlaybackStarted { segment: started, .. }] if *started == segment),
        "after `play` came {events:?}"
    );
    Ok(())
}

/// What was begun by [`begins_to_play`] is delivered.
async fn is_delivered(owner: &mut Owner) -> TestResult {
    let delivered = owner.next_event().await?;
    assert!(
        matches!(delivered, Event::PlaybackDelivered { .. }),
        "a segment that was being played ended as {delivered:?}"
    );
    Ok(())
}

/// The second participant of a connected conversation: dialled, answered,
/// and given a telephone at the network's end that says a tone of its own.
/// Returns the participant, the network's end of the call and the telephone
/// with its channel.
async fn a_second_participant(
    stand: &Stand,
    owner: &mut Owner,
) -> TestResult<(ParticipantId, String, String, Phone)> {
    let (outcome, mut events) = dial(owner, 10, ANSWERS, "wide", 5000).await?;
    let CommandOutcome::AcceptedParticipant { participant } = outcome else {
        return Err(format!("dialling the second participant was {outcome:?}").into());
    };
    while events.len() < 2 {
        events.push(owner.next_event().await?);
    }
    let at_the_network = read_asterisk(&stand.network.control.request("GET", "channels").await?.1)?;
    let network_end = at_the_network
        .as_array()
        .into_iter()
        .flatten()
        .find(|channel| channel.pointer("/dialplan/exten") == Some(&serde_json::json!(ANSWERS)))
        .and_then(|channel| channel.get("id"))
        .and_then(serde_json::Value::as_str)
        .ok_or("the network has no channel of the call the node placed")?
        .to_owned();
    let (telephone, phone) = stand
        .network
        .give_a_phone_that_says(&network_end, SECOND_SAYS, "the-second-phone")
        .await?;
    Ok((participant, network_end, telephone, phone))
}

/// The application connects participants to each other and takes them out
/// of the connection.
///
/// Connected, they hear each other. The application goes on hearing each of
/// them by themselves, and what it says to one the other does not hear. A
/// participant connected to one of a group joins the whole group. One who
/// is left alone in a group — the other was taken out, removed, or hung up
/// — is connected to nobody again, and the application still reaches them.
async fn the_application_connects_participants(stand: &Stand) -> TestResult {
    let (caller, mut phone, mut owner) = stand.call_from_a_phone().await?;
    let first = hello(&mut owner).await?.first.id;
    owner.send(&ApplicationMessage::Accept)?;
    refuses_connections_it_cannot_make(&mut owner, &first).await?;

    let (second, network_end, telephone, mut second_phone) =
        a_second_participant(stand, &mut owner).await?;
    for (id, participant) in [(11, &first), (12, &second)] {
        let participant = participant.clone();
        accepted(&mut owner, id, Command::Listen { participant }).await?;
    }
    hears_each_by_themselves(&mut owner, &first, &second).await?;

    let both = vec![first.clone(), second.clone()];
    let connect = |participants: &[ParticipantId]| Command::Connect {
        participants: participants.to_vec(),
    };
    accepted(&mut owner, 13, connect(&both)).await?;
    phone.hears(SECOND_SAYS, true).await?;
    second_phone.hears(Phone::SAYS, true).await?;
    hears_each_by_themselves(&mut owner, &first, &second).await?;

    // What the application says to one of them, the other does not hear.
    begins_to_play(&mut owner, 14, &first).await?;
    phone.forget();
    second_phone.forget();
    let (to_first, to_second) = (phone.hear(4000).await?, second_phone.hear(4000).await?);
    let heard = [
        level(&to_first, Phone::RATE, 1000),
        level(&to_first, Phone::RATE, SECOND_SAYS),
        level(&to_second, Phone::RATE, 1000),
        level(&to_second, Phone::RATE, Phone::SAYS),
    ];
    assert!(
        matches!(heard, [said, other, leaked, others] if said > 7000.0 && other > 7000.0 && leaked < 500.0 && others > 7000.0),
        "said to one of two connected: they heard it, the other, and the other heard it and them \
         at {heard:.0?}"
    );
    is_delivered(&mut owner).await?;

    // A third, connected to one of the two, is connected to both.
    let (outcome, mut events) = dial(&mut owner, 15, ANSWERS, "wide", 5000).await?;
    let CommandOutcome::AcceptedParticipant { participant: third } = outcome else {
        return Err(format!("dialling the third participant was {outcome:?}").into());
    };
    while events.len() < 2 {
        events.push(owner.next_event().await?);
    }
    accepted(&mut owner, 16, connect(&[second.clone(), third.clone()])).await?;
    stand
        .expect_a_group_of(3, "after a third was connected")
        .await?;
    phone.hears(SECOND_SAYS, true).await?;

    // The first is taken out: the two others stay connected, the first
    // hears nobody and is reached by the application as before.
    let participant = first.clone();
    accepted(&mut owner, 17, Command::Separate { participant }).await?;
    stand
        .expect_a_group_of(2, "after one of three was taken out")
        .await?;
    phone.hears(SECOND_SAYS, false).await?;
    second_phone.hears(Phone::SAYS, false).await?;
    begins_to_play(&mut owner, 18, &first).await?;
    phone.hears(1000, true).await?;
    is_delivered(&mut owner).await?;
    hears_each_by_themselves(&mut owner, &first, &second).await?;

    let mut ends = Ends {
        caller,
        phone,
        network_end,
        telephone,
        second_phone,
    };
    those_left_alone_are_reached(stand, &mut owner, &mut ends, [&first, &second, &third]).await
}

/// The far ends of a connected conversation, as the test holds them.
struct Ends {
    /// The channel of the one who called in, and their telephone.
    caller: String,
    phone: Phone,
    /// The network's end of the call to the second participant, the channel
    /// of their telephone, and the telephone.
    network_end: String,
    telephone: String,
    second_phone: Phone,
}

/// The rest of [`the_application_connects_participants`]: whoever is left
/// alone in a group — the other was removed, or hung up — is connected to
/// nobody again, and the application still reaches them.
async fn those_left_alone_are_reached(
    stand: &Stand,
    owner: &mut Owner,
    ends: &mut Ends,
    [first, second, third]: [&ParticipantId; 3],
) -> TestResult {
    let Ends {
        caller,
        phone,
        network_end,
        telephone,
        second_phone,
    } = ends;
    let both = vec![first.clone(), second.clone()];
    // The third is removed: the second is left alone in the group, which is
    // then no group, and the application reaches them as before.
    let participant = third.clone();
    let mut events = accepted(owner, 19, Command::Remove { participant }).await?;
    if events.is_empty() {
        events.push(owner.next_event().await?);
    }
    assert!(
        matches!(events.as_slice(), [Event::ParticipantLeft { participant, departure: Departure::Removed }] if participant == third),
        "after the third was removed came {events:?}"
    );
    stand
        .expect_a_group_of(0, "after one of two connected was removed")
        .await?;
    begins_to_play(owner, 20, second).await?;
    second_phone.hears(1000, true).await?;
    is_delivered(owner).await?;

    // Connected once more, and now the second hangs up.
    accepted(owner, 21, Command::Connect { participants: both }).await?;
    phone.hears(SECOND_SAYS, true).await?;
    for gone in [
        format!("channels/{network_end}"),
        format!("channels/{telephone}"),
        "bridges/the-second-phone".to_owned(),
    ] {
        stand.network.control.request("DELETE", &gone).await?;
    }
    let left = owner.next_event().await?;
    assert!(
        matches!(&left, Event::ParticipantLeft { participant, departure: Departure::HungUp } if participant == second),
        "after the second hung up came {left:?}"
    );
    stand
        .expect_a_group_of(0, "after one of two connected hung up")
        .await?;
    begins_to_play(owner, 22, first).await?;
    phone.hears(1000, true).await?;
    is_delivered(owner).await?;
    one_hangs_up_while_being_connected(stand, owner, first, phone).await?;

    stand
        .control
        .request("DELETE", &format!("channels/{caller}"))
        .await?;
    let mut events = vec![owner.next_event().await?, owner.next_event().await?];
    assert!(
        matches!(
            events.as_mut_slice(),
            [
                Event::ParticipantLeft {
                    departure: Departure::HungUp,
                    ..
                },
                Event::ConversationEnded {
                    reason: EndReason::LastParticipantLeft
                },
            ]
        ),
        "after the caller hung up came {events:?}"
    );
    owner.closed().await?;
    stand
        .expect_no_channels("after a connected conversation")
        .await?;
    stand
        .expect_no_bridges("after a connected conversation")
        .await?;
    stand
        .network
        .control
        .expect_none("channels", "after a connected conversation")
        .await
}

/// A participant whose channel ends at the moment they are being connected
/// takes nobody down with them.
///
/// The test ends the channel in the same instant as it sends the command.
/// Which of the two the controller meets first is not the test's to choose,
/// so it holds to what must be true either way: the one who is gone is
/// reported as having hung up, no group is left behind, and the other is
/// still reached. It does not show that Asterisk refused anything: seventy
/// such attempts on the pinned build never caught it between the two.
async fn one_hangs_up_while_being_connected(
    stand: &Stand,
    owner: &mut Owner,
    first: &ParticipantId,
    phone: &mut Phone,
) -> TestResult {
    let (outcome, mut events) = dial(owner, 30, ANSWERS, "wide", 5000).await?;
    let CommandOutcome::AcceptedParticipant {
        participant: leaving,
    } = outcome
    else {
        return Err(format!("dialling one more participant was {outcome:?}").into());
    };
    while events.len() < 2 {
        events.push(owner.next_event().await?);
    }
    // The node's own end of the call it placed.
    let node_end = read_asterisk(&stand.control.request("GET", "channels").await?.1)?
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|channel| channel.get("id").and_then(serde_json::Value::as_str))
        .find(|id| id.contains(".participant-"))
        .ok_or("the node has no channel of the call it placed")?
        .to_owned();
    // With an audio path of their own there is more to arrange for them.
    let participant = leaving.clone();
    accepted(owner, 31, Command::Listen { participant }).await?;

    let participants = vec![first.clone(), leaving.clone()];
    let gone = format!("channels/{node_end}");
    // The command is on its way the moment it is put out; the hang-up
    // follows at once.
    owner.command(32, Command::Connect { participants })?;
    stand.control.request("DELETE", &gone).await?;
    let (outcome, mut events) = owner.outcome_of(32).await?;
    assert!(
        matches!(
            outcome,
            CommandOutcome::Accepted
                | CommandOutcome::Rejected {
                    reason: CommandRejection::UnknownParticipant
                }
        ),
        "connecting a participant who was hanging up was {outcome:?}"
    );
    let left = loop {
        let found = events.iter().find(|event| {
            matches!(event, Event::ParticipantLeft { participant, .. } if *participant == leaving)
        });
        if let Some(left) = found {
            break left.clone();
        }
        events.push(owner.next_event().await?);
    };
    assert!(
        matches!(
            left,
            Event::ParticipantLeft {
                departure: Departure::HungUp,
                ..
            }
        ),
        "a participant who hung up while being connected left as {left:?}"
    );
    stand
        .expect_a_group_of(0, "after one hung up while being connected")
        .await?;
    begins_to_play(owner, 33, first).await?;
    phone.hears(1000, true).await?;
    is_delivered(owner).await
}

/// Asterisk refuses a step about a participant who stays: the conversation
/// ends with an error, and nobody is left on the line.
///
/// The refusal is the test's doing. It removes the bridge of a participant's
/// audio path behind the controller's back; connecting the participant then
/// needs that bridge, and Asterisk says it has none. The controller asks
/// about the participant's channel, finds it still in the application, and
/// so knows the step failed — it was not a participant on their way out.
async fn a_step_refused_for_one_who_stays(stand: &Stand) -> TestResult {
    let (_caller, _phone, mut owner) = stand.call_from_a_phone().await?;
    let first = hello(&mut owner).await?.first.id;
    owner.send(&ApplicationMessage::Accept)?;
    let participant = first.clone();
    accepted(&mut owner, 1, Command::Listen { participant }).await?;

    // The path is built a moment after the command is answered.
    let path = timeout(STEP, async {
        loop {
            let bridges = read_asterisk(&stand.control.request("GET", "bridges").await?.1)?;
            let built = bridges.as_array().into_iter().flatten().find_map(|bridge| {
                let id = bridge.get("id").and_then(serde_json::Value::as_str)?;
                let channels = bridge
                    .get("channels")
                    .and_then(serde_json::Value::as_array)?;
                (id.contains(".bridge-") && channels.len() == 2).then(|| id.to_owned())
            });
            if let Some(built) = built {
                return Ok::<_, Box<dyn Error>>(built);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| "the audio path of a participant was not built")??;
    stand
        .control
        .request("DELETE", &format!("bridges/{path}"))
        .await?;

    let (outcome, mut events) = dial(&mut owner, 2, ANSWERS, "wide", 5000).await?;
    let CommandOutcome::AcceptedParticipant {
        participant: second,
    } = outcome
    else {
        return Err(format!("dialling a second participant was {outcome:?}").into());
    };
    while events.len() < 2 {
        events.push(owner.next_event().await?);
    }
    let refused = format!(
        "when asked to join the media channel and the participant for {}",
        first.as_str()
    );
    let participants = vec![first, second];
    accepted(&mut owner, 3, Command::Connect { participants }).await?;
    owner.closed().await?;
    if !stand.controller.comes_to_say(&refused).await {
        return Err("the controller did not say which step Asterisk refused".into());
    }
    stand.expect_no_channels("after a refused step").await?;
    stand.expect_no_bridges("after a refused step").await?;
    stand
        .network
        .control
        .expect_none("channels", "after a refused step")
        .await
}

/// Read the hello of a conversation that is handed over as it is: the
/// number it came through, who is in it and who is connected to whom.
async fn handed_over(
    owner: &mut Owner,
) -> TestResult<(String, Vec<ParticipantSnapshot>, Vec<Vec<ParticipantId>>)> {
    let ControllerMessage::Hello {
        opening:
            Opening::Resumed {
                origin: Origin::DialedNumber { dialed },
                participants,
                connected,
                recordings,
            },
        ..
    } = owner.next().await?
    else {
        return Err("the conversation was not handed over as one that goes on".into());
    };
    assert!(recordings.is_empty(), "recordings nobody started");
    Ok((dialed.as_str().to_owned(), participants, connected))
}

/// End a conversation of `participants` people by the handler's word, and
/// see it ended and the connection closed.
async fn ends_it(owner: &mut Owner, id: u64, participants: usize) -> TestResult {
    let mut events = accepted(owner, id, Command::End).await?;
    while events.len() <= participants {
        events.push(owner.next_event().await?);
    }
    let (ended, left) = events.split_last().ok_or("no events")?;
    assert!(
        matches!(
            ended,
            Event::ConversationEnded {
                reason: EndReason::EndedByHandler
            }
        ) && left.iter().all(|event| matches!(
            event,
            Event::ParticipantLeft {
                departure: Departure::Removed,
                ..
            }
        )),
        "after `end` came {events:?}"
    );
    owner.closed().await
}

/// The instance that owns a conversation is lost while two people are
/// connected. They go on talking, and an instance that is there is handed
/// the conversation as it is — the two of them, connected — and controls it.
/// What the lost instance had asked for is not handed over with it.
async fn connected_people_outlive_their_owner(stand: &Stand) -> TestResult {
    let (_caller, mut phone, mut owner) = stand.call_from_a_phone().await?;
    let first = hello(&mut owner).await?.first.id;
    owner.send(&ApplicationMessage::Accept)?;
    let (second, _network_end, telephone, mut second_phone) =
        a_second_participant(stand, &mut owner).await?;
    let participant = first.clone();
    accepted(&mut owner, 11, Command::Listen { participant }).await?;
    let participants = vec![first.clone(), second.clone()];
    accepted(&mut owner, 12, Command::Connect { participants }).await?;
    phone.hears(SECOND_SAYS, true).await?;
    // How long the second is held is theirs, not the instance's.
    let participant = second.clone();
    accepted(
        &mut owner,
        13,
        Command::HoldFor {
            participant,
            limit_ms: SECOND_HOLD_LIMIT_MS,
        },
    )
    .await?;

    // The instance dies: its connection is gone with nothing said.
    drop(owner);
    let mut owner = Owner::accept(&stand.application).await?;
    let (dialed, participants, connected) = handed_over(&mut owner).await?;
    assert_eq!(dialed, DIALED);
    let mut there: Vec<(&str, &ParticipantState, Option<&str>, u64)> = participants
        .iter()
        .map(|snapshot| {
            (
                snapshot.participant.id.as_str(),
                &snapshot.state,
                snapshot
                    .participant
                    .number
                    .as_ref()
                    .map(|number| number.as_str()),
                snapshot.hold_limit_ms,
            )
        })
        .collect();
    there.sort_unstable_by_key(|(id, ..)| *id);
    assert!(
        matches!(
            there.as_slice(),
            [
                (one, ParticipantState::InConversation, None, 0),
                (other, ParticipantState::InConversation, Some(ANSWERS), SECOND_HOLD_LIMIT_MS),
            ] if *one == first.as_str() && *other == second.as_str()
        ),
        "the conversation was handed over with {participants:?}"
    );
    let mut together: Vec<Vec<&str>> = connected
        .iter()
        .map(|group| {
            group
                .iter()
                .map(|participant| participant.as_str())
                .collect()
        })
        .collect();
    for group in &mut together {
        group.sort_unstable();
    }
    assert_eq!(together, [[first.as_str(), second.as_str()]]);
    // Nobody had it for a while, and the two heard each other all along.
    phone.hears(SECOND_SAYS, true).await?;
    second_phone.hears(Phone::SAYS, true).await?;

    owner.send(&ApplicationMessage::Accept)?;
    // It is sent nothing it did not ask for itself, and then all it asks for.
    assert!(
        owner.heard.is_empty(),
        "audio the lost instance had asked for"
    );
    for (id, participant) in [(1, &first), (2, &second)] {
        let participant = participant.clone();
        accepted(&mut owner, id, Command::Listen { participant }).await?;
    }
    hears_each_by_themselves(&mut owner, &first, &second).await?;
    begins_to_play(&mut owner, 3, &first).await?;
    phone.hears(1000, true).await?;
    is_delivered(&mut owner).await?;
    ends_it(&mut owner, 4, 2).await?;

    for gone in [
        format!("channels/{telephone}"),
        "bridges/the-second-phone".to_owned(),
    ] {
        stand.network.control.request("DELETE", &gone).await?;
    }
    stand
        .expect_no_channels("after a conversation changed hands")
        .await?;
    stand
        .expect_no_bridges("after a conversation changed hands")
        .await?;
    stand
        .network
        .control
        .expect_none("channels", "after a conversation changed hands")
        .await
}

/// The instance that owns a conversation is lost while the caller is alone
/// with it. The one instance that is there will not take the conversation —
/// it is leaving — and the fallback of the entry is a message: the caller
/// hears it and is hung up, not left on a line nobody controls.
async fn a_caller_alone_loses_the_application(stand: &Stand) -> TestResult {
    let (_caller, mut owner) = stand.call().await?;
    let first = hello(&mut owner).await?.first.id;
    owner.send(&ApplicationMessage::Accept)?;
    let participant = first.clone();
    let mut events = accepted(&mut owner, 1, Command::Answer { participant }).await?;
    if events.is_empty() {
        events.push(owner.next_event().await?);
    }

    drop(owner);
    let mut owner = Owner::accept(&stand.application).await?;
    let (_, participants, connected) = handed_over(&mut owner).await?;
    assert!(
        matches!(
            participants.as_slice(),
            [ParticipantSnapshot { participant, state: ParticipantState::InConversation, .. }]
                if participant.id == first
        ) && connected.is_empty(),
        "a caller alone was offered as {participants:?}, connected {connected:?}"
    );
    owner.send(&ApplicationMessage::Decline {
        reason: DeclineReason::Draining,
    })?;
    owner.closed().await?;
    stand
        .reported("a caller alone who lost the application", true, is_message)
        .await?;
    stand
        .expect_no_channels("after a caller alone lost the application")
        .await
}

/// What a hello says about a new conversation.
struct ControlMessageHello {
    conversation: String,
    dialed: String,
    first: node_protocol::messages::Participant,
}

/// Read the hello of a conversation that has just been opened.
async fn hello(owner: &mut Owner) -> TestResult<ControlMessageHello> {
    let ControllerMessage::Hello {
        conversation,
        opening: Opening::Started { origin, first },
        ..
    } = owner.next().await?
    else {
        return Err("the conversation did not open with a hello of a new one".into());
    };
    let Origin::DialedNumber { dialed } = origin else {
        return Err("the call did not arrive as a dialed number".into());
    };
    Ok(ControlMessageHello {
        conversation: conversation.as_str().to_owned(),
        dialed: dialed.as_str().to_owned(),
        first,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn calls_are_carried_from_their_first_ring_to_their_end() -> TestResult {
    let mut stand = Stand::start().await?;
    let mut outcome = every_scenario(&stand).await;
    if outcome.is_ok() {
        outcome = the_node_without_the_application(&mut stand).await;
    }
    if outcome.is_ok() {
        outcome = a_fallback_transfer_finds_the_number_busy(&mut stand).await;
    }
    if outcome.is_ok() {
        // Late, because here the controller and Asterisk do lose each other.
        outcome = the_node_without_its_controller(&mut stand).await;
    }
    if outcome.is_ok() {
        outcome = the_telephony_server_restarts_under_a_conversation(&mut stand).await;
    }
    if outcome.is_ok() {
        // Last, because the controller stops.
        outcome = a_second_controller_takes_the_application(&mut stand).await;
    }
    if outcome.is_err() {
        stand.report().await;
    }
    outcome
}

/// The node's Asterisk dies under a conversation and is started again.
/// The owner is told the telephony server is lost, and nothing is carried
/// out meanwhile; when it is back, the participant whose call ended with it
/// has left — how is not known — before the word that it is back, and the
/// conversation ends.
async fn the_telephony_server_restarts_under_a_conversation(stand: &mut Stand) -> TestResult {
    let (_caller, _phone, mut owner) = stand.call_from_a_phone().await?;
    let first = hello(&mut owner).await?.first.id;
    owner.send(&ApplicationMessage::Accept)?;

    stand.kill_asterisk().await?;
    let lost = owner.next_event().await?;
    assert!(
        matches!(lost, Event::TelephonyServerLost),
        "the owner was told {lost:?} when Asterisk died"
    );
    owner.command(
        1,
        Command::StopListening {
            participant: first.clone(),
        },
    )?;
    let (outcome, _) = owner.outcome_of(1).await?;
    assert!(
        matches!(
            outcome,
            CommandOutcome::Rejected {
                reason: CommandRejection::TelephonyServerUnavailable
            }
        ),
        "a command without Asterisk was {outcome:?}"
    );

    stand.start_asterisk_again().await?;
    let mut told = Vec::new();
    for _ in 0..3 {
        told.push(owner.next_event().await?);
    }
    assert!(
        matches!(
            told.as_slice(),
            [
                Event::ParticipantLeft { participant, departure: Departure::Lost },
                Event::TelephonyServerBack,
                Event::ConversationEnded { reason: EndReason::LastParticipantLeft },
            ] if *participant == first
        ),
        "after Asterisk was back the owner was told {told:?}"
    );
    owner.closed().await?;
    stand
        .expect_no_channels("after Asterisk was started again")
        .await
}

/// A call from the operator through its side whose audio stops when its
/// telephone does, answered, with a telephone at the caller's end of the
/// network. Returns the caller's end, their participant, the telephone's
/// channel, the telephone and the owner.
async fn an_answered_call_from_the_operator(
    stand: &Stand,
) -> TestResult<(String, ParticipantId, String, Phone, Owner)> {
    let caller = stand
        .network
        .control
        .create_channel(&format!(
            "endpoint=PJSIP/{DIALED_IN_URL}@the-node-quiet&app={FAR_END}&callerId={CALLER_IN_URL}"
        ))
        .await?;
    let mut owner = Owner::accept(&stand.application).await?;
    let first = hello(&mut owner).await?.first.id;
    owner.send(&ApplicationMessage::Accept)?;
    let participant = first.clone();
    let mut events = accepted(&mut owner, 1, Command::Answer { participant }).await?;
    if events.is_empty() {
        events.push(owner.next_event().await?);
    }
    let (telephone, phone) = stand.network.give_a_phone(&caller).await?;
    Ok((caller, first, telephone, phone, owner))
}

/// The network lets go of a caller's telephone and the bridge that joined
/// it to the caller's end — once that end is gone, or the telephone would
/// go on saying its tone to whoever is joined to it next.
async fn the_telephone_is_put_down(stand: &Stand, telephone: &str) -> TestResult {
    for gone in [
        format!("channels/{telephone}"),
        "bridges/the-callers-phone".to_owned(),
    ] {
        stand.network.control.request("DELETE", &gone).await?;
    }
    Ok(())
}

/// The next event, or none within `wait`.
async fn event_within(owner: &mut Owner, wait: Duration) -> TestResult<Option<Event>> {
    match timeout(wait, owner.next_event()).await {
        Ok(event) => Ok(Some(event?)),
        Err(_) => Ok(None),
    }
}

/// Audio that stops arriving while the call goes on. While the far end has
/// the call on hold nothing is judged; then five seconds without audio is
/// audio lost: the participant is held for as long as `hold_for` allows and
/// returns when audio comes back — and with no limit, the default, they are
/// gone at once, as one who did not return.
async fn audio_that_stops_arriving(stand: &Stand) -> TestResult {
    let (caller, first, telephone, phone, mut owner) =
        an_answered_call_from_the_operator(stand).await?;
    let participant = first.clone();
    accepted(
        &mut owner,
        2,
        Command::HoldFor {
            participant,
            limit_ms: 20_000,
        },
    )
    .await?;

    // The caller's side puts the call on hold and sends nothing.
    stand
        .network
        .control
        .request("POST", &format!("channels/{caller}/hold"))
        .await?;
    phone.speaks(false);
    let told = event_within(&mut owner, Duration::from_secs(7)).await?;
    assert!(told.is_none(), "audio on hold was judged: {told:?}");

    // Off hold, still silent: lost, held — then back.
    stand
        .network
        .control
        .request("DELETE", &format!("channels/{caller}/hold"))
        .await?;
    let mut told = Vec::new();
    for _ in 0..2 {
        told.push(owner.next_event().await?);
    }
    assert!(
        matches!(
            told.as_slice(),
            [Event::MediumLost { participant: lost }, Event::ParticipantHeld { participant: held }]
                if *lost == first && *held == first
        ),
        "silence off hold was told as {told:?}"
    );
    phone.speaks(true);
    let back = owner.next_event().await?;
    assert!(
        matches!(&back, Event::ParticipantReturned { participant } if *participant == first),
        "audio that came back was told as {back:?}"
    );
    ends_it(&mut owner, 3, 1).await?;
    the_telephone_is_put_down(stand, &telephone).await?;

    // With no limit, lost is gone.
    let (_caller, first, telephone, phone, mut owner) =
        an_answered_call_from_the_operator(stand).await?;
    phone.speaks(false);
    let mut told = Vec::new();
    for _ in 0..4 {
        told.push(owner.next_event().await?);
    }
    assert!(
        matches!(
            told.as_slice(),
            [
                Event::MediumLost { .. },
                Event::ParticipantHeld { .. },
                Event::ParticipantLeft { participant, departure: Departure::DidNotReturn },
                Event::ConversationEnded { reason: EndReason::LastParticipantLeft },
            ] if *participant == first
        ),
        "audio lost with no limit was told as {told:?}"
    );
    owner.closed().await?;
    the_telephone_is_put_down(stand, &telephone).await?;
    stand
        .expect_no_channels("after audio stopped arriving")
        .await?;
    stand
        .network
        .control
        .expect_none("channels", "after audio stopped arriving")
        .await
}

/// Asterisk does not begin a call the application asks for — here because
/// the name the node gives the call's channel is already taken — and the
/// `dial` command is refused as `call_not_placed`; the line's place is given
/// back, so the next call on it is placed.
async fn a_call_asterisk_does_not_place(stand: &Stand) -> TestResult {
    let (_caller, _phone, mut owner) = stand.call_from_a_phone().await?;
    let opened = hello(&mut owner).await?;
    owner.send(&ApplicationMessage::Accept)?;
    // The caller is participant "p-<run>-1"; the call the node places next
    // is participant 2, its channel "<conversation>.<run>.participant-2".
    let run = opened
        .first
        .id
        .as_str()
        .strip_prefix("p-")
        .and_then(|rest| rest.rsplit_once('-'))
        .map(|(run, _)| run.to_owned())
        .ok_or("a participant identifier without the controller's run")?;
    let taken = format!("{}.{run}.participant-2", opened.conversation);
    let blocker = stand
        .control
        .create_channel(&format!(
            "endpoint=WebSocket/INCOMING/c(ulaw)&app={FAR_END}&channelId={taken}"
        ))
        .await?;
    let (outcome, _) = dial(&mut owner, 1, ANSWERS, "main", 5000).await?;
    assert!(
        matches!(
            outcome,
            CommandOutcome::Rejected {
                reason: CommandRejection::CallNotPlaced
            }
        ),
        "a call Asterisk would not begin was {outcome:?}"
    );
    stand
        .control
        .request("DELETE", &format!("channels/{blocker}"))
        .await?;
    let (outcome, mut events) = dial(&mut owner, 2, ANSWERS, "main", 5000).await?;
    assert!(
        matches!(outcome, CommandOutcome::AcceptedParticipant { .. }),
        "the next call on the line was {outcome:?}"
    );
    while events.len() < 2 {
        events.push(owner.next_event().await?);
    }
    ends_it(&mut owner, 3, 2).await?;
    stand
        .expect_no_channels("after a call Asterisk did not place")
        .await
}

/// Two groups become one when the application connects someone of each:
/// the caller and three the node called, two by two, then all four.
async fn two_groups_are_merged(stand: &Stand) -> TestResult {
    let (_caller, _phone, mut owner) = stand.call_from_a_phone().await?;
    let first = hello(&mut owner).await?.first.id;
    owner.send(&ApplicationMessage::Accept)?;
    let mut called = Vec::new();
    for (id, line) in [(1, "main"), (2, "wide"), (3, "wide")] {
        let (outcome, mut events) = dial(&mut owner, id, ANSWERS, line, 5000).await?;
        let CommandOutcome::AcceptedParticipant { participant } = outcome else {
            return Err(format!("dialling on {line} was {outcome:?}").into());
        };
        while events.len() < 2 {
            events.push(owner.next_event().await?);
        }
        called.push(participant);
    }
    let [a, b, c] = called.as_slice() else {
        return Err("three were not called".into());
    };
    let connect = |participants: &[&ParticipantId]| Command::Connect {
        participants: participants.iter().map(|&who| who.clone()).collect(),
    };
    accepted(&mut owner, 4, connect(&[&first, a])).await?;
    accepted(&mut owner, 5, connect(&[b, c])).await?;
    stand
        .expect_groups(&[2, 2], "after two pairs were connected")
        .await?;
    accepted(&mut owner, 6, connect(&[a, b])).await?;
    stand
        .expect_groups(&[4], "after one of each pair was connected")
        .await?;
    ends_it(&mut owner, 7, 4).await?;
    stand
        .expect_no_channels("after two groups were merged")
        .await
}

/// Tone digits travel both ways through the trunk: what the caller presses
/// reaches the application, and what the application sends reaches the
/// caller's network.
async fn digits_go_both_ways(
    stand: &Stand,
    owner: &mut Owner,
    caller: &str,
    participant: &ParticipantId,
    id: u64,
) -> TestResult {
    stand
        .network
        .control
        .request("POST", &format!("channels/{caller}/dtmf?dtmf=5"))
        .await?;
    let pressed = timeout(STEP, async {
        loop {
            if let Event::DigitReceived {
                participant: who,
                digit,
            } = owner.next_event().await?
            {
                return Ok::<_, Box<dyn Error>>((who, digit));
            }
        }
    })
    .await
    .map_err(|_| "the application was not told of the digit the caller pressed")??;
    assert!(
        pressed.0 == *participant && pressed.1.as_str() == "5",
        "the caller pressed 5 and the application was told {pressed:?}"
    );
    // The network's Asterisk reports a digit that arrives back to back with
    // the one before twice while its channel is in a bridge — "122##" for
    // "12#" — and once otherwise; the node sends them once either way
    // (digits sent one at a time arrive once in the bridge too). So the
    // caller's end listens outside its bridge, and goes back after.
    let network = &stand.network.control;
    network
        .request(
            "POST",
            &format!("bridges/the-callers-phone/removeChannel?channel={caller}"),
        )
        .await?;
    owner.command(
        id,
        Command::SendDigits {
            participant: participant.clone(),
            digits: "12#".parse()?,
        },
    )?;
    let (outcome, _) = owner.outcome_of(id).await?;
    assert!(
        matches!(outcome, CommandOutcome::Accepted),
        "`send_digits` was {outcome:?}"
    );
    let heard = network.digits_heard(caller, 3).await?;
    assert_eq!(heard, "12#", "the caller's network heard other digits");
    network
        .request(
            "POST",
            &format!("bridges/the-callers-phone/addChannel?channel={caller}"),
        )
        .await?;
    Ok(())
}

/// A media connection that does not present the control secret Asterisk
/// has from its configuration is refused: the loopback address keeps the
/// network out, and this every other process of the machine.
async fn only_asterisk_opens_media_connections(stand: &Stand) -> TestResult {
    let mut request =
        format!("ws://{}/media", stand.controller_address()?).into_client_request()?;
    request
        .headers_mut()
        .insert(SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("media"));
    match connect_async(request).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "a media connection without the secret was answered {}",
                response.status()
            );
            Ok(())
        }
        Err(error) => {
            Err(format!("a media connection without the secret failed otherwise: {error}").into())
        }
        Ok(_) => Err("a media connection without the secret was let in".into()),
    }
}

/// A fallback's transfer that cannot connect the caller — the number it
/// calls is busy — is reported failed, with the reason; the caller, with
/// nobody to be connected to, is hung up and reported so.
async fn a_fallback_transfer_finds_the_number_busy(stand: &mut Stand) -> TestResult {
    stand.application.go_away().await;
    stand
        .control
        .create_channel(&format!(
            "endpoint=Local/{}@gabion-from-network&app={FAR_END}",
            DIALED_TO_BUSY.replace('+', "%2B")
        ))
        .await?;
    stand
        .reported("a fallback's transfer to a busy number", true, |report| {
            matches!(
                report,
                Report::FallbackFailed {
                    fallback: FallbackKind::Transfer,
                    failure: FallbackFailure::NotConnected {
                        departure: Departure::Busy
                    },
                    ..
                }
            )
        })
        .await?;
    stand
        .reported("the caller of a transfer that failed", true, is_hang_up)
        .await?;
    stand
        .expect_no_channels("after a fallback's transfer failed")
        .await?;
    stand.application.come_back().await
}

/// A second connection takes the node's application in Asterisk, as a
/// second controller started next to the same Asterisk would. The
/// controller does not take it back — that would only begin a tug of war —
/// and does not stay on hearing nothing: it says so and stops with a
/// failure, for whoever runs the node to see.
async fn a_second_controller_takes_the_application(stand: &mut Stand) -> TestResult {
    let _second = stand.control.events_of(NODE_APPLICATION).await?;
    if !stand.controller.comes_to_say("TAKEN").await {
        return Err("the controller did not notice its application was taken".into());
    }
    let status = timeout(STEP, stand.controller.child.wait())
        .await
        .map_err(|_| "the controller whose application was taken did not stop")??;
    assert_eq!(
        status.code(),
        Some(1),
        "the controller whose application was taken stopped with {status}"
    );
    Ok(())
}

/// The network's end of the call the node has placed to the number that
/// answers — waited for, since the call takes a moment to arrive.
async fn the_call_the_node_placed(stand: &Stand) -> TestResult<serde_json::Value> {
    let network = &stand.network.control;
    timeout(STEP, async {
        loop {
            let channels = read_asterisk(&network.request("GET", "channels").await?.1)?;
            let found = channels.as_array().into_iter().flatten().find(|channel| {
                channel.pointer("/dialplan/exten") == Some(&serde_json::json!(ANSWERS))
            });
            if let Some(found) = found {
                return Ok::<_, Box<dyn Error>>(found.clone());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| "the node did not call the number of the fallback")?
}

/// The node without the application, and with it again.
///
/// No instance is there when a call arrives. The controller carries out the
/// fallback of the entry by itself: it calls the fallback's number on its
/// line, showing the line's number, and connects the caller to whoever
/// answers. The two are then people connected to each other: they are kept,
/// and the application is looked for while they talk. When it is there
/// again it is handed the conversation as it is. A caller who still rings
/// is answered only when the one called for them answers; and when one of
/// two connected by a fallback leaves, the other is not kept on the line.
async fn the_node_without_the_application(stand: &mut Stand) -> TestResult {
    stand.application.go_away().await;

    let (_caller, mut phone) = stand.a_phone_calls(DIALED_WITH_TRANSFER).await?;
    let network_end = the_call_the_node_placed(stand).await?;
    assert_eq!(
        network_end.pointer("/caller/number"),
        Some(&serde_json::json!(DIALED)),
        "the fallback's transfer did not show the line's number"
    );
    let network_end = network_end
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or("a channel without an identifier")?
        .to_owned();
    let (telephone, mut second_phone) = stand
        .network
        .give_a_phone_that_says(&network_end, SECOND_SAYS, "the-second-phone")
        .await?;
    phone.hears(SECOND_SAYS, true).await?;
    second_phone.hears(Phone::SAYS, true).await?;
    the_fallback_is_reported(stand).await?;

    // The application is there again.
    stand.application.come_back().await?;
    let mut owner = Owner::accept(&stand.application).await?;
    let (dialed, participants, connected) = handed_over(&mut owner).await?;
    assert_eq!(dialed, DIALED_WITH_TRANSFER);
    assert!(
        participants.len() == 2
            && participants
                .iter()
                .all(|snapshot| snapshot.state == ParticipantState::InConversation)
            && matches!(connected.as_slice(), [group] if group.len() == 2),
        "two connected by a fallback were handed over as {participants:?}, connected {connected:?}"
    );
    owner.send(&ApplicationMessage::Accept)?;
    ends_it(&mut owner, 1, 2).await?;
    for gone in [
        format!("channels/{telephone}"),
        "bridges/the-second-phone".to_owned(),
    ] {
        stand.network.control.request("DELETE", &gone).await?;
    }
    stand
        .expect_no_channels("after a fallback was handed over")
        .await?;
    stand
        .network
        .control
        .expect_none("channels", "after a fallback was handed over")
        .await?;

    // The application is away again, and this caller still rings.
    stand.application.go_away().await;
    let caller = stand
        .control
        .create_channel(&format!(
            "endpoint=Local/{}@gabion-from-network&app={FAR_END}",
            DIALED_WITH_TRANSFER.replace('+', "%2B")
        ))
        .await?;
    the_call_the_node_placed(stand).await?;
    stand
        .expect_a_group_of(2, "after a ringing caller was transferred by a fallback")
        .await?;
    the_fallback_is_reported(stand).await?;
    // The caller hangs up: the one who was called for them is not kept.
    stand
        .control
        .request("DELETE", &format!("channels/{caller}"))
        .await?;
    // Left unacknowledged: the controller started again later sends it again.
    let kept = stand
        .reported("the one called for a caller who left", false, is_hang_up)
        .await?;
    *stand.unacknowledged.lock().await = Some(kept);
    stand
        .expect_no_channels("after one of two connected by a fallback left")
        .await?;
    stand
        .expect_no_bridges("after one of two connected by a fallback left")
        .await?;
    stand
        .network
        .control
        .expect_none("channels", "after one of two connected by a fallback left")
        .await?;

    stand.application.come_back().await
}

/// The node carried out a fallback's transfer by itself, and keeps the two
/// connected while it looks for the application: both are reported.
async fn the_fallback_is_reported(stand: &Stand) -> TestResult {
    stand
        .reported("a fallback's transfer", true, |report| {
            matches!(report, Report::FallbackApplied {
                fallback: node_protocol::messages::FallbackKind::Transfer,
                origin: Origin::DialedNumber { dialed },
                ..
            } if dialed.as_str() == DIALED_WITH_TRANSFER)
        })
        .await?;
    stand
        .reported("two kept connected by a fallback", true, |report| {
            matches!(report, Report::HoldingWithoutOwner { .. })
        })
        .await?;
    Ok(())
}

/// The node without its controller, and with it again.
///
/// The controller dies with a conversation in progress: two people
/// connected, each with an audio path. Asterisk goes on running, and to it
/// the node's application no longer exists: calls that arrive wait a few
/// seconds for the controller and are then given the fallback of their
/// entry by Asterisk alone — a transfer through the operator, or a hang-up.
/// A call that is still waiting when a controller is started is served. The
/// conversation from before is found by the new controller in Asterisk —
/// its two people, connected, hearing each other all along — and handed to
/// the application as it is; what the old controller had built for audio is
/// removed.
async fn the_node_without_its_controller(stand: &mut Stand) -> TestResult {
    let (_caller, mut phone, mut owner) = stand.call_from_a_phone().await?;
    let first = hello(&mut owner).await?.first.id;
    owner.send(&ApplicationMessage::Accept)?;
    let (second, network_end, telephone, mut second_phone) =
        a_second_participant(stand, &mut owner).await?;
    for (id, participant) in [(1, &first), (2, &second)] {
        let participant = participant.clone();
        accepted(&mut owner, id, Command::Listen { participant }).await?;
    }
    let participants = vec![first.clone(), second.clone()];
    accepted(&mut owner, 3, Command::Connect { participants }).await?;
    phone.hears(SECOND_SAYS, true).await?;
    // Kept on the second's channel, for the controller that comes next.
    let participant = second.clone();
    accepted(
        &mut owner,
        4,
        Command::HoldFor {
            participant,
            limit_ms: SECOND_HOLD_LIMIT_MS,
        },
    )
    .await?;

    let named_before = audio_path_names(stand).await?;
    stand.kill_the_controller().await?;
    owner.closed().await?;

    let transferred = asterisk_alone_gives_fallbacks(stand, &network_end).await?;

    // The conversation in progress is still there: Asterisk dropped nobody,
    // and the two go on hearing each other.
    let (_, still) = stand.control.request("GET", "channels").await?;
    assert!(
        still.contains("Stasis"),
        "the conversation in progress did not outlive the controller: {still}"
    );
    phone.hears(SECOND_SAYS, true).await?;

    // A call that arrives a moment before a controller is started waits
    // for it and is served; the conversation from before is carried on.
    // Either may reach the application first.
    let network = &stand.network.control;
    let waited = network.create_channel(&from_the_operator(DIALED)).await?;
    stand.start_the_controller().await?;
    let network = &stand.network.control;
    let (mut carried_on, found, mut new) = both_conversations_offered(stand).await?;
    let mut limits: Vec<(&str, u64)> = found
        .iter()
        .map(|snapshot| (snapshot.participant.id.as_str(), snapshot.hold_limit_ms))
        .collect();
    limits.sort_unstable();
    assert_eq!(
        limits,
        [(first.as_str(), 0), (second.as_str(), SECOND_HOLD_LIMIT_MS)],
        "the new controller did not find the hold limits the one before had"
    );
    new.send(&ApplicationMessage::Accept)?;
    accepted(&mut new, 1, Command::End).await?;
    network.reason_it_ended(&waited).await?;

    // The two from before are controlled by their new owner, which is sent
    // nothing the old controller's owner had asked for.
    carried_on.send(&ApplicationMessage::Accept)?;
    assert!(
        carried_on.heard.is_empty(),
        "audio nobody asked this owner's node for"
    );
    second_phone.hears(Phone::SAYS, true).await?;
    for (id, participant) in [(1, &first), (2, &second)] {
        let participant = participant.clone();
        accepted(&mut carried_on, id, Command::Listen { participant }).await?;
    }
    hears_each_by_themselves(&mut carried_on, &first, &second).await?;
    // The audio paths are built anew, under names the controller before
    // never gave: Asterisk keeps a removed bridge's name taken for as long
    // as anything still holds the bridge.
    let named_after = audio_path_names(stand).await?;
    assert!(
        !named_after.is_empty() && named_after.iter().all(|name| !named_before.contains(name)),
        "the new controller gave a name of the one before: before {named_before:?}, after {named_after:?}"
    );
    begins_to_play(&mut carried_on, 3, &first).await?;
    phone.hears(1000, true).await?;
    is_delivered(&mut carried_on).await?;
    ends_it(&mut carried_on, 4, 2).await?;

    if !stand
        .controller
        .said("left from before this connection")
        .await
    {
        return Err("the new controller did not remove what the old one had built".into());
    }
    the_new_controller_carries_on_the_service(stand).await?;
    for gone in [
        format!("channels/{transferred}"),
        format!("channels/{telephone}"),
        "bridges/the-second-phone".to_owned(),
    ] {
        network.request("DELETE", &gone).await?;
    }
    network
        .expect_none("channels", "after the node was without its controller")
        .await?;
    stand
        .expect_no_channels("after the node was without its controller")
        .await?;
    stand
        .expect_no_bridges("after the node was without its controller")
        .await
}

/// The names of the channels and bridges of audio paths the node's Asterisk
/// has now: media channels, taps and the bridges that join them.
async fn audio_path_names(stand: &Stand) -> TestResult<Vec<String>> {
    let mut names = Vec::new();
    for kind in ["channels", "bridges"] {
        let listed = read_asterisk(&stand.control.request("GET", kind).await?.1)?;
        names.extend(
            listed
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|item| item.get("id").and_then(serde_json::Value::as_str))
                .filter(|id| {
                    [".media-", ".tap-", ".bridge-"]
                        .iter()
                        .any(|part| id.contains(part))
                })
                .map(str::to_owned),
        );
    }
    Ok(names)
}

/// The service connection of a controller started again: it runs on the
/// settings the application gave the one before it, kept in the state
/// directory, and sends again what the one before reported and the
/// application never confirmed — under the same identifier — forgetting it
/// once it is confirmed.
async fn the_new_controller_carries_on_the_service(stand: &Stand) -> TestResult {
    let mut service = ServiceEnd::accept(&stand.application).await?;
    let NodeMessage::Hello {
        applied_settings, ..
    } = service.next().await?
    else {
        return Err("the new controller's service connection did not open with a hello".into());
    };
    let later_fingerprint =
        node_protocol::fingerprint(&settings_with_a_later_entry(stand.operators)?)?;
    assert_eq!(
        applied_settings
            .as_ref()
            .map(|fingerprint| fingerprint.as_str()),
        Some(later_fingerprint.as_str()),
        "the new controller does not run on the settings the application gave"
    );
    // What the controller before reported and the application never
    // confirmed outlived it: the new one sends it again, under the same
    // identifier, and forgets it once it is confirmed.
    service.send(&ApplicationServiceMessage::Welcome)?;
    let kept = stand
        .unacknowledged
        .lock()
        .await
        .clone()
        .ok_or("no report was left unacknowledged")?;
    let again = service
        .reported("a report from before the restart", true, is_hang_up)
        .await?;
    assert_eq!(
        again, kept,
        "the report from before came back under another identifier"
    );
    let journal = stand.state.join("reports.jsonl");
    let forgotten = timeout(STEP, async {
        while !fs::read_to_string(&journal).is_ok_and(|kept| kept.trim().is_empty()) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    if forgotten.is_err() {
        return Err("a confirmed report was not forgotten by the journal".into());
    }
    Ok(())
}

/// What a call from the operator looks like to the network's Asterisk.
fn from_the_operator(number: &str) -> String {
    format!(
        "endpoint=PJSIP/{}@the-node&app={FAR_END}&callerId={CALLER_IN_URL}",
        number.replace('+', "%2B")
    )
}

/// Two calls arrive from the operator while there is no controller, and
/// Asterisk alone gives each the fallback of its entry. Returns the
/// network's channel of the one that was transferred; `network_end` is the
/// network's end of a call the node placed earlier, which is not it.
async fn asterisk_alone_gives_fallbacks(stand: &Stand, network_end: &str) -> TestResult<String> {
    let network = &stand.network.control;
    let transferred = network
        .create_channel(&from_the_operator(DIALED_WITH_TRANSFER))
        .await?;
    let turned_away = network.create_channel(&from_the_operator(DIALED)).await?;

    // The first is connected to the number of its entry's fallback: the
    // network gets a call from the node for that number, showing the line's.
    let arrived = timeout(STEP, async {
        loop {
            let channels = read_asterisk(&network.request("GET", "channels").await?.1)?;
            let found = channels.as_array().and_then(|channels| {
                channels
                    .iter()
                    .filter(|channel| {
                        channel.pointer("/dialplan/exten") == Some(&serde_json::json!(ANSWERS))
                    })
                    // The call to the second participant is one as well.
                    .find(|channel| channel.get("id") != Some(&serde_json::json!(network_end)))
                    .cloned()
            });
            if let Some(found) = found {
                return Ok::<_, Box<dyn Error>>(found);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| "Asterisk alone did not transfer a call to the fallback number")??;
    assert_eq!(
        arrived.pointer("/caller/number"),
        Some(&serde_json::json!(DIALED)),
        "the fallback's transfer did not show the line's number"
    );
    // The second is hung up on: its entry's fallback is a message, and the
    // node has no prompts to say it with.
    network.reason_it_ended(&turned_away).await?;
    Ok(transferred)
}

/// The two conversation connections a restarted controller opens: for the
/// conversation it found in Asterisk — two people, connected — and for the
/// call that waited for it. Either may come first. The first comes with its
/// participants as they were found.
async fn both_conversations_offered(
    stand: &Stand,
) -> TestResult<(Owner, Vec<ParticipantSnapshot>, Owner)> {
    let (mut carried_on, mut new) = (None, None);
    for _ in 0..2 {
        let mut owner = Owner::accept(&stand.application).await?;
        match owner.next().await? {
            ControllerMessage::Hello {
                opening:
                    Opening::Resumed {
                        participants,
                        connected,
                        ..
                    },
                ..
            } => {
                assert!(
                    participants.len() == 2
                        && participants
                            .iter()
                            .all(|snapshot| snapshot.state == ParticipantState::InConversation)
                        && matches!(connected.as_slice(), [group] if group.len() == 2),
                    "the conversation from before was found as {participants:?}, connected {connected:?}"
                );
                carried_on = Some((owner, participants));
            }
            ControllerMessage::Hello {
                opening: Opening::Started { .. },
                ..
            } => new = Some(owner),
            other @ (ControllerMessage::CommandResult { .. }
            | ControllerMessage::Event { .. }
            | ControllerMessage::Ping { .. }
            | ControllerMessage::Pong { .. }) => {
                return Err(format!("the first message was {other:?}").into());
            }
        }
    }
    match (carried_on, new) {
        (Some((carried_on, found)), Some(new)) => Ok((carried_on, found, new)),
        _ => Err("the new controller did not offer both conversations".into()),
    }
}

/// A call that still rings is turned away, and the caller's network is told
/// why — here, that the line is busy.
async fn a_ringing_call_is_turned_away(stand: &Stand) -> TestResult {
    // Each reason reaches the caller's network as its own answer — 486, 403
    // and 503 — which that network numbers 17 ("user busy"), 21 ("call
    // rejected") and 34 ("no circuit available").
    for (reason, network_reason) in [
        (RejectReason::Busy, 17),
        (RejectReason::Declined, 21),
        (RejectReason::RateLimited, 34),
    ] {
        let caller = stand
            .network
            .control
            .create_channel(&format!(
                "endpoint=PJSIP/{DIALED_IN_URL}@the-node&app={FAR_END}&callerId={CALLER_IN_URL}"
            ))
            .await?;
        let mut owner = Owner::accept(&stand.application).await?;
        let first = hello(&mut owner).await?.first;
        owner.send(&ApplicationMessage::Accept)?;
        owner.command(
            1,
            Command::Reject {
                participant: first.id.clone(),
                reason,
            },
        )?;
        let (outcome, mut events) = owner.outcome_of(1).await?;
        while events.len() < 2 {
            events.push(owner.next_event().await?);
        }
        assert!(
            matches!(outcome, CommandOutcome::Accepted)
                && matches!(
                    events.as_slice(),
                    [
                        Event::ParticipantLeft {
                            departure: Departure::Removed,
                            ..
                        },
                        Event::ConversationEnded {
                            reason: EndReason::LastParticipantLeft
                        },
                    ]
                ),
            "turning a ringing call away as {reason:?} was {outcome:?}, then {events:?}"
        );
        owner.closed().await?;
        let told = stand.network.control.reason_it_ended(&caller).await?;
        assert_eq!(
            told, network_reason,
            "the caller's network was told another reason for {reason:?}"
        );
        stand
            .expect_no_channels("after a call was turned away")
            .await?;
    }
    Ok(())
}

/// The settings of the test with one more entry: the number the
/// application adds while the node runs. Its fallback is a message.
fn settings_with_a_later_entry(operators: OperatorPorts) -> TestResult<Settings> {
    let mut value = settings(operators);
    value
        .get_mut("entries")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or("the settings have no entries")?
        .push(serde_json::json!({
            "key": { "type": "dialed_number", "number": DIALED_LATER },
            "fallback": { "type": "message", "prompt": "closed" }
        }));
    Ok(node_protocol::decode_value(value)?)
}

/// Settings the node refuses, each with the reason — and keeps the ones
/// it had: whose parts disagree, that the protocol does not allow, and
/// with a prompt nobody serves.
async fn settings_that_are_refused(stand: &Stand, service: &mut ServiceEnd) -> TestResult {
    let mut disagreeing = settings(stand.operators);
    *disagreeing
        .pointer_mut("/lines/0/operator")
        .ok_or("the settings have no line")? = serde_json::json!("nobody");
    let disagreeing: Settings = node_protocol::decode_value(disagreeing)?;
    let disagreeing_fingerprint = node_protocol::fingerprint(&disagreeing)?;
    let answer = service.give(disagreeing).await?;
    assert!(
        matches!(&answer, NodeMessage::SettingsRefused { fingerprint, problem }
            if fingerprint.as_str() == disagreeing_fingerprint
                && problem.as_str().contains("not among the operators")),
        "settings whose parts disagree were answered {answer:?}"
    );

    // Settings that are not what the protocol allows are refused with the
    // reason and the fingerprint of what was sent, on a connection that
    // goes on — the steps below take place on it.
    let mut malformed = settings(stand.operators);
    *malformed
        .pointer_mut("/operators/0/port")
        .ok_or("the settings have no operator")? = serde_json::json!("five thousand");
    let malformed_fingerprint =
        node_protocol::sha256_hex(node_protocol::canonical(&malformed).as_bytes());
    service
        .send_text(serde_json::json!({ "type": "settings", "settings": malformed }).to_string())?;
    let answer = service.next().await?;
    assert!(
        matches!(&answer, NodeMessage::SettingsRefused { fingerprint, problem }
            if fingerprint.as_str() == malformed_fingerprint
                && problem.as_str().contains("not what the protocol allows")),
        "settings the protocol does not allow were answered {answer:?}"
    );

    // A prompt the application does not serve: the settings are refused.
    let mut unfetchable = settings(stand.operators);
    *unfetchable
        .pointer_mut("/prompts/0/id")
        .ok_or("the settings have no prompt")? = serde_json::json!("nowhere");
    // Other audio than the node has: the node must fetch it.
    *unfetchable
        .pointer_mut("/prompts/0/sha256")
        .ok_or("the settings have no prompt")? = serde_json::json!("1".repeat(64));
    *unfetchable
        .pointer_mut("/entries/0/fallback/prompt")
        .ok_or("the settings have no message fallback")? = serde_json::json!("nowhere");
    let unfetchable: Settings = node_protocol::decode_value(unfetchable)?;
    let answer = service.give(unfetchable).await?;
    assert!(
        matches!(&answer, NodeMessage::SettingsRefused { problem, .. }
            if problem.as_str().contains("cannot be fetched")),
        "settings with a prompt nobody serves were answered {answer:?}"
    );
    Ok(())
}

/// The application gives the node its settings on the service connection,
/// and the node takes them in while it runs.
///
/// The node says which settings it runs on — the ones it stored — and is
/// welcomed. The same settings again change nothing. Settings whose parts
/// disagree are refused whole, with the reason. Settings with one more
/// entry are applied while Asterisk runs: a call to the new number arrives
/// as a conversation, and the node keeps the new settings for its next
/// start, written as their canonical text.
async fn the_application_gives_the_node_its_settings(stand: &Stand) -> TestResult {
    let mut service = ServiceEnd::accept(&stand.application).await?;
    let NodeMessage::Hello {
        protocol,
        node,
        asterisk_version,
        applied_settings,
        ..
    } = service.next().await?
    else {
        return Err("the service connection did not open with a hello".into());
    };
    let stored: Settings = node_protocol::decode_value(settings(stand.operators))?;
    let stored_fingerprint = node_protocol::fingerprint(&stored)?;
    assert_eq!(
        (protocol.get(), node.as_str(), asterisk_version.as_str()),
        (1, NODE, "22.11.0")
    );
    assert_eq!(
        applied_settings
            .as_ref()
            .map(|fingerprint| fingerprint.as_str()),
        Some(stored_fingerprint.as_str()),
        "the node did not say it runs on the settings it stored"
    );
    service.send(&ApplicationServiceMessage::Welcome)?;

    let answer = service.give(stored).await?;
    assert!(
        matches!(&answer, NodeMessage::SettingsApplied { fingerprint } if fingerprint.as_str() == stored_fingerprint),
        "the same settings again were answered {answer:?}"
    );

    settings_that_are_refused(stand, &mut service).await?;

    let later = settings_with_a_later_entry(stand.operators)?;
    let later_fingerprint = node_protocol::fingerprint(&later)?;
    let answer = service.give(later).await?;
    assert!(
        matches!(&answer, NodeMessage::SettingsApplied { fingerprint } if fingerprint.as_str() == later_fingerprint),
        "settings with one more entry were answered {answer:?}"
    );
    let kept = fs::read(stand.state.join("settings.json"))?;
    assert_eq!(
        node_protocol::sha256_hex(&kept),
        later_fingerprint,
        "the node did not keep the settings it applied as their canonical text"
    );
    if stand.asterisk.said("WARNING[").await || stand.asterisk.said("ERROR[").await {
        return Err("Asterisk did not take the new settings in cleanly".into());
    }

    stand
        .control
        .create_channel(&format!(
            "endpoint=Local/{}@gabion-from-network&app={FAR_END}",
            DIALED_LATER.replace('+', "%2B")
        ))
        .await?;
    let mut owner = Owner::accept(&stand.application).await?;
    assert_eq!(hello(&mut owner).await?.dialed, DIALED_LATER);
    owner.send(&ApplicationMessage::Decline {
        reason: DeclineReason::NoHandler,
    })?;
    owner.closed().await?;
    service
        .reported("a declined call to the new entry", true, is_message)
        .await?;
    stand
        .expect_no_channels("after a call to an entry added while the node ran")
        .await?;
    *stand.service.lock().await = Some(service);
    Ok(())
}

/// An instance that takes a conversation connection and then says nothing
/// at all — not even an answer to a ping — is as good as none.
///
/// The controller pings it every ten seconds, gives up on it when the
/// protocol's thirty seconds of silence have passed, and gives the caller
/// the fallback of the entry: a message, after which the caller is hung up
/// rather than left waiting for nobody.
async fn a_silent_instance_is_no_owner(stand: &Stand) -> TestResult {
    stand
        .control
        .create_channel(&format!(
            "endpoint=Local/{DIALED_IN_URL}@gabion-from-network&app={FAR_END}"
        ))
        .await?;
    let mut silent = stand.application.conversation().await?;
    if !stand
        .controller
        .comes_to_say_within(
            "heard nothing from the application for 30 seconds",
            SILENCE_LIMIT + STEP,
        )
        .await
    {
        return Err("the controller kept waiting for an instance that said nothing".into());
    }
    let mut pings = Vec::new();
    while let Ok(Some(Ok(frame))) = timeout(STEP, silent.next()).await {
        if let Frame::Text(text) = frame
            && let ControllerMessage::Ping { n } = node_protocol::decode(text.as_str())?
        {
            pings.push(n);
        }
    }
    assert_eq!(
        pings,
        [1, 2],
        "a silent instance was not pinged every ten seconds until the silence ran out"
    );
    stand
        .reported("a caller left with a silent instance", true, is_message)
        .await?;
    stand
        .expect_no_channels("after an instance that said nothing")
        .await
}

async fn every_scenario(stand: &Stand) -> TestResult {
    stand.expect_the_operator_as_it_was_given().await?;
    only_asterisk_opens_media_connections(stand).await?;
    the_application_gives_the_node_its_settings(stand).await?;
    answered_then_the_caller_hangs_up(stand).await?;
    ended_by_the_handler(stand).await?;
    declined(stand).await?;
    the_owner_breaks_the_protocol(stand).await?;
    the_application_hears_and_speaks(stand).await?;
    a_call_arrives_from_the_operator(stand).await?;
    calls_go_over_tls(stand).await?;
    a_renewed_certificate_is_taken_in(stand).await?;
    a_call_from_elsewhere_is_refused(stand).await?;
    a_ringing_call_is_turned_away(stand).await?;
    the_application_dials(stand).await?;
    the_application_starts_a_conversation(stand).await?;
    a_request_identifier_used_twice(stand).await?;
    the_application_connects_participants(stand).await?;
    two_groups_are_merged(stand).await?;
    audio_that_stops_arriving(stand).await?;
    a_call_asterisk_does_not_place(stand).await?;
    a_step_refused_for_one_who_stays(stand).await?;
    connected_people_outlive_their_owner(stand).await?;
    a_caller_alone_loses_the_application(stand).await?;
    a_silent_instance_is_no_owner(stand).await?;
    stand.expect_no_lost_control_connection().await
}
