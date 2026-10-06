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
//! The tree is taken from `ASTERISK_TREE` or from `target/asterisk-server`,
//! where `scripts/fetch-asterisk.sh` puts it. Without a tree the test fails
//! and says how to get one: a check that quietly skipped itself would report
//! a controller nobody ran.

use std::{
    collections::VecDeque,
    error::Error,
    f64::consts::TAU,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use node_protocol::{
    audio::AudioFrame,
    messages::{
        ApplicationMessage, Command, CommandOutcome, CommandRejection, ControllerMessage,
        DeclineReason, Departure, EndReason, Event, Opening, Origin, ParticipantId,
        ParticipantMedium, SegmentId,
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
    WebSocketStream, accept_async, accept_hdr_async,
    tungstenite::{
        Message as Frame,
        handshake::server::{ErrorResponse, Request, Response},
        http::{HeaderValue, header::SEC_WEBSOCKET_PROTOCOL},
    },
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// How long any single step may take before the test gives up on it. A
/// limit of the test, not of the controller: without it a failure would
/// show as a run that never ends.
const STEP: Duration = Duration::from_secs(15);

const DIALED: &str = "+19715870050";
/// `probe:probe`, the control user of the test's Asterisk, for Basic auth.
const BASIC_AUTH: &str = "cHJvYmU6cHJvYmU=";

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
fn write_configuration(
    tree: &Path,
    state: &Path,
    http_port: u16,
    controller_port: u16,
    phone_port: u16,
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
    if modules.is_empty() {
        return Err("BUILD-INFO.txt lists no modules".into());
    }

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
            format!("[general]\nenabled = yes\nbindaddr = 127.0.0.1\nbindport = {http_port}\n"),
        ),
        ("manager.conf", "[general]\nenabled = no\n".into()),
        (
            "indications.conf",
            "[general]\ncountry = us\n\n[us]\ndescription = United States\nringcadence = 2000,4000\n\
             dial = 350+440\nbusy = 480+620/500,0/500\nring = 440+480/2000,0/4000\n\
             congestion = 480+620/250,0/250\n"
                .into(),
        ),
        // Asterisk opens a control connection to the controller for every
        // call, and a media connection for every participant whose audio the
        // application asked for. `phone` is the test's own: the telephone of
        // a caller who speaks and listens.
        (
            "ari.conf",
            "[general]\nenabled = yes\n\n[probe]\ntype = user\nread_only = no\npassword = probe\n\n\
             [controller]\ntype = outbound_websocket\nwebsocket_client_id = controller\napps = gabion\n\
             local_ari_user = probe\n"
                .into(),
        ),
        (
            "websocket_client.conf",
            format!(
                "[controller]\ntype = websocket_client\nuri = ws://127.0.0.1:{controller_port}/\n\
                 protocols = ari\nconnection_type = per_call_config\nconnection_timeout = 500\n\
                 reconnect_interval = 500\nreconnect_attempts = 4\ntls_enabled = no\n\n\
                 [gabion-media]\ntype = websocket_client\n\
                 uri = ws://127.0.0.1:{controller_port}/media\nprotocols = media\n\
                 connection_type = per_call_config\nconnection_timeout = 500\n\
                 reconnect_interval = 500\nreconnect_attempts = 4\ntls_enabled = no\n\n\
                 [phone]\ntype = websocket_client\nuri = ws://127.0.0.1:{phone_port}/phone\n\
                 protocols = media\nconnection_type = per_call_config\n\
                 connection_timeout = 500\nreconnect_interval = 500\nreconnect_attempts = 4\n\
                 tls_enabled = no\n"
            ),
        ),
        // `in` is how a call from the telephone network enters; `caller` is
        // what the far end of the test's call does once it is answered.
        (
            "extensions.conf",
            format!(
                "[probe]\nexten = in,1,Stasis(gabion,dialed_number,{DIALED})\n same = n,Hangup()\n\
                 exten = caller,1,Wait(600)\n"
            ),
        ),
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
        "pjsip.conf",
        "stasis.conf",
        "udptl.conf",
    ] {
        fs::write(etc.join(name), "")?;
    }
    Ok(etc.join("asterisk.conf"))
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

/// One request to the test Asterisk's control interface; returns status and body.
///
/// The answer is read by its stated length, not until the connection ends:
/// Asterisk may end the connection with a reset once it has answered, and a
/// reset after a complete answer is not a failure of the request.
async fn asterisk_http(port: u16, method: &str, path: &str) -> TestResult<(u16, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    let request = format!(
        "{method} /ari/{path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Basic {BASIC_AUTH}\r\n\
         Content-Length: 0\r\nConnection: close\r\n\r\n"
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

/// Place a call: `endpoint` is the channel Asterisk creates for the caller,
/// `extension` what that channel does once it is answered. Returns the
/// identifier of the caller's channel.
async fn place_call(http_port: u16, endpoint: &str, extension: &str) -> TestResult<String> {
    let (status, body) = asterisk_http(
        http_port,
        "POST",
        &format!("channels?endpoint={endpoint}&extension={extension}&context=probe&priority=1"),
    )
    .await?;
    if status != 200 {
        return Err(format!("Asterisk refused the test call: {status} {body}").into());
    }
    body.split("\"id\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .map(str::to_owned)
        .ok_or_else(|| format!("no channel identifier in {body}").into())
}

/// What Asterisk still has of the given kind — `channels` or `bridges`.
async fn left(http_port: u16, kind: &str) -> TestResult<String> {
    Ok(asterisk_http(http_port, "GET", kind).await?.1)
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
/// Asterisk connected to the test, in the telephone network's own audio
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

    /// Wait for Asterisk to connect the caller's telephone.
    async fn accept(listener: &TcpListener) -> TestResult<Self> {
        #[expect(
            clippy::result_large_err,
            clippy::unnecessary_wraps,
            reason = "the callback's signature is the WebSocket library's"
        )]
        fn agree_on_subprotocol(
            _: &Request,
            mut response: Response,
        ) -> Result<Response, ErrorResponse> {
            response
                .headers_mut()
                .insert(SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("media"));
            Ok(response)
        }

        let (stream, _) = timeout(STEP, listener.accept())
            .await
            .map_err(|_| "Asterisk did not connect the caller's telephone")??;
        let mut socket = accept_hdr_async(stream, agree_on_subprotocol).await?;
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

/// A running Asterisk with its controller, and the place where the
/// controller opens conversation connections.
struct Stand {
    http_port: u16,
    application: TcpListener,
    /// Where Asterisk connects the telephone of a caller who speaks.
    phone: TcpListener,
    controller: Running,
    asterisk: Running,
}

impl Stand {
    async fn start() -> TestResult<Self> {
        let tree = asterisk_tree()?;
        let state = Path::new(env!("CARGO_TARGET_TMPDIR")).join("against-asterisk");
        let _ = fs::remove_dir_all(&state);

        let http_port = free_port().await?;
        let controller_port = free_port().await?;
        let application = TcpListener::bind("127.0.0.1:0").await?;
        let application_port = application.local_addr()?.port();
        let phone = TcpListener::bind("127.0.0.1:0").await?;
        let phone_port = phone.local_addr()?.port();

        let mut controller = Process::new(env!("CARGO_BIN_EXE_asterisk-controller"));
        controller.args([
            "--node",
            "test-node",
            "--listen",
            &format!("127.0.0.1:{controller_port}"),
            "--application",
            &format!("ws://127.0.0.1:{application_port}/"),
        ]);
        let controller = start(controller, "asterisk-controller ready", "the controller").await?;

        let configuration =
            write_configuration(&tree, &state, http_port, controller_port, phone_port)?;
        let mut asterisk = Process::new(tree.join("sbin/asterisk"));
        asterisk.arg("-C").arg(&configuration).args(["-f", "-n"]);
        if cfg!(target_os = "linux") {
            asterisk.env("LD_LIBRARY_PATH", tree.join("lib"));
        }
        let asterisk = start(asterisk, "Asterisk Ready.", "Asterisk").await?;

        Ok(Self {
            http_port,
            application,
            phone,
            controller,
            asterisk,
        })
    }

    /// What both processes have to say about a failed test.
    async fn report(&mut self) {
        self.asterisk.report().await;
        self.controller.report().await;
    }

    /// Asterisk closed every control connection itself.
    ///
    /// A control connection the controller lets go of — at any moment, in
    /// any way — is to Asterisk a lost one: it opens the connection again
    /// for a call that may be over, and can crash while it drops the old
    /// one. Asterisk reports each such loss, and the controller reports each
    /// connection that then arrives without a call; neither may have
    /// happened, whatever the scenarios themselves saw.
    async fn expect_no_lost_control_connection(&self) -> TestResult {
        if self.asterisk.said("Websocket disconnected").await {
            return Err(
                "Asterisk lost a control connection: the controller let go of it \
                        before Asterisk closed it"
                    .into(),
            );
        }
        if self.controller.said("carried no call").await {
            return Err("the controller was given a control connection without a call".into());
        }
        Ok(())
    }

    /// Place a call from the telephone network — a channel that enters
    /// through `in`, ringing — and take the conversation the controller
    /// opens for it. Returns the identifier of the caller's end.
    async fn call(&self) -> TestResult<(String, Owner)> {
        let caller = place_call(self.http_port, "Local/in@probe", "caller").await?;
        Ok((caller, Owner::accept(&self.application).await?))
    }

    /// The same from a caller who speaks and listens. Their telephone is
    /// itself the channel that enters through `in`, and it enters answered:
    /// Asterisk runs the dialplan for it only once the test has taken it.
    async fn call_from_a_phone(&self) -> TestResult<(String, Phone, Owner)> {
        let caller = place_call(self.http_port, "WebSocket/phone/c(ulaw)", "in").await?;
        let phone = Phone::accept(&self.phone).await?;
        Ok((caller, phone, Owner::accept(&self.application).await?))
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
        let mut still = String::new();
        let emptied = timeout(STEP, async {
            loop {
                still = left(self.http_port, kind).await?;
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

    asterisk_http(stand.http_port, "DELETE", &format!("channels/{caller}")).await?;
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
    asterisk_http(stand.http_port, "DELETE", &format!("channels/{caller}")).await?;
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

#[tokio::test(flavor = "multi_thread")]
async fn calls_are_carried_from_their_first_ring_to_their_end() -> TestResult {
    let mut stand = Stand::start().await?;
    let outcome = every_scenario(&stand).await;
    if outcome.is_err() {
        stand.report().await;
    }
    outcome
}

async fn every_scenario(stand: &Stand) -> TestResult {
    answered_then_the_caller_hangs_up(stand).await?;
    ended_by_the_handler(stand).await?;
    declined(stand).await?;
    the_owner_breaks_the_protocol(stand).await?;
    the_application_hears_and_speaks(stand).await?;
    stand.expect_no_lost_control_connection().await
}
