// crates/asterisk-controller/src/asterisk.rs

//! The controller's way to Asterisk.
//!
//! The controller is a client of Asterisk's control interface on the
//! loopback address. It keeps one connection open, on which Asterisk says
//! what happens to every channel of the node's application, and it makes
//! its requests as separate HTTP requests.
//!
//! The connection is opened by the controller, not by Asterisk, and that is
//! what makes a node without a controller behave. To Asterisk an application
//! exists while someone is connected for it. A call that arrives while
//! nobody is fails to enter at once, and goes on through the dialplan the
//! controller wrote: a short wait for the controller to return, then the
//! fallback of the entry — carried out by Asterisk alone. Calls already in
//! the application stay as they are: people connected to each other go on
//! talking.
//!
//! Asterisk can also open control connections itself — one for each call,
//! or one for the node — and the controller was built on each in turn. A
//! call's own connection is closed by Asterisk a few seconds after the
//! channel it was opened for leaves, whoever else is still in the call. A
//! node's connection opened by Asterisk keeps the application in existence
//! while the controller is away, so a call that arrives then waits without
//! end. And while Asterisk drops a connection it opened, an event it writes
//! to it can crash it (22.11.0).
//!
//! Requests of one conversation are made one after another, in the order
//! they were asked for: "create a bridge" must reach Asterisk before "put a
//! channel into it". Requests of different conversations do not wait for
//! each other.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use data_encoding::BASE64;
use futures_util::StreamExt;
use http_body_util::{BodyExt, Full};
use hyper::{
    Request,
    body::Bytes,
    client::conn::http1,
    header::{AUTHORIZATION, CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HOST},
};
use hyper_util::rt::TokioIo;
use tokio::{net::TcpStream, sync::mpsc};
use tokio_tungstenite::{connect_async, tungstenite::Message as Frame};

use crate::{
    ari,
    asterisk_files::{APPLICATION, CONTROL_USER},
    conversation,
    node::Node,
};

/// How long the controller waits before it looks for Asterisk again. Until
/// Asterisk listens there is no event to wait for: its listening is the
/// state, and this is how often the state is read.
const LOOK_AGAIN: Duration = Duration::from_millis(250);

/// What a conversation hears from Asterisk.
pub enum FromAsterisk {
    /// Something Asterisk said about a channel of the conversation.
    Said(ari::Message),
    /// Asterisk's answer to a request the conversation made.
    Answered {
        /// The name the conversation gave the request.
        request: String,
        /// The status of the answer; zero when Asterisk could not be asked.
        status: u16,
        /// What Asterisk answered with; empty when it could not be asked.
        body: String,
    },
    /// The connection to Asterisk is lost. Asterisk has stopped, or is no
    /// longer the Asterisk the conversation's channels were in.
    Gone,
}

#[derive(Default)]
struct Routes {
    /// Conversations, by identifier.
    conversations: HashMap<String, mpsc::UnboundedSender<FromAsterisk>>,
    /// Which conversation owns a channel, by the channel's identifier.
    channels: HashMap<String, String>,
}

/// Asterisk's control interface, as the controller uses it.
pub struct Asterisk {
    address: SocketAddr,
    /// The `Authorization` header of the control user.
    authorization: String,
    /// The same credentials as the address of the event connection wants them.
    api_key: String,
    routes: Mutex<Routes>,
}

/// A request waiting for its turn: its name, method, address and body.
type Asked = (String, String, String, Option<String>);

impl Asterisk {
    /// The control interface at `address`, used as the control user with
    /// this secret.
    pub fn new(address: SocketAddr, secret: &str) -> Arc<Self> {
        let credentials = format!("{CONTROL_USER}:{secret}");
        Arc::new(Self {
            address,
            authorization: format!("Basic {}", BASE64.encode(credentials.as_bytes())),
            api_key: credentials,
            routes: Mutex::new(Routes::default()),
        })
    }

