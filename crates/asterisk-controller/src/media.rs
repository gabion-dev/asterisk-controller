// crates/asterisk-controller/src/media.rs

//! Asterisk's media channel: the connection audio travels on.
//!
//! For every participant whose audio the application wants, the controller
//! has Asterisk create a media channel. Asterisk then opens one more
//! connection to the controller — this one — and on it sends what the
//! participant says and plays what it is given, at the pace of the call.
//! Besides audio the connection carries short messages in Asterisk's own
//! words; this is the one place that knows them.
//!
//! A media connection is closed by Asterisk and never by the controller:
//! the controller ends the media channel with a request to Asterisk and
//! reads this one to its end.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
};

use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use node_protocol::messages::ParticipantId;
use serde::Deserialize;
use serde_json::json;
use tokio::{net::TcpStream, sync::mpsc};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message as Frame};

use crate::playout::FRAME_BYTES;

/// The name, in Asterisk's configuration, of its connection to this
/// controller for media channels.
pub const CONNECTION: &str = "gabion-media";
/// The path Asterisk asks for when it opens a media connection.
pub const PATH: &str = "/media";
/// The WebSocket subprotocol Asterisk speaks on a media connection.
pub const SUBPROTOCOL: &str = "media";
/// The user Asterisk presents when it opens a media connection. Its
/// password is the controller's control secret, which Asterisk reads from
/// its configuration: the loopback address keeps the network out, and this
/// keeps out every other process of the machine.
pub const USER: &str = "gabion-asterisk";

/// The `Authorization` header a media connection must come with.
pub fn authorization(secret: &str) -> String {
    format!(
        "Basic {}",
        data_encoding::BASE64.encode(format!("{USER}:{secret}").as_bytes())
    )
}
/// The audio format the controller asks every media channel for: the one
/// format of the node protocol, so nothing is converted here.
pub const FORMAT: &str = "slin16";
/// How Asterisk is asked to word its messages on this connection: as JSON.
pub const OPTIONS: &str = "f(json)";

/// A media connection of Asterisk.
pub type Socket = WebSocketStream<TcpStream>;
/// The half of a media connection the conversation writes to.
pub type Sink = SplitSink<Socket, Frame>;

/// A message Asterisk sends on a media connection.
#[derive(Debug, Deserialize)]
#[serde(tag = "event")]
pub enum Event {
    /// The first message: which channel the connection belongs to.
    #[serde(rename = "MEDIA_START")]
    Start {
        /// Asterisk's identifier of the media channel.
        channel_id: String,
        /// The audio format of the channel.
        format: String,
        /// Bytes of one frame of that format.
        optimal_frame_size: usize,
    },
    /// Everything handed over before this mark has been played.
    #[serde(rename = "MEDIA_MARK_PROCESSED")]
    MarkReached {
        /// The mark, as it was given.
        correlation_id: String,
    },
    /// The answer to a request for status.
    #[serde(rename = "STATUS")]
    Status {
        /// Frames handed over and not yet played.
        queue_length: usize,
    },
    /// Asterisk could not do what it was asked.
    #[serde(rename = "ERROR")]
    Error {
        /// Its explanation.
        error_text: String,
    },
    /// Anything else Asterisk says.
    #[serde(other)]
    Other,
}

/// Read one message of a media connection.
///
/// # Errors
///
/// Text that is not JSON, or a known message without a field the controller
/// needs, is an error.
#[expect(
    clippy::disallowed_methods,
    reason = "this is Asterisk's language, not the node protocol: there is no description to judge it"
)]
pub fn read(text: &str) -> Result<Event, serde_json::Error> {
    serde_json::from_str(text)
}

/// The text of a command that needs no more than its name.
pub fn command(name: &str) -> String {
    json!({ "command": name }).to_string()
}

/// The text of a mark: Asterisk reports it back once it has played
/// everything handed over before it.
pub fn mark(id: u64) -> String {
    json!({ "command": "MARK_MEDIA", "correlation_id": id.to_string() }).to_string()
}

/// What a media connection tells the conversation it belongs to.
pub enum FromMedia {
    /// Asterisk opened the connection; this is where audio for the
    /// participant is written.
    Connected {
        /// Whose audio the connection carries.
        participant: ParticipantId,
        /// The writing half.
        sink: Sink,
    },
    /// What the participant says, as it came: any number of bytes.
    Audio {
        /// Who is speaking.
        participant: ParticipantId,
        /// The audio.
        audio: Vec<u8>,
    },
    /// A message of Asterisk.
    Said {
        /// Whose media channel said it.
        participant: ParticipantId,
        /// The message.
        event: Event,
    },
    /// The connection cannot be used, or said something unreadable.
    Failed {
        /// Whose audio it was to carry.
        participant: ParticipantId,
        /// What is wrong.
        problem: String,
    },
    /// Asterisk closed the connection.
    Closed {
        /// Whose audio it carried.
        participant: ParticipantId,
    },
}

