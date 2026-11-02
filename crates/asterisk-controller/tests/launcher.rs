// crates/asterisk-controller/tests/launcher.rs

//! The launcher of a node against a real Asterisk.
//!
//! The launcher is the built binary with `launch`; Asterisk is the build the
//! controller is pinned to, started by the launcher from its tree; the
//! controller is the launcher's own program. Nothing is imitated but the
//! application, and here not even that: no application listens, which the
//! controller survives by trying again — the launcher's rule does not depend
//! on it.
//!
//! What is held: the controller first and Asterisk on its ready line; a half
//! killed is started again while the other one goes on; SIGHUP reaches the
//! controller; a stop ends Asterisk first and the controller after it; an
//! exhausted ladder stops the other half too and ends the launcher with its
//! own code; an Asterisk whose HTTP port is another's is not called ready;
//! arguments the launcher or the controller would refuse start nothing.
//!
//! The tree is taken from `ASTERISK_TREE` or from `target/asterisk-server`,
//! where `scripts/fetch-asterisk.sh` puts it. Without a tree the test fails
//! and says how to get one.

use std::{
    error::Error,
    fs,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    process::{Child, Command},
    sync::{Mutex, mpsc},
    time::timeout,
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// How long any single step may take before the test gives up on it — a
/// limit of the test, so a failure is reported instead of hanging.
const STEP: Duration = Duration::from_secs(15);

/// The code the launcher ends with when the node is given up.
const GIVEN_UP: i32 = 2;

/// The Asterisk tree the controller is paired with.
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

/// A port nothing listens on right now, as the system offers one.
async fn free_port() -> TestResult<u16> {
    Ok(TcpListener::bind("127.0.0.1:0").await?.local_addr()?.port())
}

/// One node's directories and ports, apart from every other node's.
struct Place {
    state: PathBuf,
    run: PathBuf,
    listen: u16,
    http: u16,
    audio: (u16, u16),
    application: u16,
}

impl Place {
    /// Directories in the system's temporary directory — short enough for
    /// Asterisk's console socket — with the node's secret in place.
    async fn new(name: &str) -> TestResult<Self> {
        let base = std::env::temp_dir().join(format!("launcher-{}-{name}", std::process::id()));
        if base.exists() {
            fs::remove_dir_all(&base)?;
        }
        let state = base.join("state");
        let run = base.join("run");
        fs::create_dir_all(&state)?;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(state.join("node-secret"))?
            .write_all_sync(b"the secret of the launched node")?;
        // Audio ports are taken from one the system offers: a pair range of
        // four, which is all a node that carries no call needs.
        let first = free_port().await? & !1;
        Ok(Self {
            state,
            run,
            listen: free_port().await?,
            http: free_port().await?,
            audio: (first, first + 3),
            application: free_port().await?,
        })
    }

    /// The launcher's arguments: its ladder, then the controller's.
    fn arguments(&self, tree: &Path, restarts: u32) -> Vec<String> {
        vec![
            "launch".into(),
            "--restarts".into(),
            restarts.to_string(),
            "--stable-after".into(),
            "3600000".into(),
            "--restart-delay".into(),
            "200".into(),
            "--node".into(),
            "launched".into(),
            "--listen".into(),
            format!("127.0.0.1:{}", self.listen),
            "--application".into(),
            format!("ws://127.0.0.1:{}/node", self.application),
            "--asterisk-tree".into(),
            tree.display().to_string(),
            "--state".into(),
            self.state.display().to_string(),
            "--run".into(),
            self.run.display().to_string(),
            "--asterisk-http".into(),
            format!("127.0.0.1:{}", self.http),
            "--audio-ports".into(),
            format!("{}-{}", self.audio.0, self.audio.1),
        ]
    }
}

/// `write_all` on a file opened with the standard library.
trait WriteAllSync {
    fn write_all_sync(self, bytes: &[u8]) -> std::io::Result<()>;
}

impl WriteAllSync for fs::File {
    fn write_all_sync(mut self, bytes: &[u8]) -> std::io::Result<()> {
        std::io::Write::write_all(&mut self, bytes)
    }
}

/// A running launcher: its events as they come, and its stderr kept.
struct Launcher {
    child: Child,
    events: mpsc::UnboundedReceiver<Value>,
    stderr: Arc<Mutex<Vec<String>>>,
}

impl Launcher {
    fn start(arguments: &[String]) -> TestResult<Self> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_asterisk-controller"))
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let stderr = child.stderr.take().ok_or("no stderr")?;

        let (sender, events) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let event = read_event(&line)
                    .unwrap_or_else(|_| Value::String(format!("NOT AN EVENT: {line}")));
                if sender.send(event).is_err() {
                    return;
                }
            }
        });
        let kept = Arc::new(Mutex::new(Vec::new()));
        let into = Arc::clone(&kept);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                into.lock().await.push(line);
            }
        });
        Ok(Self {
            child,
            events,
            stderr: kept,
        })
    }

    /// The next event, which must be `name`.
    async fn next(&mut self, name: &str) -> TestResult<Value> {
        let event = match timeout(STEP, self.events.recv()).await {
            Ok(Some(event)) => event,
            Ok(None) => return Err(self.failed(&format!("ended before {name}")).await),
            Err(_) => return Err(self.failed(&format!("no {name} within a step")).await),
        };
        if at(&event, "event") == name {
            Ok(event)
        } else {
            Err(self.failed(&format!("{name} expected, {event} came")).await)
        }
    }

    /// Wait until stderr has a line containing `words`.
    async fn comes_to_say(&self, words: &str) -> bool {
        timeout(STEP, async {
            while !self
                .stderr
                .lock()
                .await
                .iter()
                .any(|line| line.contains(words))
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .is_ok()
    }

    /// The launcher's exit code, once it ends.
    async fn ends(&mut self) -> TestResult<Option<i32>> {
        match timeout(STEP, self.child.wait()).await {
            Ok(status) => Ok(status?.code()),
            Err(_) => Err(self.failed("did not end within a step").await),
        }
    }

    /// A failure, with what the launcher wrote on stderr.
    async fn failed(&self, what: &str) -> Box<dyn Error> {
        let stderr = self.stderr.lock().await.join("\n");
        format!("the launcher {what}; its stderr:\n{stderr}").into()
    }

    fn signal(&self, signal: &str) -> TestResult {
        let pid = self.child.id().ok_or("the launcher has no process")?;
        send(signal, pid)
    }
}

/// Send a signal with the `kill` command, which every machine of the lab has.
fn send(signal: &str, pid: u32) -> TestResult {
    let sent = std::process::Command::new("kill")
        .args([format!("-{signal}"), pid.to_string()])
        .status()?;
    if sent.success() {
        Ok(())
    } else {
        Err(format!("kill -{signal} {pid}: {sent}").into())
    }
}

/// Whether a process exists.
fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0".to_owned(), pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Read a line of the launcher's stdout.
#[expect(
    clippy::disallowed_methods,
    reason = "the launcher's own events, not a protocol message"
)]
fn read_event(line: &str) -> serde_json::Result<Value> {
    serde_json::from_str(line)
}