    fn routes(&self) -> MutexGuard<'_, Routes> {
        // Entries are only inserted and removed: a holder that panicked
        // cannot have left the maps half-changed.
        self.routes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Make one request and return the status and the body of the answer.
    ///
    /// # Errors
    ///
    /// Asterisk could not be reached, or did not answer as an HTTP server.
    async fn request(&self, method: &str, uri: &str) -> Result<(u16, String), String> {
        self.request_with(method, uri, None).await
    }

    /// The same with a body: what the request says beyond its address, as
    /// JSON.
    ///
    /// # Errors
    ///
    /// Asterisk could not be reached, or did not answer as an HTTP server.
    async fn request_with(
        &self,
        method: &str,
        uri: &str,
        body: Option<&str>,
    ) -> Result<(u16, String), String> {
        let failed = |error: &dyn std::fmt::Display| format!("{method} {uri}: {error}");
        let stream = TcpStream::connect(self.address)
            .await
            .map_err(|error| failed(&error))?;
        let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|error| failed(&error))?;
        // The connection is driven beside the request and ends with it.
        tokio::spawn(connection);

        let body = Bytes::from(body.unwrap_or_default().to_owned());
        let request = Request::builder()
            .method(method)
            .uri(format!("/ari/{uri}"))
            .header(HOST, self.address.to_string())
            .header(AUTHORIZATION, &self.authorization)
            .header(CONTENT_LENGTH, body.len())
            .header(CONTENT_TYPE, "application/json")
            .header(CONNECTION, "close")
            .body(Full::new(body))
            .map_err(|error| failed(&error))?;
        let response = sender
            .send_request(request)
            .await
            .map_err(|error| failed(&error))?;
        let status = response.status().as_u16();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|error| failed(&error))?
            .to_bytes();
        Ok((status, String::from_utf8_lossy(&body).into_owned()))
    }

    /// Give a conversation its line to Asterisk, with the channels it has.
    fn open_line(self: &Arc<Self>, conversation: &str, channels: &[&str]) -> Line {
        let (to_conversation, inbox) = mpsc::unbounded_channel();
        let (requests, mut asked) = mpsc::unbounded_channel::<Asked>();
        {
            let mut routes = self.routes();
            routes
                .conversations
                .insert(conversation.to_owned(), to_conversation.clone());
            for channel in channels {
                routes
                    .channels
                    .insert((*channel).to_owned(), conversation.to_owned());
            }
        }
        // The conversation's requests, one after another. This outlives the
        // conversation by as long as it takes to make what it left behind:
        // its last requests are the ones that take its channels down.
        let asterisk = Arc::clone(self);
        tokio::spawn(async move {
            while let Some((request, method, uri, body)) = asked.recv().await {
                let (status, body) = match asterisk
                    .request_with(&method, &uri, body.as_deref())
                    .await
                {
                    Ok(answer) => answer,
                    Err(problem) => {
                        eprintln!("asterisk-controller: Asterisk could not be asked — {problem}");
                        (0, String::new())
                    }
                };
                // A conversation that has ended no longer listens.
                let _ = to_conversation.send(FromAsterisk::Answered {
                    request,
                    status,
                    body,
                });
            }
        });
        Line {
            asterisk: Arc::clone(self),
            conversation: conversation.to_owned(),
            inbox,
            requests,
        }
    }
}

/// One conversation's line to Asterisk: what Asterisk says about the
/// conversation's channels comes in here, and the conversation's requests
/// go out.
pub struct Line {
    asterisk: Arc<Asterisk>,
    conversation: String,
    inbox: mpsc::UnboundedReceiver<FromAsterisk>,
    requests: mpsc::UnboundedSender<Asked>,
}

impl Line {
    /// Ask Asterisk for something. Requests are made in the order they are
    /// asked for; the answer to each comes back under the name given here.
    pub fn ask(&self, request: &str, method: &str, uri: &str) {
        self.ask_with(request, method, uri, None);
    }

    /// The same with a body.
    pub fn ask_with(&self, request: &str, method: &str, uri: &str, body: Option<String>) {
        // The other end lives until every request has been made.
        let _ = self
            .requests
            .send((request.to_owned(), method.to_owned(), uri.to_owned(), body));
    }

