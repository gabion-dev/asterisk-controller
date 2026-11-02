// crates/asterisk-controller/src/launch.rs

//! The launcher of a telephony node: Asterisk together with its controller,
//! kept running by one rule wherever a node runs — under the Gabion
//! development environment and on a telephony server alike.
//!
//! ```text
//! asterisk-controller launch --restarts <n> --stable-after <milliseconds> \
//!     --restart-delay <milliseconds> <the controller's arguments>
//! ```
//!
//! The rule:
//!
//! - the controller first. Before its ready line it writes the whole of
//!   Asterisk's configuration; Asterisk is started on that line, on the
//!   controller's main file. Its first start belongs to the controller's
//!   first ready line, every later one to Asterisk's own restart ladder: a
//!   controller that comes back finds Asterisk by itself.
//! - Asterisk is ready when it says it has booted AND its HTTP port answers
//!   as Asterisk: it reports a finished boot even when its HTTP server could
//!   not bind, and call control rides that server. A booted Asterisk whose
//!   port is another's is stopped and goes through its ladder.
//! - each half has its own ladder of restarts. While the controller
//!   restarts, Asterisk keeps running: calls in it go on or wait, and the
//!   controller that comes back takes them over. While Asterisk restarts,
//!   the controller keeps running and finds the new Asterisk.
//! - a ladder is exhausted — the other half is stopped too and the launcher
//!   ends with [`GIVEN_UP`]. Either half alone looks alive and serves
//!   nothing.
//! - a stop (SIGTERM, or SIGINT from a terminal) ends Asterisk first and the
//!   controller after Asterisk has exited: the audio connections between
//!   them are opened by Asterisk, and Asterisk must be the one to close
//!   them. Both end by themselves with code 0; so does the launcher.
//! - SIGHUP goes on to the controller: it is how whoever renews the node's
//!   certificate says so.
//!
//! What happens is told on stdout, one JSON object per line, for whoever
//! started the launcher to follow ([`Event`]). The output of both halves
//! goes on to stderr line by line: the controller's as it is (its lines name
//! it), Asterisk's after [`ASTERISK_PREFIX`].
//!
//! The ladders are the launcher's alone: whoever started it does not start
//! it again when it ends, or a half would have two ladders.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{ExitCode, ExitStatus, Stdio},
    time::{Duration, Instant},
};

use http_body_util::Empty;
use hyper::{
    Request,
    body::Bytes,
    client::conn::http1,
    header::{CONNECTION, HOST, SERVER},
};
use hyper_util::rt::TokioIo;
use rustix::process::{Pid, Signal, kill_process};
use serde::Serialize;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    net::TcpStream,
    process::{Child, Command},
    signal::unix::{SignalKind, signal},
    sync::mpsc,
};

use crate::config::Config;

/// The code the launcher ends with when a ladder is exhausted, a half
/// cannot be started, or the controller refused to start: the node is
/// given up, and whoever started the launcher must not start it again on
/// its own — the reason is the last event.
pub const GIVEN_UP: u8 = 2;

/// What every line of Asterisk's output is prefixed with on the launcher's
/// stderr.
pub const ASTERISK_PREFIX: &str = "asterisk: ";

/// The line Asterisk prints on stdout, alone, when its boot has finished.
const ASTERISK_BOOTED: &str = "Asterisk Ready.";

/// The beginning of the controller's ready line on stdout; the address it
/// listens on follows.
const CONTROLLER_READY: &str = "asterisk-controller ready on ";

const USAGE: &str = "usage: asterisk-controller launch --restarts <n> --stable-after <milliseconds> \
    --restart-delay <milliseconds> <the controller's arguments>";

/// The two halves of a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Half {
    /// The Asterisk controller — this program, without `launch`.
    Controller,
    /// Asterisk, from the tree the controller is paired with.
    Asterisk,
}

