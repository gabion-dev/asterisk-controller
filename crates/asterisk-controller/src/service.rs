// crates/asterisk-controller/src/service.rs

//! The service connection: the node's one standing connection to the
//! application.
//!
//! The controller opens it to `<application>/service` and keeps it open for
//! as long as it runs, to whichever instance the address leads to; lost, it
//! is opened again. On it the node says who it is and which settings it runs
//! on, and the application gives the node its settings — whole, in the
//! message itself, never as a signal to fetch them elsewhere: one channel
//! and one order, so "settings changed" can never arrive without the
//! settings.
//!
//! Settings are applied while calls go on. The files of Asterisk's
//! configuration that follow from settings are replaced and the modules
//! that read them are reloaded; the node answers with the fingerprint of
//! what it applied, or refuses with the reason and keeps what it had. What
//! Asterisk will not take is taken back: the node never runs on settings
//! that are half in force.

use std::{collections::HashSet, num::NonZeroU64, sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use node_protocol::messages::{ApplicationServiceMessage, NodeMessage, Request, Settings};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{
    Message as Frame,
    protocol::{CloseFrame, frame::coding::CloseCode},
};

use crate::{
    application::{Endpoint, PING_EVERY, SILENCE_LIMIT, Socket},
    asterisk::Asterisk,
    asterisk_files::{self, FROM_SETTINGS},
    config::PROTOCOL_VERSION,
    node::Node,
    prompts,
    settings::{self, Applied},
};

/// How long the controller waits before it opens the service connection
/// again. Until an instance listens there is no event to wait for: its
/// listening is the state, and this is how often the state is read.
const OPEN_AGAIN: Duration = Duration::from_secs(1);

/// The longest problem a refusal of settings may carry.
const PROBLEM_LENGTH: usize = 2000;

/// Keep the service connection open for as long as the controller runs.
pub async fn run(node: Arc<Node>, asterisk: Arc<Asterisk>) {
    let mut said_why = String::new();
    loop {
        let why = match Service::open(&node).await {
            Ok(mut service) => {
                said_why.clear();
                let why = service.serve(&asterisk).await;
                service.close(&why).await;
                why
            }
            Err(why) => why,
        };
        // Said once, not every second, while it stays the same.
        if why != said_why {
            eprintln!("asterisk-controller: no service connection — {why}");
            said_why = why;
        }
        tokio::time::sleep(OPEN_AGAIN).await;
    }
}

/// One service connection, from its hello to its loss.
struct Service<'a> {
    node: &'a Arc<Node>,
    socket: Socket,
    /// When the application was last heard.
    heard_at: Instant,
    /// When the next ping is due.
    ping_at: Instant,
    /// How many pings have been sent: numbers the next one.
    pings: u64,
}

impl<'a> Service<'a> {
    /// Open the connection, say who the node is, and wait to be welcomed.
    ///
    /// # Errors
    ///
    /// The connection cannot be opened, or the application refuses the node.
    async fn open(node: &'a Arc<Node>) -> Result<Self, String> {
        let socket = node.application.open(Endpoint::Service).await?;
        let now = Instant::now();
        let mut service = Self {
            node,
            socket,
            heard_at: now,
            ping_at: now + PING_EVERY,
            pings: 0,
        };
        let hello = service.hello()?;
        service.tell(&hello).await?;
        loop {
            match service.hear().await? {
                ApplicationServiceMessage::Welcome => {
                    eprintln!("asterisk-controller: the application welcomed this node");
                    // Prompts the node should have and has not — its state
                    // directory lost them — are fetched while it can be.
                    let applied = node.applied();
                    if let Err(problem) = prompts::fetch_missing(
                        &node.config.state,
                        &node.application,
                        &applied.settings,
                    )
                    .await
                    {
                        eprintln!("asterisk-controller: {problem}");
                    }
                    return Ok(service);
                }
                ApplicationServiceMessage::Refuse { reason } => {
                    return Err(format!("the application refused this node: {reason}"));
                }
                ApplicationServiceMessage::Ping { n } => {
                    service.tell(&NodeMessage::Pong { n }).await?;
                }
                ApplicationServiceMessage::Pong { .. } => {}
                ApplicationServiceMessage::Settings { .. }
                | ApplicationServiceMessage::ReportReceived { .. }
                | ApplicationServiceMessage::Request { .. } => {
                    return Err("the application spoke before it welcomed the node".into());
                }
            }
        }
    }