    /// From now on what Asterisk says about `channel` is for this
    /// conversation. Said before the channel is asked for, so that not a
    /// word about it goes astray.
    pub fn own(&self, channel: &str) {
        self.asterisk
            .routes()
            .channels
            .insert(channel.to_owned(), self.conversation.clone());
    }

    /// What Asterisk says next.
    pub async fn next(&mut self) -> FromAsterisk {
        self.inbox.recv().await.unwrap_or(FromAsterisk::Gone)
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        let mut routes = self.asterisk.routes();
        routes.conversations.remove(&self.conversation);
        routes
            .channels
            .retain(|_, conversation| *conversation != self.conversation);
    }
}

/// Keep the controller connected to Asterisk for as long as the controller
/// runs: find Asterisk, clear what an earlier connection left, listen until
/// the connection is lost, and begin again.
pub async fn run(node: Arc<Node>, asterisk: Arc<Asterisk>) {
    let mut said_why = String::new();
    loop {
        match connected(&node, &asterisk).await {
            Ok(()) => {
                said_why.clear();
                eprintln!("asterisk-controller: the connection to Asterisk is LOST");
                // Every conversation is told. Its channels are out of reach,
                // and when Asterisk is found again they are leftovers.
                for conversation in asterisk.routes().conversations.values() {
                    let _ = conversation.send(FromAsterisk::Gone);
                }
            }
            Err(problem) => {
                // Said once, not four times a second, while it stays the same.
                if problem != said_why {
                    eprintln!("asterisk-controller: Asterisk is not there yet — {problem}");
                    said_why = problem;
                }
                tokio::time::sleep(LOOK_AGAIN).await;
            }
        }
    }
}

/// One connection to Asterisk, from finding it to losing it.
///
/// # Errors
///
/// Asterisk could not be found or would not let the controller in; nothing
/// was begun.
async fn connected(node: &Arc<Node>, asterisk: &Arc<Asterisk>) -> Result<(), String> {
    let address = format!(
        "ws://{}/ari/events?app={APPLICATION}&api_key={}",
        asterisk.address,
        ari::query(&asterisk.api_key),
    );
    let (mut events, _response) = connect_async(address)
        .await
        .map_err(|error| format!("the event connection: {error}"))?;
    eprintln!("asterisk-controller: connected to Asterisk");

    // What is in the application is from before this connection: the
    // conversations of a controller that was restarted, carried on from
    // here, and the pieces of the audio paths it had built, removed.
    for found in take_over(asterisk).await? {
        let channels: Vec<&str> = found
            .members
            .iter()
            .map(|member| member.channel.as_str())
            .collect();
        let line = asterisk.open_line(&found.id, &channels);
        eprintln!(
            "asterisk-controller: carrying on conversation {}, found with {} participants",
            found.id,
            found.members.len()
        );
        tokio::spawn(conversation::carry_on(found, line, Arc::clone(node)));
    }

    while let Some(Ok(frame)) = events.next().await {
        if let Frame::Text(text) = frame {
            match ari::read(text.as_str()) {
                Ok(message) => pass_on(node, asterisk, message),
                Err(error) => {
                    eprintln!("asterisk-controller: Asterisk said something unreadable: {error}");
                }
            }
        }
    }
    Ok(())
}