/// A conversation waiting for the media connection of one of its channels.
pub struct Waiter {
    /// Whose audio the channel carries.
    pub participant: ParticipantId,
    /// Where the conversation hears from the connection.
    pub conversation: mpsc::Sender<FromMedia>,
}

/// Where media connections meet the conversations that asked for them.
///
/// A conversation that has Asterisk create a media channel leaves a waiter
/// here under the channel's identifier; the connection Asterisk then opens
/// names that channel in its first message and is handed to the waiter.
#[derive(Clone, Default)]
pub struct Door(Arc<Mutex<HashMap<String, Waiter>>>);

impl Door {
    /// Expect the media connection of `channel`.
    pub fn expect(&self, channel: String, waiter: Waiter) {
        self.waiters().insert(channel, waiter);
    }

    /// Stop expecting the media connection of `channel`.
    pub fn forget(&self, channel: &str) {
        self.waiters().remove(channel);
    }

    fn take(&self, channel: &str) -> Option<Waiter> {
        self.waiters().remove(channel)
    }

    fn waiters(&self) -> std::sync::MutexGuard<'_, HashMap<String, Waiter>> {
        // The map is only ever inserted into and removed from: a holder that
        // panicked cannot have left it half-changed.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Serve one media connection, from Asterisk's first word until Asterisk
/// closes it.
pub async fn serve(mut socket: Socket, door: Door) {
    match first_word(&mut socket, &door).await {
        Ok(waiter) => carry(socket, waiter).await,
        Err(problem) => {
            eprintln!("asterisk-controller: a media connection was not taken — {problem}");
            // Nobody will end this channel with a request of a conversation, so
            // Asterisk is asked here to end it — and then closes.
            let _ = socket.send(Frame::text(command("HANGUP"))).await;
            while let Some(Ok(_)) = socket.next().await {}
        }
    }
}

/// Read which channel the connection belongs to and find who waits for it.
async fn first_word(socket: &mut Socket, door: &Door) -> Result<Waiter, String> {
    let text = loop {
        match socket.next().await {
            Some(Ok(Frame::Text(text))) => break text,
            Some(Ok(Frame::Binary(_) | Frame::Ping(_) | Frame::Pong(_) | Frame::Frame(_))) => {}
            Some(Ok(Frame::Close(_))) | None => {
                return Err("Asterisk closed it before saying which channel it is for".into());
            }
            Some(Err(error)) => return Err(error.to_string()),
        }
    };
    let Event::Start {
        channel_id,
        format,
        optimal_frame_size,
    } = read(text.as_str()).map_err(|error| format!("its first message is unreadable: {error}"))?
    else {
        return Err(format!("its first message does not name a channel: {text}"));
    };
    let waiter = door
        .take(&channel_id)
        .ok_or_else(|| format!("no conversation expects media channel {channel_id}"))?;
    if format != FORMAT || optimal_frame_size != FRAME_BYTES {
        let problem =
            format!("its audio is {format} in frames of {optimal_frame_size} bytes, not {FORMAT}");
        let failed = FromMedia::Failed {
            participant: waiter.participant,
            problem: problem.clone(),
        };
        let _ = waiter.conversation.send(failed).await;
        return Err(problem);
    }
    Ok(waiter)
}

/// Pass everything Asterisk says on to the conversation, to the end.
async fn carry(socket: Socket, waiter: Waiter) {
    let Waiter {
        participant,
        conversation,
    } = waiter;
    let (sink, mut stream) = socket.split();
    let connected = FromMedia::Connected {
        participant: participant.clone(),
        sink,
    };
    // A conversation that is over no longer listens; the connection is read
    // to its end all the same.
    let mut listening = conversation.send(connected).await.is_ok();
    while let Some(Ok(frame)) = stream.next().await {
        let participant = participant.clone();
        let said = match frame {
            Frame::Binary(audio) => FromMedia::Audio {
                participant,
                audio: audio.to_vec(),
            },
            Frame::Text(text) => match read(text.as_str()) {
                Ok(event) => FromMedia::Said { participant, event },
                Err(error) => FromMedia::Failed {
                    participant,
                    problem: format!("said something unreadable: {error}"),
                },
            },
            Frame::Close(_) => break,
            Frame::Ping(_) | Frame::Pong(_) | Frame::Frame(_) => continue,
        };
        if listening {
            listening = conversation.send(said).await.is_ok();
        }
    }
    if listening {
        let _ = conversation.send(FromMedia::Closed { participant }).await;
    }
}