/// What the launcher tells whoever started it, one per line on stdout.
#[derive(Debug, Serialize)]
#[serde(
    tag = "event",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum Event {
    /// The controller has written Asterisk's configuration and accepts
    /// Asterisk's audio connections.
    ControllerReady {
        /// Its process.
        pid: u32,
        /// Where it accepts Asterisk's audio connections.
        listen: String,
    },
    /// Asterisk has booted and answers on its HTTP port.
    AsteriskReady {
        /// Its process.
        pid: u32,
        /// Its HTTP port.
        http: SocketAddr,
    },
    /// Asterisk has booted, but its HTTP port is not its own; it is stopped.
    AsteriskUnusable {
        /// Its process.
        pid: u32,
        /// What answers on the port instead.
        problem: String,
    },
    /// A half has ended while the node was running.
    Exited {
        /// Which.
        half: Half,
        /// Its process.
        pid: u32,
        /// Whether it had been ready.
        ready: bool,
        /// Its exit code, when it exited.
        code: Option<i32>,
        /// The signal that ended it, when one did.
        signal: Option<i32>,
        /// The last line it wrote before it was ready — the controller's
        /// refusal, when it refused to start.
        last_line: Option<String>,
        /// The restart that follows; none when the ladder is exhausted.
        restart: Option<Restart>,
    },
    /// The node is given up: the other half is stopped, and the launcher
    /// ends with [`GIVEN_UP`].
    GivenUp {
        /// Why, in words.
        reason: String,
    },
    /// A half has ended in a stop: asked for, or the node given up.
    Stopped {
        /// Which.
        half: Half,
        /// Its exit code, when it exited.
        code: Option<i32>,
        /// The signal that ended it, when one did.
        signal: Option<i32>,
    },
}

/// Which restart of a ladder this is.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Restart {
    /// This one, from 1.
    pub attempt: u32,
    /// The most a ladder allows.
    pub limit: u32,
}

/// What the launcher is told.
struct Launch {
    /// The most restarts a half is given before the node is given up.
    restarts: u32,
    /// A half that ran this long has its ladder start over.
    stable_after: Duration,
    /// How long a half waits before it is started again.
    restart_delay: Duration,
    /// The controller's arguments, as they were given.
    controller_arguments: Vec<String>,
    /// The same, read: where the tree, the state and Asterisk's HTTP port
    /// are.
    config: Config,
}

impl Launch {
    /// Read what the launcher is told; the controller's arguments are read
    /// by the controller's own reader, so a launcher never starts a
    /// controller that would refuse them.
    fn from_arguments(arguments: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut restarts = None;
        let mut stable_after = None;
        let mut restart_delay = None;
        let mut controller_arguments = Vec::new();

        let mut arguments = arguments;
        while let Some(name) = arguments.next() {
            let value = arguments
                .next()
                .ok_or_else(|| format!("{name} needs a value\n{USAGE}"))?;
            match name.as_str() {
                "--restarts" => restarts = Some(number(&name, &value)?),
                "--stable-after" => {
                    stable_after = Some(Duration::from_millis(number(&name, &value)?.into()));
                }
                "--restart-delay" => {
                    restart_delay = Some(Duration::from_millis(number(&name, &value)?.into()));
                }
                _ => {
                    controller_arguments.push(name);
                    controller_arguments.push(value);
                }
            }
        }

        let config = Config::from_arguments(controller_arguments.iter().cloned())?;
        Ok(Self {
            restarts: restarts.ok_or_else(|| format!("--restarts is missing\n{USAGE}"))?,
            stable_after: stable_after
                .ok_or_else(|| format!("--stable-after is missing\n{USAGE}"))?,
            restart_delay: restart_delay
                .ok_or_else(|| format!("--restart-delay is missing\n{USAGE}"))?,
            controller_arguments,
            config,
        })
    }
}

/// A whole number of a launcher argument.
fn number(name: &str, value: &str) -> Result<u32, String> {
    value
        .parse()
        .map_err(|_| format!("{name} must be a whole number, not {value:?}\n{USAGE}"))
}

