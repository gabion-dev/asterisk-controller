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
    error::Error,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use node_protocol::messages::{
    ApplicationMessage, Command, CommandOutcome, CommandRejection, ControllerMessage,
    DeclineReason, Departure, EndReason, Event, Opening, Origin, ParticipantMedium,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    process::{Child, Command as Process},
    time::timeout,
};
use tokio_tungstenite::{WebSocketStream, accept_async, tungstenite::Message as Frame};

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
        // Asterisk opens a control connection to the controller for every call.
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
                 reconnect_interval = 500\nreconnect_attempts = 4\ntls_enabled = no\n"
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

/// Start a process and wait until it prints `ready` on a line of its own
/// output; its death before that is the failure.
async fn start(mut process: Process, ready: &str, what: &str) -> TestResult<Child> {
    let mut child = process
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let mut lines = BufReader::new(stdout).lines();
    let waited = timeout(STEP, async {
        while let Some(line) = lines.next_line().await? {
            if line.contains(ready) {
                // Keep draining, so the process never blocks on a full pipe.
                tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
                return Ok::<bool, std::io::Error>(true);
            }
        }
        Ok(false)
    })
    .await;
    match waited {
        Ok(Ok(true)) => Ok(child),
        Ok(Ok(false)) => Err(format!("{what} exited before it was ready").into()),
        Ok(Err(error)) => Err(error.into()),
        Err(_) => Err(format!("{what} did not become ready").into()),
    }
}

/// One request to the test Asterisk's control interface; returns status and body.
async fn asterisk_http(port: u16, method: &str, path: &str) -> TestResult<(u16, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    let request = format!(
        "{method} /ari/{path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Basic {BASIC_AUTH}\r\n\
         Content-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    let mut response = String::new();
    timeout(STEP, stream.read_to_string(&mut response)).await??;
    let status = response
        .split_whitespace()
        .nth(1)
        .ok_or("no status line")?
        .parse()?;
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or_default();
    Ok((status, body))
}

/// Place a call from the telephone network: a channel enters through `in`,
/// ringing. Returns the identifier of the caller's end.
async fn place_call(http_port: u16) -> TestResult<String> {
    let (status, body) = asterisk_http(
        http_port,
        "POST",
        "channels?endpoint=Local/in@probe&extension=caller&context=probe&priority=1",
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

async fn channels_left(http_port: u16) -> TestResult<String> {
    Ok(asterisk_http(http_port, "GET", "channels").await?.1)
}

/// The application's end of one conversation connection.
struct Owner(WebSocketStream<TcpStream>);

impl Owner {
    /// Wait for the controller to open a conversation connection.
    async fn accept(listener: &TcpListener) -> TestResult<Self> {
        let (stream, _) = timeout(STEP, listener.accept())
            .await
            .map_err(|_| "the controller did not open a conversation connection")??;
        Ok(Self(accept_async(stream).await?))
    }

    async fn next(&mut self) -> TestResult<ControllerMessage> {
        loop {
            let frame = timeout(STEP, self.0.next())
                .await
                .map_err(|_| "the controller said nothing")?
                .ok_or("the controller closed the conversation connection")??;
            if let Frame::Text(text) = frame {
                return Ok(node_protocol::decode(text.as_str())?);
            }
        }
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
        self.0
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
            match timeout(STEP, self.0.next())
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
    _controller: Child,
    _asterisk: Child,
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

        let configuration = write_configuration(&tree, &state, http_port, controller_port)?;
        let mut asterisk = Process::new(tree.join("sbin/asterisk"));
        asterisk.arg("-C").arg(&configuration).args(["-f", "-n"]);
        if cfg!(target_os = "linux") {
            asterisk.env("LD_LIBRARY_PATH", tree.join("lib"));
        }
        let asterisk = start(asterisk, "Asterisk Ready.", "Asterisk").await?;

        Ok(Self {
            http_port,
            application,
            _controller: controller,
            _asterisk: asterisk,
        })
    }

    /// Place a call and take the conversation the controller opens for it.
    async fn call(&self) -> TestResult<(String, Owner)> {
        let caller = place_call(self.http_port).await?;
        Ok((caller, Owner::accept(&self.application).await?))
    }

    /// Asterisk ends up with no channels at all.
    ///
    /// The controller reports a participant gone when the channel leaves
    /// the application; Asterisk takes a moment more to destroy it. So this
    /// waits for the state itself, and fails if it never comes.
    async fn expect_no_channels(&self, after: &str) -> TestResult {
        let mut left = String::new();
        let emptied = timeout(STEP, async {
            loop {
                left = channels_left(self.http_port).await?;
                if left == "[]" {
                    return Ok::<(), Box<dyn Error>>(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        match emptied {
            Ok(result) => result,
            Err(_) => Err(format!("{after}: Asterisk still has {left}").into()),
        }
    }
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
        .0
        .send(Frame::text(r#"{"type":"accept","note":"x"}"#))
        .await?;
    owner.closed().await?;
    stand
        .expect_no_channels("after the owner broke the protocol")
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn calls_are_carried_from_their_first_ring_to_their_end() -> TestResult {
    let stand = Stand::start().await?;
    answered_then_the_caller_hangs_up(&stand).await?;
    ended_by_the_handler(&stand).await?;
    declined(&stand).await?;
    the_owner_breaks_the_protocol(&stand).await
}
