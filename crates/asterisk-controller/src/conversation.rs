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
//!
//! The control connection is closed by Asterisk and never by the controller
//! (see [`until_asterisk_closes`]).
//!
//! Audio between the application and a participant takes a path of its own
//! inside Asterisk — a media channel joined to the participant's channel —
//! and a connection of its own to the controller ([`crate::media`]). The
//! path is opened the first time the application asks to hear a participant
//! or to play to them, and lasts as long as the participant does.

use std::{collections::HashMap, fmt, num::NonZeroU64, sync::Arc};

use futures_util::{SinkExt, StreamExt};
use node_protocol::{
    DecodeError,
    audio::{AudioFrame, AudioFrameError},
    messages::{
        ApplicationMessage, Command, CommandOutcome, CommandRejection, ControllerMessage,
        Departure, EndReason, Event, Opening, Origin, Participant, ParticipantId,
        ParticipantMedium, SegmentId,
    },
};
use tokio::{net::TcpStream, sync::mpsc};
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
    media::{self, FromMedia},
    playout::{FRAME_BYTES, Outcome, Playout, ToMedium},
};

type AsteriskSocket = WebSocketStream<TcpStream>;
type ApplicationSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Milliseconds of audio in one heard frame.
const HEARD_FRAME_MS: u64 = 20;
/// How much a media connection may say before the conversation has taken it
/// in. Past this the connection waits, and Asterisk with it: audio is never
/// piled up without bound behind an application that does not read.
const MEDIA_BACKLOG: usize = 256;

/// Why a conversation could not be carried on.
#[derive(Debug)]
pub enum Fault {
    /// Asterisk's connection failed or said something unreadable.
    Asterisk(String),
    /// The application could not be reached or its connection failed.
    Application(String),
    /// The application sent something the protocol does not allow.
    Protocol(DecodeError),
    /// The application sent an audio frame the protocol does not allow.
    Frame(AudioFrameError),
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
            Self::Frame(error) => write!(f, "protocol: {error}"),
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
    /// The participant's audio path, once the application has asked for it.
    sound: Option<Sound>,
}

/// The audio path between the application and one participant.
///
/// In Asterisk it is a media channel and a bridge that joins it to the
/// participant's channel: what the participant says comes out of the media
/// channel, what is put into it the participant hears.
struct Sound {
    /// Asterisk's identifier of the media channel.
    channel: String,
    /// Asterisk's identifier of the bridge.
    bridge: String,
    /// Where audio for the participant is written; `None` until Asterisk
    /// has opened the media connection.
    sink: Option<media::Sink>,
    /// The participant and the media channel have been joined. Until then
    /// nothing is handed to the media channel: it would play to no one and
    /// report it as played.
    joined: bool,
    /// The application has asked to hear the participant.
    listening: bool,
    /// What the participant said and is not yet a whole frame.
    heard: Vec<u8>,
    /// Milliseconds of the participant's audio that came before `heard`.
    /// Counted from the moment the path opened, heard by the application
    /// or not: a pause in listening shows as a gap in positions.
    heard_ms: u64,
    playout: Playout,
}

/// A step of opening an audio path, as asked of Asterisk.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SoundStep {
    MediaChannel,
    Bridge,
    Join,
}

impl fmt::Display for SoundStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MediaChannel => "create a media channel",
            Self::Bridge => "create a bridge",
            Self::Join => "join the media channel and the participant",
        })
    }
}

/// What a request to Asterisk was for — what to do with its answer.
enum Pending {
    /// A command of the application; its outcome is owed.
    Command(u64),
    /// A step of opening a participant's audio path.
    Sound(ParticipantId, SoundStep),
    /// Housekeeping of the controller; nobody waits for the answer.
    Internal,
}

