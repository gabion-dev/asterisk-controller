// crates/asterisk-controller/src/conversation.rs

//! One conversation: Asterisk's control connection for a call on one side,
//! the application's conversation connection on the other, and the
//! translation between them.
//!
//! Asterisk opens a control connection for every call. For each one the
//! controller opens one conversation connection to the application; the
//! instance that accepts it owns the conversation. From then on events of
//! Asterisk become events of the node protocol, and commands of the
//! application become requests to Asterisk.

use std::{collections::HashMap, fmt, num::NonZeroU64, sync::Arc};

use futures_util::{SinkExt, StreamExt};
use node_protocol::{
    DecodeError,
    messages::{
        ApplicationMessage, Command, CommandOutcome, CommandRejection, ControllerMessage,
        Departure, EndReason, Event, Opening, Origin, Participant, ParticipantId,
        ParticipantMedium,
    },
};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{
        Message as Frame,
        protocol::{CloseFrame, frame::coding::CloseCode},
    },
};

use crate::{
    ari,
    config::{Config, PROTOCOL_VERSION},
};

type AsteriskSocket = WebSocketStream<TcpStream>;
type ApplicationSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Why a conversation could not be carried on.
#[derive(Debug)]
pub enum Fault {
    /// Asterisk's connection failed or said something unreadable.
    Asterisk(String),
    /// The application could not be reached or its connection failed.
    Application(String),
    /// The application sent something the protocol does not allow.
    Protocol(DecodeError),
    /// The dialplan started the application with arguments that name no entry.
    Entry(String),
    /// The application asked for something this build does not do yet.
    NotImplemented(&'static str),
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Asterisk(problem) => write!(f, "Asterisk: {problem}"),
            Self::Application(problem) => write!(f, "application: {problem}"),
            Self::Protocol(error) => write!(f, "protocol: {error}"),
            Self::Entry(problem) => write!(f, "entry: {problem}"),
            Self::NotImplemented(what) => write!(f, "not implemented in this build: {what}"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ParticipantState {
    Ringing,
    InConversation,
}

struct Member {
    id: ParticipantId,
    channel: String,
    state: ParticipantState,
    /// Set when the controller itself asked Asterisk to drop the channel:
    /// the hang-up that follows is then ours, not the participant's.
    removed_by_us: bool,
}

/// What a request to Asterisk was for — what to do with its answer.
enum Pending {
    /// A command of the application; its outcome is owed.
    Command(u64),
    /// Housekeeping of the controller; nobody waits for the answer.
    Internal,
}

struct Conversation {
    id: String,
    asterisk: AsteriskSocket,
    application: ApplicationSocket,
    members: Vec<Member>,
    pending: HashMap<String, Pending>,
    next_request: u64,
    /// Set by the `end` command: the conversation ends when the last
    /// participant is gone, and it ends because the handler said so.
    ended_by_handler: bool,
}

/// Serve one call from Asterisk's first word to the end of the conversation.
pub async fn serve(mut asterisk: AsteriskSocket, config: Arc<Config>) {
    let id = uuid::Uuid::new_v4().to_string();
    let outcome = async {
        let (channel, origin) = first_participant(&mut asterisk).await?;
        let (mut conversation, hello) = open(&id, asterisk, channel, origin, &config).await?;
        // From here on a participant is on the line. Whatever goes wrong —
        // the application declines, vanishes, breaks the protocol — the
        // participant is not left there with no one in control.
        let result = async {
            conversation.tell(&hello).await?;
            conversation.await_acceptance().await?;
            conversation.run().await
        }
        .await;
        if let Err(fault) = &result {
            conversation.abandon(fault).await;
        }
        result
    }
    .await;
    match outcome {
        Ok(()) => eprintln!("conversation {id}: ended"),
        Err(fault) => eprintln!("conversation {id}: FAILED — {fault}"),
    }
}

/// Wait for the channel that opened the connection and read which entry it
/// came through.
async fn first_participant(asterisk: &mut AsteriskSocket) -> Result<(ari::Channel, Origin), Fault> {
    loop {
        let text = next_text(asterisk).await?.ok_or_else(|| {
            Fault::Asterisk("closed the connection before any channel entered".into())
        })?;
        if let ari::Message::StasisStart { args, channel } = read_asterisk(&text)? {
            return match entry(&args) {
                Ok(origin) => Ok((channel, origin)),
                Err(fault) => {
                    // A channel nobody can own must not stay up: Asterisk is
                    // told to drop it before the fault is reported.
                    let drop = ari::request(
                        "entry-refused",
                        "DELETE",
                        &format!("channels/{}", channel.id),
                    );
                    let _ = asterisk.send(Frame::text(drop)).await;
                    Err(fault)
                }
            };
        }
    }
}

/// The entry a channel came through, from the arguments the dialplan gave.
///
/// The dialplan is written by the controller, so the arguments are its own
/// words: `dialed_number,<number>`.
fn entry(arguments: &[String]) -> Result<Origin, Fault> {
    match arguments {
        [kind, number] if kind == "dialed_number" => Ok(Origin::DialedNumber {
            dialed: number
                .parse()
                .map_err(|_| Fault::Entry(format!("dialed number {number:?} is not E.164")))?,
        }),
        other => Err(Fault::Entry(format!("arguments {other:?} name no entry"))),
    }
}

/// Open the conversation connection and prepare the hello for it.
async fn open(
    id: &str,
    mut asterisk: AsteriskSocket,
    channel: ari::Channel,
    origin: Origin,
    config: &Config,
) -> Result<(Conversation, ControllerMessage), Fault> {
    let first = Member {
        id: participant_id(1)?,
        channel: channel.id,
        state: if channel.state == ari::STATE_UP {
            ParticipantState::InConversation
        } else {
            ParticipantState::Ringing
        },
        removed_by_us: false,
    };

    let connected = connect_async(config.application_url.as_str()).await;
    let application = match connected {
        Ok((socket, _response)) => socket,
        Err(error) => {
            let drop = ari::request("no-owner", "DELETE", &format!("channels/{}", first.channel));
            let _ = asterisk.send(Frame::text(drop)).await;
            return Err(Fault::Application(format!(
                "cannot open a conversation connection to {}: {error}",
                config.application_url
            )));
        }
    };

    let hello = ControllerMessage::Hello {
        protocol: NonZeroU64::new(PROTOCOL_VERSION)
            .ok_or_else(|| Fault::Application("protocol version is zero".into()))?,
        node: config
            .node
            .parse()
            .map_err(|_| Fault::Application(format!("node name {:?} is not valid", config.node)))?,
        controller_version: env!("CARGO_PKG_VERSION")
            .parse()
            .map_err(|_| Fault::Application("controller version is not valid".into()))?,
        conversation: id
            .parse()
            .map_err(|_| Fault::Application("conversation identifier is not valid".into()))?,
        opening: Opening::Started {
            origin,
            first: Participant {
                id: first.id.clone(),
                medium: ParticipantMedium::TelephoneNetwork,
                // A number the medium did not give in full form is not
                // guessed at: the participant then has none.
                number: channel.caller.number.parse().ok(),
            },
        },
    };

    let conversation = Conversation {
        id: id.to_owned(),
        asterisk,
        application,
        members: vec![first],
        pending: HashMap::new(),
        next_request: 0,
        ended_by_handler: false,
    };
    Ok((conversation, hello))
}

impl Conversation {
    /// Wait for the application to take the conversation.
    async fn await_acceptance(&mut self) -> Result<(), Fault> {
        loop {
            let text = next_text(&mut self.application)
                .await
                .map_err(|fault| Fault::Application(fault.to_string()))?
                .ok_or_else(|| {
                    Fault::Application("closed the connection before accepting".into())
                })?;
            match node_protocol::decode::<ApplicationMessage>(&text).map_err(Fault::Protocol)? {
                ApplicationMessage::Accept => return Ok(()),
                ApplicationMessage::Decline { reason } => {
                    return Err(Fault::Application(format!(
                        "declined the conversation: {reason}"
                    )));
                }
                ApplicationMessage::Ping { n } => self.tell(&ControllerMessage::Pong { n }).await?,
                ApplicationMessage::Pong { .. } => {}
                ApplicationMessage::Command { .. } => {
                    return Err(Fault::Application(
                        "sent a command before accepting the conversation".into(),
                    ));
                }
            }
        }
    }

    /// Translate in both directions until the conversation is over.
    async fn run(&mut self) -> Result<(), Fault> {
        while !self.members.is_empty() {
            tokio::select! {
                said = next_text(&mut self.asterisk) => match said? {
                    Some(text) => self.on_asterisk(read_asterisk(&text)?).await?,
                    // Asterisk closes the connection when the last channel
                    // is gone; whoever is still listed left with it.
                    None => self.asterisk_gone().await?,
                },
                said = self.application.next() => match said {
                    Some(Ok(Frame::Text(text))) => {
                        let message = node_protocol::decode(text.as_str()).map_err(Fault::Protocol)?;
                        self.on_application(message).await?;
                    }
                    Some(Ok(Frame::Binary(_))) => return Err(Fault::NotImplemented("audio frames")),
                    Some(Ok(Frame::Ping(_) | Frame::Pong(_) | Frame::Frame(_))) => {}
                    Some(Ok(Frame::Close(_))) | None => {
                        return Err(Fault::Application("lost the conversation connection".into()));
                    }
                    Some(Err(error)) => return Err(Fault::Application(error.to_string())),
                },
            }
        }

        let reason = if self.ended_by_handler {
            EndReason::EndedByHandler
        } else {
            EndReason::LastParticipantLeft
        };
        self.event(Event::ConversationEnded { reason }).await?;
        let _ = self.application.close(None).await;
        Ok(())
    }

    async fn on_asterisk(&mut self, message: ari::Message) -> Result<(), Fault> {
        match message {
            ari::Message::ChannelStateChange { channel } => {
                let answered = self.members.iter_mut().find(|member| {
                    member.channel == channel.id
                        && member.state == ParticipantState::Ringing
                        && channel.state == ari::STATE_UP
                });
                if let Some(member) = answered {
                    member.state = ParticipantState::InConversation;
                    let participant = member.id.clone();
                    self.event(Event::ParticipantAnswered { participant })
                        .await?;
                }
            }
            ari::Message::ChannelDtmfReceived { digit, channel } => {
                if let Some(member) = self
                    .members
                    .iter()
                    .find(|member| member.channel == channel.id)
                {
                    let participant = member.id.clone();
                    let digit = digit
                        .parse()
                        .map_err(|_| Fault::Asterisk(format!("reported a digit {digit:?}")))?;
                    self.event(Event::DigitReceived { participant, digit })
                        .await?;
                }
            }
            ari::Message::StasisEnd { channel } => self.left(&channel.id).await?,
            ari::Message::RestResponse {
                request_id,
                status_code,
            } => {
                if let Some(Pending::Command(id)) = self.pending.remove(&request_id) {
                    let outcome = outcome_of(status_code)?;
                    self.tell(&ControllerMessage::CommandResult { id, outcome })
                        .await?;
                }
            }
            // The first channel has already entered; further ones are added
            // by commands this build does not have yet.
            ari::Message::StasisStart { .. } | ari::Message::Other => {}
        }
        Ok(())
    }

    /// A channel is gone: its participant left.
    async fn left(&mut self, channel: &str) -> Result<(), Fault> {
        let Some(position) = self
            .members
            .iter()
            .position(|member| member.channel == channel)
        else {
            return Ok(());
        };
        let member = self.members.swap_remove(position);
        let departure = if member.removed_by_us {
            Departure::Removed
        } else {
            Departure::HungUp
        };
        self.event(Event::ParticipantLeft {
            participant: member.id,
            departure,
        })
        .await
    }

    async fn asterisk_gone(&mut self) -> Result<(), Fault> {
        let channels: Vec<String> = self
            .members
            .iter()
            .map(|member| member.channel.clone())
            .collect();
        for channel in channels {
            self.left(&channel).await?;
        }
        Ok(())
    }

    async fn on_application(&mut self, message: ApplicationMessage) -> Result<(), Fault> {
        match message {
            ApplicationMessage::Command { id, command } => self.command(id, command).await,
            ApplicationMessage::Ping { n } => self.tell(&ControllerMessage::Pong { n }).await,
            ApplicationMessage::Pong { .. } => Ok(()),
            ApplicationMessage::Accept | ApplicationMessage::Decline { .. } => Err(
                Fault::Application("accepted or declined a conversation it already owns".into()),
            ),
        }
    }

    async fn command(&mut self, id: u64, command: Command) -> Result<(), Fault> {
        match command {
            Command::Answer { participant } => {
                let Some(channel) = self.channel_of(&participant) else {
                    return self.reject(id, CommandRejection::UnknownParticipant).await;
                };
                self.ask(
                    Pending::Command(id),
                    "POST",
                    &format!("channels/{channel}/answer"),
                )
                .await
            }
            Command::Remove { participant } | Command::Reject { participant, .. } => {
                let Some(member) = self
                    .members
                    .iter_mut()
                    .find(|member| member.id == participant)
                else {
                    return self.reject(id, CommandRejection::UnknownParticipant).await;
                };
                member.removed_by_us = true;
                let channel = member.channel.clone();
                self.ask(
                    Pending::Command(id),
                    "DELETE",
                    &format!("channels/{channel}"),
                )
                .await
            }
            Command::SendDigits {
                participant,
                digits,
            } => {
                let Some(channel) = self.channel_of(&participant) else {
                    return self.reject(id, CommandRejection::UnknownParticipant).await;
                };
                let digits = digits.as_str().replace('#', "%23").replace('*', "%2A");
                self.ask(
                    Pending::Command(id),
                    "POST",
                    &format!("channels/{channel}/dtmf?dtmf={digits}"),
                )
                .await
            }
            Command::End => {
                self.ended_by_handler = true;
                let channels: Vec<String> = self
                    .members
                    .iter_mut()
                    .map(|member| {
                        member.removed_by_us = true;
                        member.channel.clone()
                    })
                    .collect();
                for channel in channels {
                    self.ask(Pending::Internal, "DELETE", &format!("channels/{channel}"))
                        .await?;
                }
                let outcome = CommandOutcome::Accepted;
                self.tell(&ControllerMessage::CommandResult { id, outcome })
                    .await
            }
            Command::Dial { .. } => Err(Fault::NotImplemented("dial")),
            Command::Connect { .. } => Err(Fault::NotImplemented("connect")),
            Command::Separate { .. } => Err(Fault::NotImplemented("separate")),
            Command::HoldFor { .. } => Err(Fault::NotImplemented("hold_for")),
            Command::Listen { .. } => Err(Fault::NotImplemented("listen")),
            Command::StopListening { .. } => Err(Fault::NotImplemented("stop_listening")),
            Command::Play { .. } => Err(Fault::NotImplemented("play")),
            Command::FlushPlayback { .. } => Err(Fault::NotImplemented("flush_playback")),
            Command::StartRecording { .. } => Err(Fault::NotImplemented("start_recording")),
            Command::StopRecording { .. } => Err(Fault::NotImplemented("stop_recording")),
        }
    }

    fn channel_of(&self, participant: &ParticipantId) -> Option<String> {
        self.members
            .iter()
            .find(|member| member.id == *participant)
            .map(|member| member.channel.clone())
    }

    async fn reject(&mut self, id: u64, reason: CommandRejection) -> Result<(), Fault> {
        let outcome = CommandOutcome::Rejected { reason };
        self.tell(&ControllerMessage::CommandResult { id, outcome })
            .await
    }

    /// Send a request to Asterisk over the call's own connection.
    async fn ask(&mut self, pending: Pending, method: &str, uri: &str) -> Result<(), Fault> {
        self.next_request += 1;
        let request_id = format!("{}-{}", self.id, self.next_request);
        self.pending.insert(request_id.clone(), pending);
        self.asterisk
            .send(Frame::text(ari::request(&request_id, method, uri)))
            .await
            .map_err(|error| Fault::Asterisk(error.to_string()))
    }

    async fn event(&mut self, event: Event) -> Result<(), Fault> {
        self.tell(&ControllerMessage::Event { event }).await
    }

    async fn tell(&mut self, message: &ControllerMessage) -> Result<(), Fault> {
        let text = node_protocol::encode(message).map_err(Fault::Protocol)?;
        self.application
            .send(Frame::text(text))
            .await
            .map_err(|error| Fault::Application(error.to_string()))
    }

    /// The conversation cannot go on: nobody may be left on a line that no
    /// one controls, and the application is told why its connection closes.
    async fn abandon(&mut self, fault: &Fault) {
        for member in &self.members {
            let drop = ari::request("abandon", "DELETE", &format!("channels/{}", member.channel));
            let _ = self.asterisk.send(Frame::text(drop)).await;
        }
        let close = CloseFrame {
            code: CloseCode::Error,
            reason: fault
                .to_string()
                .chars()
                .take(100)
                .collect::<String>()
                .into(),
        };
        let _ = self.application.close(Some(close)).await;
    }
}

/// What Asterisk's answer to a request means for the command behind it.
fn outcome_of(status_code: u16) -> Result<CommandOutcome, Fault> {
    match status_code {
        200..=299 => Ok(CommandOutcome::Accepted),
        404 => Ok(CommandOutcome::Rejected {
            reason: CommandRejection::UnknownParticipant,
        }),
        400 | 409 | 412 | 422 => Ok(CommandOutcome::Rejected {
            reason: CommandRejection::ParticipantNotInRequiredState,
        }),
        other => Err(Fault::Asterisk(format!(
            "answered a request with status {other}"
        ))),
    }
}

fn participant_id(number: u64) -> Result<ParticipantId, Fault> {
    format!("p-{number}")
        .parse()
        .map_err(|_| Fault::Asterisk("participant identifier is not valid".into()))
}

fn read_asterisk(text: &str) -> Result<ari::Message, Fault> {
    ari::read(text).map_err(|error| Fault::Asterisk(format!("said something unreadable: {error}")))
}

/// The next text message of a connection; `None` when it has closed.
async fn next_text<S>(socket: &mut WebSocketStream<S>) -> Result<Option<String>, Fault>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        match socket.next().await {
            Some(Ok(Frame::Text(text))) => return Ok(Some(text.as_str().to_owned())),
            Some(Ok(Frame::Close(_))) | None => return Ok(None),
            Some(Ok(Frame::Binary(_) | Frame::Ping(_) | Frame::Pong(_) | Frame::Frame(_))) => {}
            Some(Err(error)) => return Err(Fault::Asterisk(error.to_string())),
        }
    }
}