    /// The first message: the node, its versions and the settings it runs on.
    fn hello(&self) -> Result<NodeMessage, String> {
        let invalid = |what: &str| format!("the controller's own {what} is not valid");
        let applied = self.node.applied();
        Ok(NodeMessage::Hello {
            protocol: NonZeroU64::new(PROTOCOL_VERSION)
                .ok_or_else(|| invalid("protocol version"))?,
            node: self
                .node
                .config
                .node
                .parse()
                .map_err(|_| invalid("node name"))?,
            controller_version: env!("CARGO_PKG_VERSION")
                .parse()
                .map_err(|_| invalid("version"))?,
            asterisk_version: self
                .node
                .asterisk_version
                .parse()
                .map_err(|_| invalid("Asterisk version"))?,
            applied_settings: applied
                .fingerprint
                .as_deref()
                .map(str::parse)
                .transpose()
                .map_err(|_| invalid("settings fingerprint"))?,
        })
    }

    /// Serve the connection until it is lost. Returns why it ended.
    ///
    /// Reports the node kept are sent at once, those from before this
    /// connection first; each is sent once on a connection, and forgotten
    /// when the application says it has it.
    async fn serve(&mut self, asterisk: &Asterisk) -> String {
        let node = self.node;
        let mut sent = HashSet::new();
        loop {
            if let Err(why) = self.send_reports(&mut sent).await {
                return why;
            }
            let heard = tokio::select! {
                heard = self.hear() => match heard {
                    Ok(heard) => heard,
                    Err(why) => return why,
                },
                () = node.reports.changed() => continue,
            };
            let answered = match heard {
                ApplicationServiceMessage::Settings { settings } => {
                    match apply(self.node, asterisk, settings).await {
                        Ok(answer) => self.tell(&answer).await,
                        Err(why) => Err(why),
                    }
                }
                ApplicationServiceMessage::Ping { n } => self.tell(&NodeMessage::Pong { n }).await,
                ApplicationServiceMessage::Pong { .. } => Ok(()),
                ApplicationServiceMessage::Welcome | ApplicationServiceMessage::Refuse { .. } => {
                    Err("the application welcomed or refused the node a second time".into())
                }
                ApplicationServiceMessage::ReportReceived { id } => {
                    sent.remove(id.as_str());
                    node.reports.forget(id.as_str())
                }
                // A request this build cannot carry out ends the connection
                // with its name: none is accepted and ignored.
                ApplicationServiceMessage::Request { request, .. } => Err(format!(
                    "not implemented in this build: {}",
                    match request {
                        Request::StartConversation { .. } => "start_conversation",
                        Request::BrowserLegBegin { .. } => "browser_leg_begin",
                        Request::BrowserLegEnd { .. } => "browser_leg_end",
                        Request::DeviceFact { .. } => "device_fact",
                    }
                )),
            };
            if let Err(why) = answered {
                return why;
            }
        }
    }

    /// Send the reports kept and not yet sent on this connection.
    ///
    /// # Errors
    ///
    /// The journal cannot be read, or the connection is lost.
    async fn send_reports(&mut self, sent: &mut HashSet<String>) -> Result<(), String> {
        for (id, report) in self.node.reports.kept()? {
            if sent.insert(id) {
                self.tell(&report).await?;
            }
        }
        Ok(())
    }

    /// The application's next message, under the protocol's liveness: a
    /// ping goes out at least every [`PING_EVERY`], and [`SILENCE_LIMIT`]
    /// of silence is a connection lost.
    ///
    /// # Errors
    ///
    /// The connection is lost, or the application broke the protocol.
    async fn hear(&mut self) -> Result<ApplicationServiceMessage, String> {
        loop {
            let due = self.ping_at.min(self.heard_at + SILENCE_LIMIT);
            let frame = tokio::select! {
                frame = self.socket.next() => frame,
                () = tokio::time::sleep_until(due) => {
                    let now = Instant::now();
                    if now >= self.heard_at + SILENCE_LIMIT {
                        return Err(format!(
                            "heard nothing from the application for {} seconds",
                            SILENCE_LIMIT.as_secs()
                        ));
                    }
                    self.pings += 1;
                    self.ping_at = now + PING_EVERY;
                    let n = self.pings;
                    self.tell(&NodeMessage::Ping { n }).await?;
                    continue;
                }
            };
            self.heard_at = Instant::now();
            match frame {
                Some(Ok(Frame::Text(text))) => {
                    return node_protocol::decode(text.as_str())
                        .map_err(|error| format!("protocol: {error}"));
                }
                Some(Ok(Frame::Binary(_))) => {
                    return Err("protocol: the service connection carries no binary frames".into());
                }
                Some(Ok(Frame::Ping(_) | Frame::Pong(_) | Frame::Frame(_))) => {}
                Some(Ok(Frame::Close(_))) | None => {
                    return Err("the application closed the service connection".into());
                }
                Some(Err(error)) => return Err(format!("the service connection: {error}")),
            }
        }
    }