/// Read what is in the node's application and sort it: the channels of
/// participants, by the conversation each belongs to, with the groups
/// their bridges make — and everything else, which is removed.
///
/// Nothing new can enter the application before this connection —
/// to Asterisk it does not exist while nobody is connected for it — so what
/// is found is exactly what a controller before this one left: its
/// conversations, which carry on, and the media channels, taps and bridges
/// of the audio paths it had built, which died with it or are of no use to
/// anyone, and nobody may be left on a line that no one controls.
async fn take_over(asterisk: &Asterisk) -> Result<Vec<conversation::Found>, String> {
    let (status, body) = asterisk
        .request("GET", &format!("applications/{APPLICATION}"))
        .await?;
    match status {
        200 => {}
        // Nobody has ever connected for the application: nothing is in it.
        404 => return Ok(Vec::new()),
        other => return Err(format!("it answered {other} about its application: {body}")),
    }
    let application = ari::read_application(&body)
        .map_err(|error| format!("it described its application unreadably: {error}"))?;

    let mut found: HashMap<String, conversation::Found> = HashMap::new();
    let mut leftovers = Vec::new();
    for channel in &application.channel_ids {
        let path = format!("channels/{}", ari::query(channel));
        let (status, body) = asterisk.request("GET", &path).await?;
        if status != 200 {
            // Gone meanwhile.
            continue;
        }
        let described = ari::read_channel(&body)
            .map_err(|error| format!("it described channel {channel} unreadably: {error}"))?;
        let mut notes = HashMap::new();
        for name in conversation::NOTES {
            let (status, body) = asterisk
                .request("GET", &format!("{path}/variable?variable={name}"))
                .await?;
            if status == 200
                && let Ok(value) = ari::read_variable(&body)
                && !value.is_empty()
            {
                notes.insert(*name, value);
            }
        }
        match conversation::found(&described, &notes) {
            Some((conversation, origin, member)) => {
                found
                    .entry(conversation.clone())
                    .or_insert_with(|| conversation::Found::new(conversation, origin))
                    .members
                    .push(member);
            }
            None => leftovers.push(path),
        }
    }
    for bridge in &application.bridge_ids {
        let path = format!("bridges/{}", ari::query(bridge));
        let Some((conversation, group)) = conversation::group_of(bridge) else {
            leftovers.push(path);
            continue;
        };
        let Some(found) = found.get_mut(&conversation) else {
            leftovers.push(path);
            continue;
        };
        let (status, body) = asterisk.request("GET", &path).await?;
        if status != 200 {
            continue;
        }
        let described = ari::read_bridge(&body)
            .map_err(|error| format!("it described bridge {bridge} unreadably: {error}"))?;
        found.groups_made = found.groups_made.max(group);
        for member in &mut found.members {
            if described.channels.contains(&member.channel) {
                member.group = Some(group);
            }
        }
    }
    for leftover in leftovers {
        eprintln!("asterisk-controller: removing {leftover}, left from before this connection");
        asterisk.request("DELETE", &leftover).await?;
    }
    Ok(found.into_values().collect())
}

/// Pass what Asterisk said to whom it is for.
fn pass_on(node: &Arc<Node>, asterisk: &Arc<Asterisk>, message: ari::Message) {
    let owner = message
        .channel()
        .and_then(|channel| asterisk.routes().channels.get(channel).cloned());
    if let Some(conversation) = owner {
        if let Some(to_conversation) = asterisk.routes().conversations.get(&conversation) {
            // A conversation that has just ended no longer listens.
            let _ = to_conversation.send(FromAsterisk::Said(message));
        }
    } else if let ari::Message::StasisStart { args, channel } = message {
        arrived(node, asterisk, &args, channel);
    }
}

/// A channel nobody owns entered the node's application: a call arrived.
fn arrived(
    node: &Arc<Node>,
    asterisk: &Arc<Asterisk>,
    arguments: &[String],
    channel: ari::Channel,
) {
    match conversation::entry(arguments) {
        Ok(origin) => {
            // A conversation exists from the moment it has a participant,
            // and is named then.
            let id = uuid::Uuid::new_v4().to_string();
            let line = asterisk.open_line(&id, &[channel.id.as_str()]);
            tokio::spawn(conversation::carry(
                id,
                line,
                conversation::Arrival { channel, origin },
                Arc::clone(node),
            ));
        }
        Err(fault) => {
            // A channel nobody can own must not stay up.
            eprintln!("asterisk-controller: a call was not taken — {fault}");
            let asterisk = Arc::clone(asterisk);
            tokio::spawn(async move {
                let _ = asterisk
                    .request("DELETE", &format!("channels/{}", channel.id))
                    .await;
            });
        }
    }
}