struct Conversation<'a> {
    id: String,
    asterisk: &'a mut AsteriskSocket,
    application: ApplicationSocket,
    members: Vec<Member>,
    pending: HashMap<String, Pending>,
    next_request: u64,
    /// Set by the `end` command: the conversation ends when the last
    /// participant is gone, and it ends because the handler said so.
    ended_by_handler: bool,
    /// Asterisk's name for this call's application: channels the controller
    /// adds to the call enter it, and are reported on this call's connection.
    asterisk_application: String,
    door: media::Door,
    /// What the media connections of this conversation say.
    from_media: mpsc::Receiver<FromMedia>,
    /// The other end of `from_media`, given to each media connection.
    media_inbox: mpsc::Sender<FromMedia>,
    /// How many audio paths have been opened: names their channels.
    sounds_opened: u64,
}

/// Serve one control connection of Asterisk, from its first word until
/// Asterisk closes it.
pub async fn serve(mut asterisk: AsteriskSocket, config: Arc<Config>, door: media::Door) {
    match first_participant(&mut asterisk).await {
        Ok(arrival) => {
            // A conversation exists from the moment it has a participant, so
            // it is named here and not when the connection was accepted: a
            // connection that brings no channel is not a conversation.
            let id = uuid::Uuid::new_v4().to_string();
            match carry(&id, &mut asterisk, arrival, &config, door).await {
                Ok(()) => eprintln!("conversation {id}: ended"),
                Err(fault) => eprintln!("conversation {id}: FAILED — {fault}"),
            }
        }
        Err(fault) => {
            eprintln!("asterisk-controller: a control connection carried no call — {fault}");
        }
    }
    until_asterisk_closes(&mut asterisk).await;
}