/// Something that happened to the node, in the order it happened.
enum Happening {
    /// A line of a half's stdout.
    Stdout(Half, u64, String),
    /// A line of a half's stderr.
    Stderr(Half, u64, String),
    /// A half's process has ended, after its output did.
    Ended(Half, u64, std::io::Result<ExitStatus>),
    /// What answers on Asterisk's HTTP port, for a booted Asterisk: none
    /// when it is Asterisk.
    Port(u64, Option<String>),
    /// A half's restart delay has passed.
    RestartDue(Half),
    /// A stop is asked for.
    Stop,
    /// The node's certificate was renewed.
    Hangup,
}

/// Where a half's process stands.
#[derive(Default)]
struct Process {
    /// Its process, while it runs.
    pid: Option<u32>,
    /// Which start of this half it is; what belongs to an earlier one is
    /// left alone.
    generation: u64,
    /// When it was started.
    started: Option<Instant>,
    /// How many restarts its ladder has taken.
    restarts: u32,
    /// Whether it is ready.
    ready: bool,
    /// Whether it has been asked to stop.
    signalled: bool,
    /// The last line it wrote on stderr before it was ready.
    last_line: Option<String>,
}

/// How the launcher runs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The node runs; ladders apply.
    Running,
    /// A stop was asked for: both halves end, Asterisk first.
    Stopping,
    /// A ladder is exhausted: both halves end, Asterisk first.
    GivingUp,
}

/// Run the launcher with what follows `launch` on the command line.
pub async fn run(arguments: impl Iterator<Item = String>) -> ExitCode {
    let launch = match Launch::from_arguments(arguments) {
        Ok(launch) => launch,
        Err(problem) => {
            eprintln!("asterisk-controller launch: {problem}");
            return ExitCode::FAILURE;
        }
    };
    let controller = match std::env::current_exe() {
        Ok(controller) => controller,
        Err(error) => {
            eprintln!("asterisk-controller launch: cannot find its own program: {error}");
            return ExitCode::FAILURE;
        }
    };

    let (happenings, mut happened) = mpsc::unbounded_channel();
    if let Err(problem) = listen_for_signals(&happenings) {
        eprintln!("asterisk-controller launch: {problem}");
        return ExitCode::FAILURE;
    }

    let mut node = Node {
        launch,
        controller_program: controller,
        happenings,
        mode: Mode::Running,
        controller: Process::default(),
        asterisk: Process::default(),
        asterisk_launched: false,
    };
    node.start(Half::Controller);

    loop {
        if let Some(code) = node.finished() {
            return code;
        }
        let Some(happening) = happened.recv().await else {
            // The channel ends only when no sender is left, and the node
            // holds one.
            return ExitCode::from(GIVEN_UP);
        };
        node.on(happening);
    }
}

/// Turn the signals whoever runs the node sends into happenings.
fn listen_for_signals(happenings: &mpsc::UnboundedSender<Happening>) -> Result<(), String> {
    let stop: fn() -> Happening = || Happening::Stop;
    let hangup: fn() -> Happening = || Happening::Hangup;
    for (kind, name, happening) in [
        (SignalKind::terminate(), "SIGTERM", stop),
        (SignalKind::interrupt(), "SIGINT", stop),
        (SignalKind::hangup(), "SIGHUP", hangup),
    ] {
        let mut received =
            signal(kind).map_err(|error| format!("cannot listen for {name}: {error}"))?;
        let happenings = happenings.clone();
        tokio::spawn(async move {
            while received.recv().await.is_some() {
                if happenings.send(happening()).is_err() {
                    return;
                }
            }
        });
    }
    Ok(())
}

/// The node as the launcher keeps it.
struct Node {
    launch: Launch,
    controller_program: PathBuf,
    happenings: mpsc::UnboundedSender<Happening>,
    mode: Mode,
    controller: Process,
    asterisk: Process,
    /// Whether Asterisk has been started at all: the controller's first
    /// ready line starts it, later ones do not.
    asterisk_launched: bool,
}

