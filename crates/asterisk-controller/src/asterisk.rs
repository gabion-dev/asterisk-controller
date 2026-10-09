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
    /// The conversation's line is closed: nothing more comes on it.
    Gone,
    /// The connection to Asterisk is lost: Asterisk may have stopped, or
    /// only the connection. Until it is found again nothing is known of the
    /// conversation's channels.
    Lost,
    /// Asterisk is found again after a loss. What was found of the
    /// conversation in the node's application, if anything: whoever is not
    /// in it ended their call meanwhile.
    Back(Option<conversation::Found>),
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

    /// Have Asterisk read the configuration of one of its modules again —
    /// how changed settings are taken in without dropping a call.
    ///
    /// # Errors
    ///
    /// Asterisk could not be asked, or refused; the reason, in words.
    pub async fn reload(&self, module: &str) -> Result<(), String> {
        let (status, body) = self
            .request("PUT", &format!("asterisk/modules/{}", ari::query(module)))
            .await?;
        if (200..=299).contains(&status) {
            Ok(())
        } else {
            Err(format!(
                "Asterisk answered {status} when asked to reload {module}: {body}"
            ))
        }
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
        let own = to_conversation.clone();
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
            own,
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
    /// The way into this line's inbox, as the routes hold it: what tells
    /// this line's routes from those of a later line of the same
    /// conversation.
    own: mpsc::UnboundedSender<FromAsterisk>,
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
    /// Take the conversation's routes down — unless a later line of the
    /// same conversation holds them now. A conversation found again after
    /// the connection to Asterisk was lost keeps its identifier, and its new
    /// line may be open before the old one is dropped.
    fn drop(&mut self) {
        let mut routes = self.asterisk.routes();
        let still_ours = routes
            .conversations
            .get(&self.conversation)
            .is_some_and(|routed| routed.same_channel(&self.own));
        if still_ours {
            routes.conversations.remove(&self.conversation);
            routes
                .channels
                .retain(|_, conversation| *conversation != self.conversation);
        }
    }
}

/// Keep the controller connected to Asterisk for as long as the controller
/// runs: find Asterisk, carry on the conversations an earlier connection
/// left and remove the rest of what it left ([`take_over`]), listen until
/// the connection is lost, and begin again.
pub async fn run(node: Arc<Node>, asterisk: Arc<Asterisk>) {
    let mut said_why = String::new();
    loop {
        match connected(&node, &asterisk).await {
            Ok(Ended::Replaced) => {
                // Taking the application back would only take it from the
                // other one in turn; staying would be a controller that
                // hears nothing. It stops, and says why.
                eprintln!(
                    "asterisk-controller: the node's application in Asterisk was TAKEN by another \
                     connection — is a second controller running next to the same Asterisk? \
                     This controller hears nothing more from Asterisk and stops"
                );
                node.replaced.notify_one();
                return;
            }
            Ok(Ended::Lost) => {
                said_why.clear();
                eprintln!("asterisk-controller: the connection to Asterisk is LOST");
                // Every conversation is told, and waits: when Asterisk is
                // found again, what it still has of each says who is left.
                for conversation in asterisk.routes().conversations.values() {
                    let _ = conversation.send(FromAsterisk::Lost);
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

/// How a connection to Asterisk ended.
enum Ended {
    /// The connection is gone.
    Lost,
    /// Another connection has taken the node's application: nothing more
    /// comes on this one.
    Replaced,
}

/// One connection to Asterisk, from finding it to losing it.
///
/// # Errors
///
/// Asterisk could not be found or would not let the controller in; nothing
/// was begun.
async fn connected(node: &Arc<Node>, asterisk: &Arc<Asterisk>) -> Result<Ended, String> {
    // What is in the application is read before the event connection is
    // opened. Until then nobody is connected for the application and no
    // call can enter it, so what is there is exactly what was left from
    // before: the conversations of a controller that was restarted, carried
    // on from here, and the pieces of the audio paths it had built, removed.
    // Read once the connection is open, it could also hold a call that has
    // just entered and is not yet known — and be removed as a leftover.
    let found = take_over(asterisk).await?;

    let address = format!(
        "ws://{}/ari/events?app={APPLICATION}&api_key={}",
        asterisk.address,
        ari::query(&asterisk.api_key),
    );
    let (mut events, _response) = connect_async(address)
        .await
        .map_err(|error| format!("the event connection: {error}"))?;
    eprintln!("asterisk-controller: connected to Asterisk");

    // A conversation still alive in this controller — the connection was
    // lost, not the controller — is told what was found of it, and goes on
    // with it. One that was not found ended with Asterisk.
    let mut found: HashMap<String, conversation::Found> = found
        .into_iter()
        .map(|found| (found.id.clone(), found))
        .collect();
    for (id, conversation) in &asterisk.routes().conversations {
        let _ = conversation.send(FromAsterisk::Back(found.remove(id)));
    }

    // The rest are left by a controller before this one. A participant
    // found and gone before the connection was open is told by Asterisk's
    // answer about their channel, which each carried-on conversation asks
    // for first.
    for found in found.into_values() {
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
                Ok(ari::Message::ApplicationReplaced) => return Ok(Ended::Replaced),
                Ok(message) => pass_on(node, asterisk, message),
                Err(error) => {
                    eprintln!("asterisk-controller: Asterisk said something unreadable: {error}");
                }
            }
        }
    }
    Ok(Ended::Lost)
}

/// Read what is in the node's application and sort it: the channels of
/// participants, by the conversation each belongs to, with the groups
/// their bridges make — and everything else, which is removed.
///
/// Read while nobody is connected for the application ([`connected`]):
/// nothing can enter it then, so what is found is exactly what a controller
/// before this one left — its conversations, which carry on, and the media
/// channels, taps and bridges of the audio paths it had built, which died
/// with it or are of no use to anyone, and nobody may be left on a line
/// that no one controls.
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
        let Some(conversation) = conversation::group_of(bridge) else {
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
        for member in &mut found.members {
            if described.channels.contains(&member.channel) {
                member.group = Some(bridge.clone());
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

/// The application asked the node to start a conversation: it is named now,
/// and carried from its first call to its end.
pub fn start_by_code(node: &Arc<Node>, asterisk: &Arc<Asterisk>, asked: conversation::Asked) {
    let id = uuid::Uuid::new_v4().to_string();
    let line = asterisk.open_line(&id, &[]);
    tokio::spawn(conversation::carry_by_code(
        id,
        line,
        asked,
        Arc::clone(node),
    ));
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
