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
    sync::Arc,
    time::Duration,
};

use data_encoding::BASE64;
use futures_util::{SinkExt, StreamExt};
use node_protocol::{
    audio::AudioFrame,
    messages::{
        ApplicationMessage, Command, CommandOutcome, CommandRejection, ControllerMessage,
        DeclineReason, Departure, EndReason, Event, Opening, Origin, ParticipantId,
        ParticipantMedium, RejectReason, SegmentId,
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
    WebSocketStream, accept_async, connect_async,
    tungstenite::{
        Message as Frame,
        client::IntoClientRequest,
        http::{HeaderValue, header::SEC_WEBSOCKET_PROTOCOL},
    },
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// How long any single step may take before the test gives up on it. A
/// limit of the test, not of the controller: without it a failure would
/// show as a run that never ends.
const STEP: Duration = Duration::from_secs(15);

/// The number of the node's one entry.
const DIALED: &str = "+19715870050";
/// The same, as it is written inside an address.
const DIALED_IN_URL: &str = "%2B19715870050";
/// The password the node presents to its operator: everything Asterisk's
/// configuration format treats specially inside a value.
const OPERATOR_PASSWORD: &str = "pass;word #1 = [x], \\>";
/// The number of the node's other entry, whose fallback is a transfer.
const DIALED_WITH_TRANSFER: &str = "+19715870051";
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
/// An address of this machine that is not the operator's. The stranger of
/// the test calls from it.
const STRANGER_ADDRESS: &str = "127.0.0.2";

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

/// Write the configuration of the test's Asterisk and return its main file.
/// The settings the node of the test runs on: one entry, and the operator
/// whose trunk is the test's telephone network, with a line through it.
fn settings(operator_port: u16) -> String {
    serde_json::json!({
        "operators": [{
            "id": "the test's operator",
            "host": "127.0.0.1",
            "port": operator_port,
            "transport": "udp",
            "credentials": { "username": "node", "password": OPERATOR_PASSWORD },
            "source_networks": ["127.0.0.1/32"],
            "countries": ["US"]
        }],
        "lines": [{
            "id": "main",
            "number": DIALED,
            "operator": "the test's operator",
            // Canada is allowed by the line and carried by no operator.
            "allowed_countries": ["US", "CA"],
            "concurrent_outbound_limit": 1
        }],
        // Two entries, for the two things a fallback can be. The node has
        // no prompts yet, so a message is only a hang-up.
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
            }
        ],
        "prompts": [{ "id": "closed", "sha256": "0".repeat(64) }]
    })
    .to_string()
}

/// Ports of the telephone network of the test.
struct NetworkPorts {
    http: u16,
    /// Where the operator's trunk is: what the node's settings name.
    operator: u16,
    /// Where the stranger sends from.
    stranger: u16,
    audio: u16,
    /// Where the node listens for operators.
    node: u16,
}