impl Node {
    /// How the launcher ends, once it is ending and neither half runs.
    fn finished(&self) -> Option<ExitCode> {
        if self.controller.pid.is_some() || self.asterisk.pid.is_some() {
            return None;
        }
        match self.mode {
            Mode::Running => None,
            Mode::Stopping => Some(ExitCode::SUCCESS),
            Mode::GivingUp => Some(ExitCode::from(GIVEN_UP)),
        }
    }

    fn process(&mut self, half: Half) -> &mut Process {
        match half {
            Half::Controller => &mut self.controller,
            Half::Asterisk => &mut self.asterisk,
        }
    }

    fn on(&mut self, happening: Happening) {
        match happening {
            Happening::Stdout(half, generation, line) => self.out(half, generation, &line),
            Happening::Stderr(half, generation, line) => {
                match half {
                    Half::Controller => eprintln!("{line}"),
                    Half::Asterisk => eprintln!("{ASTERISK_PREFIX}{line}"),
                }
                let process = self.process(half);
                if process.generation == generation && !process.ready {
                    process.last_line = Some(line);
                }
            }
            Happening::Ended(half, generation, status) => self.ended(half, generation, &status),
            Happening::Port(generation, problem) => self.port(generation, problem),
            Happening::RestartDue(half) => {
                if self.mode == Mode::Running {
                    self.start(half);
                }
            }
            Happening::Stop => {
                if self.mode == Mode::Running {
                    self.mode = Mode::Stopping;
                }
                self.stop_next();
            }
            Happening::Hangup => {
                if let Some(pid) = self.controller.pid {
                    signal_process(pid, Signal::HUP, "the controller");
                }
            }
        }
    }

    fn out(&mut self, half: Half, generation: u64, line: &str) {
        match half {
            Half::Controller => {
                if self.controller.generation != generation {
                    return;
                }
                if let Some(listen) = line.strip_prefix(CONTROLLER_READY) {
                    self.controller.ready = true;
                    self.controller.last_line = None;
                    if let Some(pid) = self.controller.pid {
                        tell(&Event::ControllerReady {
                            pid,
                            listen: listen.to_owned(),
                        });
                    }
                    if !self.asterisk_launched && self.mode == Mode::Running {
                        self.asterisk_launched = true;
                        self.start(Half::Asterisk);
                    }
                } else {
                    eprintln!("{line}");
                }
            }
            Half::Asterisk => {
                eprintln!("{ASTERISK_PREFIX}{line}");
                if self.asterisk.generation == generation
                    && line == ASTERISK_BOOTED
                    && self.mode == Mode::Running
                {
                    let address = self.launch.config.asterisk_http;
                    let happenings = self.happenings.clone();
                    tokio::spawn(async move {
                        let problem = who_answers(address).await;
                        // The node is gone when nobody listens.
                        let _gone = happenings.send(Happening::Port(generation, problem));
                    });
                }
            }
        }
    }

    fn port(&mut self, generation: u64, problem: Option<String>) {
        if self.asterisk.generation != generation || self.mode != Mode::Running {
            return;
        }
        let Some(pid) = self.asterisk.pid else {
            return;
        };
        match problem {
            None => {
                self.asterisk.ready = true;
                self.asterisk.last_line = None;
                tell(&Event::AsteriskReady {
                    pid,
                    http: self.launch.config.asterisk_http,
                });
            }
            Some(problem) => {
                tell(&Event::AsteriskUnusable { pid, problem });
                signal_process(pid, Signal::TERM, "Asterisk");
            }
        }
    }