/// A field of an event; null when it has none.
fn at<'a>(event: &'a Value, field: &str) -> &'a Value {
    event.get(field).unwrap_or(&Value::Null)
}

/// The `pid` of an event.
fn pid_of(event: &Value) -> TestResult<u32> {
    let pid = at(event, "pid").as_u64().ok_or("an event without a pid")?;
    Ok(u32::try_from(pid)?)
}

/// What the server on a port calls itself.
async fn server_on(port: u16) -> TestResult<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    stream
        .write_all(b"GET / HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")
        .await?;
    let mut answer = String::new();
    stream.read_to_string(&mut answer).await?;
    answer
        .lines()
        .find_map(|line| line.strip_prefix("Server: "))
        .map(str::to_owned)
        .ok_or_else(|| format!("no Server header in {answer:?}").into())
}

#[tokio::test]
async fn the_launcher_keeps_the_node_by_its_rule() -> TestResult {
    let tree = asterisk_tree()?;

    keeps_both_halves_and_stops_asterisk_first(&tree).await?;
    an_exhausted_ladder_stops_the_other_half(&tree).await?;
    asterisk_on_another_port_is_not_ready(&tree).await?;
    refused_arguments_start_nothing(&tree).await?;
    Ok(())
}

/// Ready in order; each half started again alone; SIGHUP passed on; a stop
/// ends Asterisk first.
async fn keeps_both_halves_and_stops_asterisk_first(tree: &Path) -> TestResult {
    let place = Place::new("keeps").await?;
    let mut launcher = Launcher::start(&place.arguments(tree, 2))?;

    let controller = pid_of(&launcher.next("controller-ready").await?)?;
    let asterisk = pid_of(&launcher.next("asterisk-ready").await?)?;
    if !server_on(place.http).await?.starts_with("Asterisk") {
        return Err("Asterisk does not answer on its port".into());
    }

    // The controller dies; Asterisk goes on, and is not started again.
    send("KILL", controller)?;
    let exited = launcher.next("exited").await?;
    if at(&exited, "half") != "controller" || at(at(&exited, "restart"), "attempt") != 1 {
        return Err(format!("the controller's restart expected: {exited}").into());
    }
    let controller = pid_of(&launcher.next("controller-ready").await?)?;
    if !alive(asterisk) {
        return Err("Asterisk did not survive the controller's death".into());
    }

    // Asterisk dies; the controller goes on.
    send("KILL", asterisk)?;
    let exited = launcher.next("exited").await?;
    if at(&exited, "half") != "asterisk" || at(at(&exited, "restart"), "attempt") != 1 {
        return Err(format!("Asterisk's restart expected: {exited}").into());
    }
    let asterisk = pid_of(&launcher.next("asterisk-ready").await?)?;
    if !alive(controller) {
        return Err("the controller did not survive Asterisk's death".into());
    }

    // SIGHUP reaches the controller.
    launcher.signal("HUP")?;
    if !launcher.comes_to_say("asterisk-controller: SIGHUP").await {
        return Err(launcher.failed("passed no SIGHUP on").await);
    }

    // A stop: Asterisk first, the controller after it, all with code 0.
    launcher.signal("TERM")?;
    let first = launcher.next("stopped").await?;
    let second = launcher.next("stopped").await?;
    if at(&first, "half") != "asterisk" || at(&second, "half") != "controller" {
        return Err(format!("Asterisk is to stop first: {first}, then {second}").into());
    }
    if at(&first, "code") != 0 || at(&second, "code") != 0 {
        return Err(format!("both are to end by themselves: {first}, {second}").into());
    }
    if launcher.ends().await? != Some(0) {
        return Err(launcher.failed("did not end with code 0 on a stop").await);
    }
    if alive(asterisk) || alive(controller) {
        return Err("a half outlived its launcher".into());
    }
    Ok(())
}