/// Write the configuration of the telephone network and return its main
/// file. To it the node is a customer reached at the node's address; the
/// operator calls from the address the node's settings accept, the stranger
/// from another one.
fn write_network_configuration(
    tree: &Path,
    state: &Path,
    ports: &NetworkPorts,
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
        ("pjsip.conf", network_sides(ports)),
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
fn network_sides(ports: &NetworkPorts) -> String {
    let side = |name: &str, transport: &str, more: &str| {
        format!(
            "[{name}]\ntype = endpoint\ntransport = {transport}\ncontext = from-the-node\n\
             disallow = all\nallow = ulaw\naors = {name}\ndirect_media = no\n{more}\n\
             [{name}]\ntype = aor\ncontact = sip:127.0.0.1:{}\n\n",
            ports.node,
        )
    };
    format!(
        "[global]\ntype = global\nendpoint_identifier_order = ip\n\n\
         [operator]\ntype = transport\nprotocol = udp\nbind = 127.0.0.1:{}\n\n\
         [stranger]\ntype = transport\nprotocol = udp\nbind = {STRANGER_ADDRESS}:{}\n\n\
         {}{}\
         [the-node]\ntype = identify\nendpoint = the-node\nmatch = 127.0.0.1/32\n\n\
         [the-node-credentials]\ntype = auth\nauth_type = userpass\nusername = node\n\
         password = {}\n",
        ports.operator,
        ports.stranger,
        // The operator asks the node who it is, and knows the same password
        // the node's settings carry.
        side("the-node", "operator", "auth = the-node-credentials\n"),
        side("the-node-from-elsewhere", "stranger", ""),
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

/// Start the controller and wait until it is ready for Asterisk.
async fn start_controller(arguments: &[OsString]) -> TestResult<Running> {
    let mut controller = Process::new(env!("CARGO_BIN_EXE_asterisk-controller"));
    controller.args(arguments);
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
}

impl ControlInterface {
    /// The control interface of the Asterisk whose state directory this is,
    /// as its user `user`. The password is read from the configuration: the
    /// node's is made by the controller anew for every start and lives only
    /// in the file it wrote for Asterisk.
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
        let credentials = self
            .authorization
            .strip_prefix("Basic ")
            .map(|encoded| BASE64.decode(encoded.as_bytes()))
            .ok_or("no credentials")??;
        let url = format!(
            "ws://127.0.0.1:{}/ari/events?app={FAR_END}&api_key={}",
            self.port,
            String::from_utf8(credentials)?,
        );
        let (mut events, _response) = connect_async(url).await?;
        let ended = Arc::clone(&self.ended);
        tokio::spawn(async move {
            while let Some(Ok(frame)) = events.next().await {
                let Frame::Text(text) = frame else { continue };
                let Ok(event) = read_asterisk(text.as_str()) else {
                    continue;
                };
                if event.get("type").and_then(serde_json::Value::as_str) == Some("ChannelDestroyed")
                    && let Some(channel) = event.pointer("/channel/id").and_then(|id| id.as_str())
                    && let Some(cause) = event.get("cause").and_then(serde_json::Value::as_i64)
                {
                    ended.lock().await.push((channel.to_owned(), cause));
                }
            }
        });
        Ok(())
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
}

impl Phone {
    /// The frequency the caller says, Hz.
    const SAYS: u32 = 400;
    /// The telephone network's sampling rate, Hz.
    const RATE: u32 = 8000;
    /// Samples of twenty milliseconds at that rate.
    const FRAME: u32 = 160;

    /// Connect the caller's telephone to the media channel Asterisk made for it.
    async fn connect(control: &ControlInterface, channel: &str) -> TestResult<Self> {
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
        tokio::spawn(async move {
            // A telephone speaks in real time: one frame every twenty milliseconds.
            let mut pace = tokio::time::interval(Duration::from_millis(20));
            let mut said = 0;
            loop {
                tokio::select! {
                    _ = pace.tick() => {
                        let frame: Vec<u8> = tone(Self::SAYS, Self::RATE, said, Self::FRAME)
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
        Ok(Self { heard })
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
}

/// The application's end of one conversation connection.
struct Owner {
    socket: WebSocketStream<TcpStream>,
    /// Messages that arrived while the test was waiting for audio.
    unread: VecDeque<ControllerMessage>,
    /// Audio frames that arrived, in order.
    heard: Vec<AudioFrame>,
}

impl Owner {
    /// Wait for the controller to open a conversation connection.
    async fn accept(listener: &TcpListener) -> TestResult<Self> {
        let (stream, _) = timeout(STEP, listener.accept())
            .await
            .map_err(|_| "the controller did not open a conversation connection")??;
        Ok(Self {
            socket: accept_async(stream).await?,
            unread: VecDeque::new(),
            heard: Vec::new(),
        })
    }

    /// Take one frame off the connection: a message goes to `unread`, an
    /// audio frame — decoded by the protocol library — to `heard`.
    async fn read(&mut self) -> TestResult {
        let frame = timeout(STEP, self.socket.next())
            .await
            .map_err(|_| "the controller said nothing")?
            .ok_or("the controller closed the conversation connection")??;
        match frame {
            Frame::Text(text) => self.unread.push_back(node_protocol::decode(text.as_str())?),
            Frame::Binary(bytes) => self.heard.push(AudioFrame::decode(&bytes)?),
            Frame::Ping(_) | Frame::Pong(_) | Frame::Close(_) | Frame::Frame(_) => {}
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

    /// Send audio of a queued segment, as one playback frame.
    async fn audio(
        &mut self,
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
        self.socket.send(Frame::binary(frame.encode()?)).await?;
        Ok(())
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

    async fn send(&mut self, message: &ApplicationMessage) -> TestResult {
        self.socket
            .send(Frame::text(node_protocol::encode(message)?))
            .await?;
        Ok(())
    }

    async fn command(&mut self, id: u64, command: Command) -> TestResult {
        self.send(&ApplicationMessage::Command { id, command })
            .await
    }

    /// The connection is closed by the controller, with nothing more said.
    async fn closed(&mut self) -> TestResult {
        loop {
            match timeout(STEP, self.socket.next())
                .await
                .map_err(|_| "the connection stayed open")?
            {
                None | Some(Ok(Frame::Close(_)) | Err(_)) => return Ok(()),
                Some(Ok(Frame::Text(text))) => {
                    return Err(format!("unexpected message {text}").into());
                }
                Some(Ok(_)) => {}
            }
        }
    }
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
        let channel = self
            .control
            .create_channel(&format!(
                "endpoint=WebSocket/INCOMING/c(ulaw)&app={FAR_END}"
            ))
            .await?;
        let phone = Phone::connect(&self.control, &channel).await?;
        self.control
            .request("POST", "bridges?type=mixing&bridgeId=the-callers-phone")
            .await?;
        // Both ends enter the test's application a moment after they are
        // answered; Asterisk joins them only once they are there.
        let joined = timeout(STEP, async {
            loop {
                let (status, _) = self
                    .control
                    .request(
                        "POST",
                        &format!("bridges/the-callers-phone/addChannel?channel={caller},{channel}"),
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

/// A running Asterisk with its controller, the place where the controller
/// opens conversation connections, and the telephone network around them.
struct Stand {
    control: ControlInterface,
    application: TcpListener,
    /// The state directory of the node.
    state: PathBuf,
    /// What the controller is started with.
    controller_arguments: Vec<OsString>,
    controller: Running,
    asterisk: Running,
    network: Network,
}

impl Stand {
    async fn start() -> TestResult<Self> {
        let tree = asterisk_tree()?;
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("against-asterisk");
        let _ = fs::remove_dir_all(&root);
        let state = root.join("node");
        fs::create_dir_all(&state)?;

        let http_port = free_port().await?;
        let controller_port = free_port().await?;
        let sip_port = free_port().await?;
        // Audio ports go in pairs that begin on an even one.
        let audio_port = free_port().await? & !1;
        let application = TcpListener::bind("127.0.0.1:0").await?;
        let application_port = application.local_addr()?.port();
        let network_ports = NetworkPorts {
            http: free_port().await?,
            operator: free_port().await?,
            stranger: free_port().await?,
            audio: free_port().await? & !1,
            node: sip_port,
        };
        fs::write(
            state.join("settings.json"),
            settings(network_ports.operator),
        )?;

        let controller_arguments: Vec<OsString> = [
            "--node".into(),
            "test-node".into(),
            "--listen".into(),
            format!("127.0.0.1:{controller_port}").into(),
            "--application".into(),
            format!("ws://127.0.0.1:{application_port}/").into(),
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
        ]
        .into();
        let controller = start_controller(&controller_arguments).await?;

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
        let network_file = write_network_configuration(&tree, &network_state, &network_ports)?;
        let mut network = start_asterisk(&tree, &network_file, "the telephone network").await?;
        if network.said("ERROR[").await {
            network.report().await;
            return Err(format!(
                "the telephone network of the test did not start cleanly; it needs \
                 {STRANGER_ADDRESS} to be an address of this machine"
            )
            .into());
        }
        let network_control = ControlInterface::of(network_ports.http, &network_state, "network")?;
        network_control.serve_far_ends().await?;

        Ok(Self {
            control,
            application,
            state,
            controller_arguments,
            controller,
            asterisk,
            network: Network {
                control: network_control,
                asterisk: network,
            },
        })
    }

    /// Kill the controller as a crash would. Asterisk goes on running.
    async fn kill_the_controller(&mut self) -> TestResult {
        self.controller.child.kill().await?;
        Ok(())
    }

    /// Start a controller in place of the one that was killed.
    async fn start_the_controller(&mut self) -> TestResult {
        self.controller = start_controller(&self.controller_arguments).await?;
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
        let caller = self
            .control
            .create_channel(&format!(
                "endpoint=WebSocket/INCOMING/c(ulaw)&extension={DIALED_IN_URL}\
                 &context=gabion-from-network&priority=1"
            ))
            .await?;
        let phone = Phone::connect(&self.control, &caller).await?;
        Ok((caller, phone, Owner::accept(&self.application).await?))
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
}

/// Nobody is heard, and nothing is played to anybody, before they answer.
async fn refuses_to_listen_before_the_answer(
    owner: &mut Owner,
    participant: &ParticipantId,
) -> TestResult {
    owner
        .command(
            7,
            Command::Listen {
                participant: participant.clone(),
            },
        )
        .await?;
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
    assert_eq!(node.as_str(), "test-node");
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

    owner.send(&ApplicationMessage::Accept).await?;
    refuses_to_listen_before_the_answer(&mut owner, &participant).await?;

    owner
        .command(
            1,
            Command::Answer {
                participant: participant.clone(),
            },
        )
        .await?;
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
    owner
        .command(
            2,
            Command::Answer {
                participant: "p-9".parse()?,
            },
        )
        .await?;
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
    owner.send(&ApplicationMessage::Accept).await?;
    owner.command(1, Command::End).await?;
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
    let (_caller, mut owner) = stand.call().await?;
    owner.next().await?;
    owner
        .send(&ApplicationMessage::Decline {
            reason: DeclineReason::NoHandler,
        })
        .await?;
    owner.closed().await?;
    stand.expect_no_channels("after a declined call").await
}

/// The application breaks the protocol: nobody is left on the line either.
async fn the_owner_breaks_the_protocol(stand: &Stand) -> TestResult {
    let (_caller, mut owner) = stand.call().await?;
    owner.next().await?;
    // "accept" has no fields; a field on it is not the protocol.
    owner
        .socket
        .send(Frame::text(r#"{"type":"accept","note":"x"}"#))
        .await?;
    owner.closed().await?;
    stand
        .expect_no_channels("after the owner broke the protocol")
        .await
}

/// The application hears what the caller says, as frames of twenty
/// milliseconds in the one format of the protocol.
async fn hears_the_caller(owner: &mut Owner, participant: &ParticipantId) -> TestResult {
    owner
        .command(
            3,
            Command::Listen {
                participant: participant.clone(),
            },
        )
        .await?;
    let (outcome, _) = owner.outcome_of(3).await?;
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

    owner
        .command(
            4,
            Command::StopListening {
                participant: participant.clone(),
            },
        )
        .await?;
    let (outcome, _) = owner.outcome_of(4).await?;
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
) -> TestResult {
    let greeting: SegmentId = "greeting".parse()?;
    owner
        .command(
            5,
            Command::Play {
                participant: participant.clone(),
                segment: greeting.clone(),
            },
        )
        .await?;
    // Half a second, cut where no frame of twenty milliseconds ends.
    let audio = tone(1000, 16_000, 0, 8000);
    let (first, second) = audio.split_at(5000);
    owner.audio(participant, &greeting, first, false).await?;
    owner.audio(participant, &greeting, second, true).await?;
    let (outcome, mut events) = owner.outcome_of(5).await?;
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

    owner.command(6, play(&long)).await?;
    // Five seconds, in frames as large as a playback frame may be.
    let audio = tone(1000, 16_000, 0, 80_000);
    let mut parts = audio.chunks(32_000).peekable();
    while let Some(part) = parts.next() {
        owner
            .audio(participant, &long, part, parts.peek().is_none())
            .await?;
    }
    owner.command(7, play(&behind_it)).await?;
    owner
        .audio(participant, &behind_it, &tone(1000, 16_000, 0, 3200), true)
        .await?;
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

    owner
        .command(
            8,
            Command::FlushPlayback {
                participant: participant.clone(),
            },
        )
        .await?;
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
    owner.command(9, play(&afterwards)).await?;
    owner
        .audio(participant, &afterwards, &tone(1000, 16_000, 0, 3200), true)
        .await?;
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
    owner.send(&ApplicationMessage::Accept).await?;
    hears_the_caller(&mut owner, &participant).await?;
    plays_to_the_caller(&mut owner, &mut phone, &participant).await?;
    flushes_the_queue(&mut owner, &participant).await?;

    // A name still in the queue cannot be queued again.
    let pending: SegmentId = "pending".parse()?;
    for id in [10, 11] {
        owner
            .command(
                id,
                Command::Play {
                    participant: participant.clone(),
                    segment: pending.clone(),
                },
            )
            .await?;
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
    let ControlMessageHello { dialed, first } = hello(&mut owner).await?;
    assert_eq!(dialed, DIALED);
    assert_eq!(first.medium, ParticipantMedium::TelephoneNetwork);
    assert_eq!(
        first.number.as_ref().map(|number| number.as_str()),
        Some(CALLER),
        "the caller's number did not arrive with the call"
    );

    owner.send(&ApplicationMessage::Accept).await?;
    owner
        .command(
            1,
            Command::Answer {
                participant: first.id.clone(),
            },
        )
        .await?;
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
    hears_the_caller(&mut owner, &first.id).await?;
    plays_to_the_caller(&mut owner, &mut phone, &first.id).await?;

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
    owner
        .command(
            id,
            Command::Dial {
                number: number.parse()?,
                line: line.parse()?,
                answer_limit_ms,
            },
        )
        .await?;
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
    owner.send(&ApplicationMessage::Accept).await?;
    owner
        .command(
            1,
            Command::Answer {
                participant: first.id.clone(),
            },
        )
        .await?;
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
    hears_the_caller(owner, &second).await?;
    plays_to_the_caller(owner, &mut phone, &second).await?;

    owner
        .command(
            8,
            Command::Remove {
                participant: second.clone(),
            },
        )
        .await?;
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
    owner
        .command(
            13,
            Command::Remove {
                participant: third.clone(),
            },
        )
        .await?;
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

/// What a hello says about a new conversation.
struct ControlMessageHello {
    dialed: String,
    first: node_protocol::messages::Participant,
}

/// Read the hello of a conversation that has just been opened.
async fn hello(owner: &mut Owner) -> TestResult<ControlMessageHello> {
    let ControllerMessage::Hello {
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
        dialed: dialed.as_str().to_owned(),
        first,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn calls_are_carried_from_their_first_ring_to_their_end() -> TestResult {
    let mut stand = Stand::start().await?;
    let mut outcome = every_scenario(&stand).await;
    if outcome.is_ok() {
        // Last, because here the controller and Asterisk do lose each other.
        outcome = the_node_without_its_controller(&mut stand).await;
    }
    if outcome.is_err() {
        stand.report().await;
    }
    outcome
}

/// The node without its controller, and with it again.
///
/// The controller dies with a call in progress. Asterisk goes on running,
/// and to it the node's application no longer exists: calls that arrive
/// wait a few seconds for the controller and are then given the fallback of
/// their entry by Asterisk alone — a transfer through the operator, or a
/// hang-up. A call that is still waiting when a controller is started is
/// served. The call from before is nobody's then — this build does not
/// resume conversations — and its participant is not left on the line.
async fn the_node_without_its_controller(stand: &mut Stand) -> TestResult {
    let (_caller, mut owner) = stand.call().await?;
    let first = hello(&mut owner).await?.first;
    owner.send(&ApplicationMessage::Accept).await?;
    owner
        .command(
            1,
            Command::Answer {
                participant: first.id,
            },
        )
        .await?;
    owner.outcome_of(1).await?;

    stand.kill_the_controller().await?;
    owner.closed().await?;

    // Two calls arrive from the operator while there is no controller.
    let from_the_operator = |number: &str| {
        format!(
            "endpoint=PJSIP/{}@the-node&app={FAR_END}&callerId={CALLER_IN_URL}",
            number.replace('+', "%2B")
        )
    };
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
                    .find(|channel| {
                        channel.pointer("/dialplan/exten") == Some(&serde_json::json!(ANSWERS))
                    })
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

    // The call that was in progress is still there: Asterisk dropped nobody.
    let (_, still) = stand.control.request("GET", "channels").await?;
    assert!(
        still.contains("Stasis"),
        "the call in progress did not outlive the controller: {still}"
    );

    // A call that arrives a moment before a controller is started waits
    // for it and is served.
    let waited = network.create_channel(&from_the_operator(DIALED)).await?;
    stand.start_the_controller().await?;
    let network = &stand.network.control;
    let mut owner = Owner::accept(&stand.application).await?;
    hello(&mut owner).await?;
    owner.send(&ApplicationMessage::Accept).await?;
    owner.command(1, Command::End).await?;
    owner.outcome_of(1).await?;
    network.reason_it_ended(&waited).await?;

    if !stand
        .controller
        .said("left from before this connection")
        .await
    {
        return Err("the new controller did not remove what the old one left".into());
    }
    network
        .request("DELETE", &format!("channels/{transferred}"))
        .await?;
    network
        .expect_none("channels", "after the node was without its controller")
        .await?;
    stand
        .expect_no_channels("after the node was without its controller")
        .await
}

/// A call that still rings is turned away, and the caller's network is told
/// why — here, that the line is busy.
async fn a_ringing_call_is_turned_away(stand: &Stand) -> TestResult {
    let caller = stand
        .network
        .control
        .create_channel(&format!(
            "endpoint=PJSIP/{DIALED_IN_URL}@the-node&app={FAR_END}&callerId={CALLER_IN_URL}"
        ))
        .await?;
    let mut owner = Owner::accept(&stand.application).await?;
    let first = hello(&mut owner).await?.first;
    owner.send(&ApplicationMessage::Accept).await?;
    owner
        .command(
            1,
            Command::Reject {
                participant: first.id.clone(),
                reason: RejectReason::Busy,
            },
        )
        .await?;
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
        "turning a ringing call away was {outcome:?}, then {events:?}"
    );
    owner.closed().await?;
    // 17 is the telephone network's number for "user busy".
    let reason = stand.network.control.reason_it_ended(&caller).await?;
    assert_eq!(reason, 17, "the caller's network was told another reason");
    stand
        .expect_no_channels("after a call was turned away")
        .await
}

async fn every_scenario(stand: &Stand) -> TestResult {
    stand.expect_the_operator_as_it_was_given().await?;
    answered_then_the_caller_hangs_up(stand).await?;
    ended_by_the_handler(stand).await?;
    declined(stand).await?;
    the_owner_breaks_the_protocol(stand).await?;
    the_application_hears_and_speaks(stand).await?;
    a_call_arrives_from_the_operator(stand).await?;
    a_call_from_elsewhere_is_refused(stand).await?;
    a_ringing_call_is_turned_away(stand).await?;
    the_application_dials(stand).await?;
    stand.expect_no_lost_control_connection().await
}