    fn ended(&mut self, half: Half, generation: u64, status: &std::io::Result<ExitStatus>) {
        let limit = self.launch.restarts;
        let stable_after = self.launch.stable_after;
        let mode = self.mode;
        let process = self.process(half);
        if process.generation != generation {
            return;
        }
        let Some(pid) = process.pid.take() else {
            return;
        };
        let (code, signal) = match status {
            Ok(status) => {
                use std::os::unix::process::ExitStatusExt;
                (status.code(), status.signal())
            }
            Err(error) => {
                eprintln!("asterisk-controller launch: cannot wait for process {pid}: {error}");
                (None, None)
            }
        };
        let ready = std::mem::take(&mut process.ready);
        let last_line = process.last_line.take();

        if mode != Mode::Running {
            tell(&Event::Stopped { half, code, signal });
            self.stop_next();
            return;
        }

        if process
            .started
            .is_some_and(|started| started.elapsed() >= stable_after)
        {
            process.restarts = 0;
        }
        process.restarts += 1;
        let attempt = process.restarts;
        let restart = (attempt <= limit).then_some(Restart { attempt, limit });
        tell(&Event::Exited {
            half,
            pid,
            ready,
            code,
            signal,
            last_line,
            restart,
        });

        if restart.is_some() {
            let delay = self.launch.restart_delay;
            let happenings = self.happenings.clone();
            tokio::spawn(async move {
                // The delay of a ladder: a half that fails at once is not
                // started again at once.
                tokio::time::sleep(delay).await;
                let _gone = happenings.send(Happening::RestartDue(half));
            });
        } else {
            let other = match half {
                Half::Controller => "Asterisk",
                Half::Asterisk => "the controller",
            };
            self.give_up(format!(
                "{} exhausted its {limit} restarts; {other} is stopped too",
                name_of(half)
            ));
        }
    }

    fn give_up(&mut self, reason: String) {
        if self.mode == Mode::Running {
            self.mode = Mode::GivingUp;
            tell(&Event::GivenUp { reason });
        }
        self.stop_next();
    }

    /// Ask the next half to stop: Asterisk while it runs, the controller
    /// once Asterisk has exited.
    fn stop_next(&mut self) {
        for half in [Half::Asterisk, Half::Controller] {
            let process = self.process(half);
            if let Some(pid) = process.pid {
                if !process.signalled {
                    process.signalled = true;
                    signal_process(pid, Signal::TERM, name_of(half));
                }
                return;
            }
        }
    }

    fn start(&mut self, half: Half) {
        let mut command = match half {
            Half::Controller => {
                let mut command = Command::new(&self.controller_program);
                command.args(&self.launch.controller_arguments);
                command
            }
            Half::Asterisk => asterisk_command(&self.launch.config),
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let happenings = self.happenings.clone();
        let process = self.process(half);
        process.generation += 1;
        process.ready = false;
        process.signalled = false;
        process.last_line = None;
        let generation = process.generation;

        match command.spawn() {
            Ok(child) => {
                process.pid = child.id();
                process.started = Some(Instant::now());
                tokio::spawn(follow(child, half, generation, happenings));
            }
            Err(error) => {
                process.pid = None;
                self.give_up(format!("{} cannot be started: {error}", name_of(half)));
            }
        }
    }
}

/// How a half is named in words.
const fn name_of(half: Half) -> &'static str {
    match half {
        Half::Controller => "the controller",
        Half::Asterisk => "Asterisk",
    }
}

/// How Asterisk is started: on the controller's main file and nothing
/// else, in the foreground, without colour codes.
fn asterisk_command(config: &Config) -> Command {
    let mut command = Command::new(config.asterisk_tree.join("sbin").join("asterisk"));
    command
        .arg("-C")
        .arg(config.state.join("etc").join("asterisk.conf"))
        .arg("-f")
        .arg("-n");
    // On Linux the tree's libraries are found through LD_LIBRARY_PATH, ahead
    // of what the caller set; on macOS the tree finds them by itself.
    if cfg!(target_os = "linux") {
        command.env("LD_LIBRARY_PATH", library_path(&config.asterisk_tree));
    }
    command
}