    async fn tell(&mut self, message: &NodeMessage) -> Result<(), String> {
        let text = node_protocol::encode(message).map_err(|error| format!("protocol: {error}"))?;
        self.socket
            .send(Frame::text(text))
            .await
            .map_err(|error| format!("the service connection: {error}"))
    }

    /// Close the connection, saying why.
    async fn close(&mut self, why: &str) {
        let close = CloseFrame {
            code: CloseCode::Error,
            reason: why.chars().take(100).collect::<String>().into(),
        };
        let _ = self.socket.close(Some(close)).await;
    }
}

/// Apply settings the application gave, while calls go on, and say what
/// came of it.
///
/// # Errors
///
/// The node cannot put its own answer into the protocol — a defect of the
/// controller, which ends the connection rather than answer wrongly.
async fn apply(
    node: &Node,
    asterisk: &Asterisk,
    settings: Settings,
) -> Result<NodeMessage, String> {
    let fingerprint = node_protocol::fingerprint(&settings)
        .map_err(|error| format!("protocol: the settings have no fingerprint: {error}"))?;
    let told = fingerprint
        .parse()
        .map_err(|_| "the node's own fingerprint is not valid".to_owned())?;
    if node.applied().fingerprint.as_deref() == Some(fingerprint.as_str()) {
        return Ok(NodeMessage::SettingsApplied { fingerprint: told });
    }
    if let Err(problem) = take_in(node, asterisk, &settings).await {
        eprintln!("asterisk-controller: settings {fingerprint} are refused — {problem}");
        let problem: String = problem.chars().take(PROBLEM_LENGTH).collect();
        return Ok(NodeMessage::SettingsRefused {
            fingerprint: told,
            problem: problem
                .parse()
                .map_err(|_| "the node's own refusal is not valid".to_owned())?,
        });
    }
    eprintln!(
        "asterisk-controller: settings {fingerprint} are applied: {} operators, {} lines, \
         {} entries",
        settings.operators.len(),
        settings.lines.len(),
        settings.entries.len(),
    );
    prompts::forget_unnamed(&node.config.state, &settings);
    node.apply(Applied {
        settings,
        fingerprint: Some(fingerprint),
    });
    Ok(NodeMessage::SettingsApplied { fingerprint: told })
}

/// Make settings Asterisk's and the node's: judge them, put the files that
/// follow from them in place, have Asterisk reload what changed, and keep
/// them for the node's next start. What Asterisk refuses is taken back.
///
/// # Errors
///
/// The settings are refused, or Asterisk would not take them; the reason,
/// in words.
async fn take_in(node: &Node, asterisk: &Asterisk, settings: &Settings) -> Result<(), String> {
    settings::check(settings)?;
    let config = &node.config;
    // Settings are not applied until every prompt they name is here.
    prompts::fetch_missing(&config.state, &node.application, settings).await?;
    let new = asterisk_files::from_settings(config, settings)?;
    let mut changed = Vec::new();
    for ((name, module), content) in FROM_SETTINGS.iter().zip(new) {
        let before = asterisk_files::current(config, name)?;
        if before != content {
            changed.push((*name, *module, before, content));
        }
    }

    let mut done = Vec::new();
    let mut failure = None;
    for (name, module, before, content) in &changed {
        let step = async {
            asterisk_files::replace(config, name, content)?;
            done.push((*name, *module, before.as_str()));
            asterisk.reload(module).await
        };
        if let Err(problem) = step.await {
            failure = Some(problem);
            break;
        }
    }
    if failure.is_none()
        && let Err(problem) = settings::store(&config.state, settings)
    {
        failure = Some(problem);
    }
    let Some(problem) = failure else {
        return Ok(());
    };

    // Taken back: the files as they were, read again by Asterisk.
    for (name, module, before) in done {
        let restored = async {
            asterisk_files::replace(config, name, before)?;
            asterisk.reload(module).await
        };
        if let Err(also) = restored.await {
            eprintln!(
                "asterisk-controller: {name} could not be taken back after a refused change of \
                 settings — {also}; Asterisk may run on part of the refused settings until the \
                 controller starts again"
            );
        }
    }
    Err(problem)
}