/// No restarts left: the controller's death stops Asterisk too.
async fn an_exhausted_ladder_stops_the_other_half(tree: &Path) -> TestResult {
    let place = Place::new("exhausted").await?;
    let mut launcher = Launcher::start(&place.arguments(tree, 0))?;

    let controller = pid_of(&launcher.next("controller-ready").await?)?;
    let asterisk = pid_of(&launcher.next("asterisk-ready").await?)?;

    send("KILL", controller)?;
    let exited = launcher.next("exited").await?;
    if at(&exited, "half") != "controller" || !at(&exited, "restart").is_null() {
        return Err(format!("the controller without a restart expected: {exited}").into());
    }
    launcher.next("given-up").await?;
    let stopped = launcher.next("stopped").await?;
    if at(&stopped, "half") != "asterisk" || at(&stopped, "code") != 0 {
        return Err(format!("Asterisk is to end by itself: {stopped}").into());
    }
    if launcher.ends().await? != Some(GIVEN_UP) {
        return Err(launcher
            .failed("did not end with the code of a node given up")
            .await);
    }
    if alive(asterisk) {
        return Err("Asterisk outlived a node given up".into());
    }
    Ok(())
}

/// Another server holds Asterisk's HTTP port: Asterisk boots, is not ready,
/// and is stopped.
async fn asterisk_on_another_port_is_not_ready(tree: &Path) -> TestResult {
    let place = Place::new("taken").await?;
    let holder = TcpListener::bind(("127.0.0.1", place.http)).await?;
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = holder.accept().await {
            let mut request = [0_u8; 1024];
            let _read = stream.read(&mut request).await;
            let _written = stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nServer: Another\r\nContent-Length: 0\r\n\
                      Connection: close\r\n\r\n",
                )
                .await;
        }
    });
    let mut launcher = Launcher::start(&place.arguments(tree, 0))?;

    launcher.next("controller-ready").await?;
    let unusable = launcher.next("asterisk-unusable").await?;
    if !at(&unusable, "problem")
        .as_str()
        .is_some_and(|problem| problem.contains("Another"))
    {
        return Err(format!("the server on the port is to be named: {unusable}").into());
    }
    let exited = launcher.next("exited").await?;
    if at(&exited, "half") != "asterisk" || at(&exited, "ready") != false {
        return Err(format!("Asterisk, never ready, expected: {exited}").into());
    }
    launcher.next("given-up").await?;
    let stopped = launcher.next("stopped").await?;
    if at(&stopped, "half") != "controller" {
        return Err(format!("the controller is to be stopped: {stopped}").into());
    }
    if launcher.ends().await? != Some(GIVEN_UP) {
        return Err(launcher
            .failed("did not end with the code of a node given up")
            .await);
    }
    Ok(())
}

/// A launcher argument missing, or a controller argument the controller
/// would refuse: code 1, the reason, and nothing started.
async fn refused_arguments_start_nothing(tree: &Path) -> TestResult {
    let place = Place::new("refused").await?;

    let mut without_ladder = place.arguments(tree, 1);
    without_ladder.drain(1..3);
    let mut launcher = Launcher::start(&without_ladder)?;
    if launcher.ends().await? != Some(1) || !launcher.comes_to_say("--restarts is missing").await {
        return Err(launcher.failed("started without its ladder").await);
    }

    let mut controller_refuses = place.arguments(tree, 1);
    controller_refuses.push("--unknown".into());
    controller_refuses.push("value".into());
    let mut launcher = Launcher::start(&controller_refuses)?;
    if launcher.ends().await? != Some(1) || !launcher.comes_to_say("usage:").await {
        return Err(launcher
            .failed("started on arguments the controller refuses")
            .await);
    }
    if launcher.events.recv().await.is_some() {
        return Err("a refused launcher told of a half".into());
    }
    Ok(())
}