/// Carry one conversation from its first participant to its end.
async fn carry(
    id: &str,
    asterisk: &mut AsteriskSocket,
    arrival: Arrival,
    config: &Config,
    door: media::Door,
) -> Result<(), Fault> {
    let (mut conversation, hello) = open(id, asterisk, arrival, config, door).await?;
    // From here on a participant is on the line. Whatever goes wrong — the
    // application declines, vanishes, breaks the protocol — the participant
    // is not left there with no one in control.
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

/// Leave the closing of a control connection to Asterisk: read it to its end.
///
/// Asterisk closes the connection of a call itself, a few seconds after the
/// call's last channel has left — it still has events to send about channels
/// that are being torn down. A peer that closes first, however properly, is
/// to Asterisk a peer that was lost: it drops the connection and opens it
/// again for the same call. And while it drops a connection, an event it
/// sends for that call can crash it — Asterisk 22.11.0 dies of a
/// segmentation fault in its WebSocket write. So the controller never closes
/// this connection and never lets go of it early; what Asterisk says
/// meanwhile needs no answer, since the conversation is already over.
async fn until_asterisk_closes(asterisk: &mut AsteriskSocket) {
    while let Some(Ok(_)) = asterisk.next().await {}
}

/// The channel a control connection was opened for.
struct Arrival {
    channel: ari::Channel,
    /// The entry it came through.
    origin: Origin,
    /// Asterisk's name for the call's application.
    application: String,
}

/// Wait for the channel that opened the connection and read which entry it
/// came through.
async fn first_participant(asterisk: &mut AsteriskSocket) -> Result<Arrival, Fault> {
    loop {
        let text = next_text(asterisk).await?.ok_or_else(|| {
            Fault::Asterisk("closed the connection before any channel entered".into())
        })?;
        if let ari::Message::StasisStart {
            application,
            args,
            channel,
        } = read_asterisk(&text)?
        {
            return match entry(&args) {
                Ok(origin) => Ok(Arrival {
                    channel,
                    origin,
                    application,
                }),
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
async fn open<'a>(
    id: &str,
    asterisk: &'a mut AsteriskSocket,
    arrival: Arrival,
    config: &Config,
    door: media::Door,
) -> Result<(Conversation<'a>, ControllerMessage), Fault> {
    let Arrival {
        channel,
        origin,
        application: asterisk_application,
    } = arrival;
    let first = Member {
        id: participant_id(1)?,
        channel: channel.id,
        state: if channel.state == ari::STATE_UP {
            ParticipantState::InConversation
        } else {
            ParticipantState::Ringing
        },
        removed_by_us: false,
        sound: None,
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

    let (media_inbox, from_media) = mpsc::channel(MEDIA_BACKLOG);
    let conversation = Conversation {
        id: id.to_owned(),
        asterisk,
        application,
        members: vec![first],
        pending: HashMap::new(),
        next_request: 0,
        ended_by_handler: false,
        asterisk_application,
        door,
        from_media,
        media_inbox,
        sounds_opened: 0,
    };
    Ok((conversation, hello))
}

impl Conversation<'_> {
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
                said = next_text(self.asterisk) => match said? {
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
                    Some(Ok(Frame::Binary(bytes))) => self.on_audio(&bytes).await?,
                    Some(Ok(Frame::Ping(_) | Frame::Pong(_) | Frame::Frame(_))) => {}
                    Some(Ok(Frame::Close(_))) | None => {
                        return Err(Fault::Application("lost the conversation connection".into()));
                    }
                    Some(Err(error)) => return Err(Fault::Application(error.to_string())),
                },
                Some(said) = self.from_media.recv() => self.on_media(said).await?,
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
            } => match self.pending.remove(&request_id) {
                Some(Pending::Command(id)) => {
                    let outcome = outcome_of(status_code)?;
                    self.tell(&ControllerMessage::CommandResult { id, outcome })
                        .await?;
                }
                Some(Pending::Sound(participant, step)) => {
                    self.sound_step_answered(&participant, step, status_code)
                        .await?;
                }
                Some(Pending::Internal) | None => {}
            },
            // The first channel has already entered. The only channels that
            // enter later are media channels the controller itself created;
            // participants are added by commands this build does not have yet.
            ari::Message::StasisStart { channel, .. } => {
                self.media_channel_entered(&channel.id).await?;
            }
            ari::Message::Other => {}
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
        let mut member = self.members.swap_remove(position);
        if let Some(mut sound) = member.sound.take() {
            // Nothing more will be played to them: every segment still
            // queued ends here, before the word that they are gone.
            let mut outcomes = Vec::new();
            sound.playout.abandon(&mut outcomes);
            self.report(&member.id, outcomes).await?;
            self.close_sound(&sound).await?;
        }
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
                self.accept(id).await
            }
            Command::Listen { participant } => self.listen(id, &participant).await,
            Command::StopListening { participant } => {
                let Some(member) = self.member(&participant) else {
                    return self.reject(id, CommandRejection::UnknownParticipant).await;
                };
                if let Some(sound) = &mut member.sound {
                    sound.listening = false;
                }
                self.accept(id).await
            }
            Command::Play {
                participant,
                segment,
            } => self.play(id, &participant, segment).await,
            Command::FlushPlayback { participant } => self.flush_playback(id, &participant).await,
            Command::Dial { .. } => Err(Fault::NotImplemented("dial")),
            Command::Connect { .. } => Err(Fault::NotImplemented("connect")),
            Command::Separate { .. } => Err(Fault::NotImplemented("separate")),
            Command::HoldFor { .. } => Err(Fault::NotImplemented("hold_for")),
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

    fn member(&mut self, participant: &ParticipantId) -> Option<&mut Member> {
        self.members
            .iter_mut()
            .find(|member| member.id == *participant)
    }

    fn sound(&mut self, participant: &ParticipantId) -> Option<&mut Sound> {
        self.member(participant)
            .and_then(|member| member.sound.as_mut())
    }

    async fn accept(&mut self, id: u64) -> Result<(), Fault> {
        let outcome = CommandOutcome::Accepted;
        self.tell(&ControllerMessage::CommandResult { id, outcome })
            .await
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

    /// The application asks to hear a participant.
    async fn listen(&mut self, id: u64, participant: &ParticipantId) -> Result<(), Fault> {
        if let Some(reason) = self.open_sound(participant).await? {
            return self.reject(id, reason).await;
        }
        if let Some(sound) = self.sound(participant) {
            sound.listening = true;
        }
        self.accept(id).await
    }

    /// The application queues a segment for a participant.
    async fn play(
        &mut self,
        id: u64,
        participant: &ParticipantId,
        segment: SegmentId,
    ) -> Result<(), Fault> {
        if let Some(reason) = self.open_sound(participant).await? {
            return self.reject(id, reason).await;
        }
        let queued = self
            .sound(participant)
            .is_some_and(|sound| sound.playout.queue(segment).is_ok());
        if queued {
            self.accept(id).await
        } else {
            self.reject(id, CommandRejection::SegmentAlreadyQueued)
                .await
        }
    }

    /// The application drops everything queued for a participant.
    async fn flush_playback(&mut self, id: u64, participant: &ParticipantId) -> Result<(), Fault> {
        let Some(member) = self.member(participant) else {
            return self.reject(id, CommandRejection::UnknownParticipant).await;
        };
        let mut to_medium = Vec::new();
        let mut outcomes = Vec::new();
        if let Some(sound) = &mut member.sound {
            sound.playout.flush(&mut to_medium, &mut outcomes);
            if let Some(sink) = &mut sound.sink {
                hand_over(sink, to_medium).await?;
            }
        }
        self.accept(id).await?;
        self.report(participant, outcomes).await
    }

    /// Make sure a participant has an audio path, opening one if they have
    /// none. Answers with the reason when they cannot have one.
    async fn open_sound(
        &mut self,
        participant: &ParticipantId,
    ) -> Result<Option<CommandRejection>, Fault> {
        let Some(member) = self
            .members
            .iter_mut()
            .find(|member| member.id == *participant)
        else {
            return Ok(Some(CommandRejection::UnknownParticipant));
        };
        if member.state != ParticipantState::InConversation {
            return Ok(Some(CommandRejection::ParticipantNotInRequiredState));
        }
        if member.sound.is_some() {
            return Ok(None);
        }

        self.sounds_opened += 1;
        let channel = format!("{}.media-{}", self.id, self.sounds_opened);
        member.sound = Some(Sound {
            channel: channel.clone(),
            bridge: format!("{}.bridge-{}", self.id, self.sounds_opened),
            sink: None,
            joined: false,
            listening: false,
            heard: Vec::new(),
            heard_ms: 0,
            playout: Playout::default(),
        });
        // Asterisk opens the media connection as soon as the channel exists:
        // whoever is to take it must be waiting before the channel is asked for.
        self.door.expect(
            channel.clone(),
            media::Waiter {
                participant: participant.clone(),
                conversation: self.media_inbox.clone(),
            },
        );
        let uri = format!(
            "channels/externalMedia?app={}&external_host={}&transport=websocket\
             &encapsulation=none&format={}&channelId={}&transport_data={}",
            ari::query(&self.asterisk_application),
            media::CONNECTION,
            media::FORMAT,
            ari::query(&channel),
            ari::query(media::OPTIONS),
        );
        self.ask(
            Pending::Sound(participant.clone(), SoundStep::MediaChannel),
            "POST",
            &uri,
        )
        .await?;
        Ok(None)
    }

    /// A channel entered the call's application. If it is the media channel
    /// of a participant, it is joined to that participant now.
    async fn media_channel_entered(&mut self, channel: &str) -> Result<(), Fault> {
        let path = self.members.iter().find_map(|member| {
            member
                .sound
                .as_ref()
                .filter(|sound| sound.channel == channel)
                .map(|sound| {
                    (
                        member.id.clone(),
                        member.channel.clone(),
                        sound.bridge.clone(),
                    )
                })
        });
        let Some((participant, participant_channel, bridge)) = path else {
            return Ok(());
        };
        self.ask(
            Pending::Sound(participant.clone(), SoundStep::Bridge),
            "POST",
            &format!("bridges?type=mixing&bridgeId={}", ari::query(&bridge)),
        )
        .await?;
        self.ask(
            Pending::Sound(participant, SoundStep::Join),
            "POST",
            &format!(
                "bridges/{bridge}/addChannel?channel={},{}",
                ari::query(&participant_channel),
                ari::query(channel),
            ),
        )
        .await
    }

    /// Asterisk answered a step of opening a participant's audio path.
    async fn sound_step_answered(
        &mut self,
        participant: &ParticipantId,
        step: SoundStep,
        status_code: u16,
    ) -> Result<(), Fault> {
        // A participant who has left meanwhile has no path to open: whatever
        // Asterisk answered about it no longer matters.
        let Some(sound) = self.sound(participant) else {
            return Ok(());
        };
        if !(200..=299).contains(&status_code) {
            return Err(Fault::Asterisk(format!(
                "answered with status {status_code} when asked to {step} for {participant}",
                participant = participant.as_str(),
            )));
        }
        if step == SoundStep::Join {
            sound.joined = true;
            return self.feed(participant).await;
        }
        Ok(())
    }

    /// End a participant's audio path in Asterisk. Asterisk then closes the
    /// media connection itself.
    async fn close_sound(&mut self, sound: &Sound) -> Result<(), Fault> {
        self.door.forget(&sound.channel);
        self.ask(
            Pending::Internal,
            "DELETE",
            &format!("channels/{}", sound.channel),
        )
        .await?;
        self.ask(
            Pending::Internal,
            "DELETE",
            &format!("bridges/{}", sound.bridge),
        )
        .await
    }

    /// Hand to a participant's media channel as much of their playback queue
    /// as it may be given now.
    async fn feed(&mut self, participant: &ParticipantId) -> Result<(), Fault> {
        let Some(sound) = self.sound(participant) else {
            return Ok(());
        };
        if !sound.joined {
            return Ok(());
        }
        let Some(sink) = &mut sound.sink else {
            return Ok(());
        };
        let mut to_medium = Vec::new();
        sound.playout.feed(&mut to_medium);
        hand_over(sink, to_medium).await
    }

    /// Tell the application what became of segments queued for a participant.
    async fn report(
        &mut self,
        participant: &ParticipantId,
        outcomes: Vec<Outcome>,
    ) -> Result<(), Fault> {
        for outcome in outcomes {
            let participant = participant.clone();
            let event = match outcome {
                Outcome::Started(segment) => Event::PlaybackStarted {
                    participant,
                    segment,
                },
                Outcome::Delivered(segment) => Event::PlaybackDelivered {
                    participant,
                    segment,
                },
                Outcome::Dropped {
                    segment,
                    delivered_ms,
                } => Event::PlaybackDropped {
                    participant,
                    segment,
                    delivered_ms,
                },
            };
            self.event(event).await?;
        }
        Ok(())
    }

    /// The application sent an audio frame: audio of a segment it queued.
    async fn on_audio(&mut self, bytes: &[u8]) -> Result<(), Fault> {
        match AudioFrame::decode(bytes).map_err(Fault::Frame)? {
            AudioFrame::Playback {
                participant,
                segment,
                last,
                audio,
            } => {
                // Audio for a participant who has left, or for a segment that
                // is not queued, is left out: the application sent it before
                // it could read that the participant was gone, the segment
                // dropped or its `play` rejected.
                if let Some(sound) = self.sound(&participant) {
                    sound.playout.audio(&segment, &audio, last);
                }
                self.feed(&participant).await
            }
            AudioFrame::Heard { .. } => Err(Fault::Application(
                "sent a frame of heard audio, which only the node sends".into(),
            )),
        }
    }

    /// A media connection of this conversation said something.
    async fn on_media(&mut self, said: FromMedia) -> Result<(), Fault> {
        match said {
            FromMedia::Connected { participant, sink } => {
                if let Some(sound) = self.sound(&participant) {
                    sound.sink = Some(sink);
                }
                self.feed(&participant).await
            }
            FromMedia::Audio { participant, audio } => self.heard(&participant, &audio).await,
            FromMedia::Said { participant, event } => self.media_said(&participant, event).await,
            FromMedia::Failed {
                participant,
                problem,
            } => match self.sound(&participant) {
                Some(_) => Err(Fault::Asterisk(format!(
                    "the media connection of {}: {problem}",
                    participant.as_str()
                ))),
                None => Ok(()),
            },
            // Asterisk closes a media connection when its channel ends. For
            // a participant still on the line nobody asked for that.
            FromMedia::Closed { participant } => match self.sound(&participant) {
                Some(_) => Err(Fault::Asterisk(format!(
                    "closed the media connection of {} while they are on the line",
                    participant.as_str()
                ))),
                None => Ok(()),
            },
        }
    }

    /// A participant said something: cut it into frames of twenty
    /// milliseconds and, if the application listens, pass them on.
    async fn heard(&mut self, participant: &ParticipantId, audio: &[u8]) -> Result<(), Fault> {
        let Some(sound) = self.sound(participant) else {
            return Ok(());
        };
        sound.heard.extend_from_slice(audio);
        let mut frames = Vec::new();
        while sound.heard.len() >= FRAME_BYTES {
            let frame = AudioFrame::Heard {
                participant: participant.clone(),
                position_ms: sound.heard_ms,
                audio: sound.heard.drain(..FRAME_BYTES).collect(),
            };
            sound.heard_ms += HEARD_FRAME_MS;
            if sound.listening {
                frames.push(frame.encode().map_err(Fault::Frame)?);
            }
        }
        for frame in frames {
            self.application
                .send(Frame::binary(frame))
                .await
                .map_err(|error| Fault::Application(error.to_string()))?;
        }
        Ok(())
    }

    /// Asterisk said something on a participant's media connection.
    async fn media_said(
        &mut self,
        participant: &ParticipantId,
        event: media::Event,
    ) -> Result<(), Fault> {
        let Some(sound) = self.sound(participant) else {
            return Ok(());
        };
        let mut outcomes = Vec::new();
        match event {
            media::Event::MarkReached { correlation_id } => {
                let mark = correlation_id.parse().map_err(|_| {
                    Fault::Asterisk(format!("reported a mark {correlation_id:?} nobody gave it"))
                })?;
                sound
                    .playout
                    .mark_reached(mark, &mut outcomes)
                    .map_err(|error| Fault::Asterisk(format!("reported a mark: {error}")))?;
            }
            media::Event::Status { queue_length } => {
                let mut to_medium = Vec::new();
                sound
                    .playout
                    .status(queue_length, &mut to_medium, &mut outcomes);
                if let Some(sink) = &mut sound.sink {
                    hand_over(sink, to_medium).await?;
                }
            }
            media::Event::Error { error_text } => {
                return Err(Fault::Asterisk(format!(
                    "refused something on the media connection of {}: {error_text}",
                    participant.as_str()
                )));
            }
            media::Event::Start { .. } | media::Event::Other => {}
        }
        self.report(participant, outcomes).await?;
        self.feed(participant).await
    }

    /// The conversation cannot go on: nobody may be left on a line that no
    /// one controls, and the application is told why its connection closes.
    async fn abandon(&mut self, fault: &Fault) {
        for member in &self.members {
            let mut uris = vec![format!("channels/{}", member.channel)];
            if let Some(sound) = &member.sound {
                self.door.forget(&sound.channel);
                uris.push(format!("channels/{}", sound.channel));
                uris.push(format!("bridges/{}", sound.bridge));
            }
            for uri in uris {
                let drop = ari::request("abandon", "DELETE", &uri);
                let _ = self.asterisk.send(Frame::text(drop)).await;
            }
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

/// Write to a media connection, in order, what a playback queue hands over.
async fn hand_over(sink: &mut media::Sink, to_medium: Vec<ToMedium>) -> Result<(), Fault> {
    let failed = |error: tokio_tungstenite::tungstenite::Error| {
        Fault::Asterisk(format!("media connection: {error}"))
    };
    for item in to_medium {
        let frame = match item {
            ToMedium::Audio(audio) => Frame::binary(audio),
            ToMedium::Mark(id) => Frame::text(media::mark(id)),
            ToMedium::Pause => Frame::text(media::command("PAUSE_MEDIA")),
            ToMedium::AskStatus => Frame::text(media::command("GET_STATUS")),
            ToMedium::Flush => Frame::text(media::command("FLUSH_MEDIA")),
            ToMedium::Continue => Frame::text(media::command("CONTINUE_MEDIA")),
        };
        sink.feed(frame).await.map_err(failed)?;
    }
    sink.flush().await.map_err(failed)
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