/// The tree's libraries ahead of the library path whoever runs the node set.
fn library_path(tree: &Path) -> std::ffi::OsString {
    let mut path = tree.join("lib").into_os_string();
    if let Some(set) = std::env::var_os("LD_LIBRARY_PATH").filter(|set| !set.is_empty()) {
        path.push(":");
        path.push(set);
    }
    path
}

/// Pass a half's output on, then tell that it ended — after its output, so
/// the last line it wrote is known when its end is.
async fn follow(
    mut child: Child,
    half: Half,
    generation: u64,
    happenings: mpsc::UnboundedSender<Happening>,
) {
    let out = child.stdout.take().map(|stream| {
        tokio::spawn(lines(stream, happenings.clone(), move |line| {
            Happening::Stdout(half, generation, line)
        }))
    });
    let err = child.stderr.take().map(|stream| {
        tokio::spawn(lines(stream, happenings.clone(), move |line| {
            Happening::Stderr(half, generation, line)
        }))
    });
    for reader in [out, err].into_iter().flatten() {
        if let Err(error) = reader.await {
            eprintln!(
                "asterisk-controller launch: reading {} failed: {error}",
                name_of(half)
            );
        }
    }
    let status = child.wait().await;
    let _gone = happenings.send(Happening::Ended(half, generation, status));
}

/// Every line of a stream, as a happening.
async fn lines(
    stream: impl AsyncRead + Unpin,
    happenings: mpsc::UnboundedSender<Happening>,
    happening: impl Fn(String) -> Happening,
) {
    let mut lines = BufReader::new(stream).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if happenings.send(happening(line)).is_err() {
                    return;
                }
            }
            Ok(None) => return,
            Err(error) => {
                eprintln!("asterisk-controller launch: a line could not be read: {error}");
                return;
            }
        }
    }
}

/// Ask Asterisk's HTTP port who is there: none when Asterisk answers,
/// otherwise what was found instead.
async fn who_answers(address: SocketAddr) -> Option<String> {
    let not_asterisk = |error: &dyn std::fmt::Display| {
        format!("no HTTP answer on {address}, so Asterisk does not own it ({error})")
    };
    let stream = match TcpStream::connect(address).await {
        Ok(stream) => stream,
        Err(error) => return Some(not_asterisk(&error)),
    };
    let (mut sender, connection) = match http1::handshake(TokioIo::new(stream)).await {
        Ok(handshake) => handshake,
        Err(error) => return Some(not_asterisk(&error)),
    };
    // The connection is driven beside the request and ends with it.
    tokio::spawn(connection);
    let request = match Request::builder()
        .method("GET")
        .uri("/")
        .header(HOST, address.to_string())
        .header(CONNECTION, "close")
        .body(Empty::<Bytes>::new())
    {
        Ok(request) => request,
        Err(error) => return Some(not_asterisk(&error)),
    };
    let response = match sender.send_request(request).await {
        Ok(response) => response,
        Err(error) => return Some(not_asterisk(&error)),
    };
    let server = response
        .headers()
        .get(SERVER)
        .and_then(|server| server.to_str().ok());
    match server {
        Some(server) if server.starts_with("Asterisk") => None,
        Some(server) => Some(format!(
            "{address} is held by another process (it answers as {server:?})"
        )),
        None => Some(format!(
            "{address} is held by another process (it answers as an unnamed server)"
        )),
    }
}

/// Tell whoever started the launcher what happened.
fn tell(event: &Event) {
    match serde_json::to_string(event) {
        Ok(line) => println!("{line}"),
        Err(error) => {
            eprintln!("asterisk-controller launch: an event could not be written: {error}");
        }
    }
}

/// Send a signal to a half.
fn signal_process(pid: u32, signal: Signal, whom: &str) {
    let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
        eprintln!("asterisk-controller launch: {whom} has no process to signal ({pid})");
        return;
    };
    if let Err(error) = kill_process(pid, signal) {
        eprintln!("asterisk-controller launch: {whom} could not be signalled: {error}");
    }
}
