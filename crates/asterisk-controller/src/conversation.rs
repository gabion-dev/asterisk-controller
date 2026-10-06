// crates/asterisk-controller/src/conversation.rs

//! One conversation: its participants in Asterisk on one side, the
//! application's conversation connection on the other, and the translation
//! between them.
//!
//! A call that arrives becomes a conversation. For each one the controller
//! opens one conversation connection to the application; the instance that
//! accepts it owns the conversation. From then on what Asterisk says about
//! the conversation's channels becomes events of the node protocol, and
//! commands of the application become requests to Asterisk
//! ([`crate::asterisk`]).
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
        Departure, EndReason, Event, LineId, Opening, Origin, Participant, ParticipantId,
        ParticipantMedium, PhoneNumber, RejectReason, SegmentId,
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
    asterisk::{FromAsterisk, Line},
    asterisk_files::{APPLICATION, operator_name},
    config::PROTOCOL_VERSION,
    destination,
    media::{self, FromMedia},
    node::{Node, Place},
    playout::{FRAME_BYTES, Outcome, Playout, ToMedium},
};

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

impl Member {
    /// Whether they called in and have not been answered yet.
    fn rings_in(&self) -> bool {
        self.state == ParticipantState::Ringing && !self.awaiting_answer
    }
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
    /// The node is calling them and they have not answered. Until they do,
    /// their channel is not in the node's application: it can be dropped
    /// and nothing else.
    awaiting_answer: bool,
    /// Their place on the outbound line they were called on; given back
    /// when they are gone.
    _place: Option<Place>,
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
///
/// A command of the application is answered at once, from what the
/// controller itself knows of the conversation, and what Asterisk then does
/// arrives as events. The answer to a command never waits for Asterisk —
/// with the one exception below.
enum Pending {
    /// The `dial` command: Asterisk's word that the call is being placed
    /// makes the participant known to the application.
    Dial(u64, Participant),
    /// A step of opening a participant's audio path.
    Sound(ParticipantId, SoundStep),
    /// Housekeeping of the controller; nobody waits for the answer.
    Internal,
}

struct Conversation {
    id: String,
    node: Arc<Node>,
    asterisk: Line,
    application: ApplicationSocket,
    members: Vec<Member>,
    pending: HashMap<String, Pending>,
    next_request: u64,
    /// Set by the `end` command: the conversation ends when the last
    /// participant is gone, and it ends because the handler said so.
    ended_by_handler: bool,
    /// What the media connections of this conversation say.
    from_media: mpsc::Receiver<FromMedia>,
    /// The other end of `from_media`, given to each media connection.
    media_inbox: mpsc::Sender<FromMedia>,
    /// How many audio paths have been opened: names their channels.
    sounds_opened: u64,
    /// How many participants the conversation has had: names the next one.
    participants: u64,
    /// Participants being called whom the application does not know yet,
    /// with what has happened to them meanwhile. The application learns of
    /// such a participant from the answer to its `dial` command, and that
    /// answer waits for Asterisk's; events about them wait with it.
    unannounced: HashMap<ParticipantId, Vec<Event>>,
}

/// The channel a call arrived on.
pub struct Arrival {
    /// The channel.
    pub channel: ari::Channel,
    /// The entry it came through.
    pub origin: Origin,
}

/// Carry one conversation from its first participant to its end.
pub async fn carry(id: String, asterisk: Line, arrival: Arrival, node: Arc<Node>) {
    let outcome = async {
        let (mut conversation, hello) = open(&id, asterisk, arrival, node).await?;
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

/// The entry a channel came through, from the arguments the dialplan gave.
///
/// The dialplan is written by the controller, so the arguments are its own
/// words: `dialed_number,<number>`.
pub fn entry(arguments: &[String]) -> Result<Origin, Fault> {
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
    asterisk: Line,
    arrival: Arrival,
    node: Arc<Node>,
) -> Result<(Conversation, ControllerMessage), Fault> {
    let Arrival { channel, origin } = arrival;
    let config = &node.config;
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
        awaiting_answer: false,
        _place: None,
    };

    let connected = connect_async(config.application_url.as_str()).await;
    let application = match connected {
        Ok((socket, _response)) => socket,
        Err(error) => {
            asterisk.ask("no-owner", "DELETE", &format!("channels/{}", first.channel));
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
        node: Arc::clone(&node),
        asterisk,
        application,
        members: vec![first],
        pending: HashMap::new(),
        next_request: 0,
        ended_by_handler: false,
        from_media,
        media_inbox,
        sounds_opened: 0,
        participants: 1,
        unannounced: HashMap::new(),
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
        // The conversation lasts while it has a participant — or is still
        // owed Asterisk's word about one it tried to call: the application
        // has a `dial` command waiting for its answer.
        while !(self.members.is_empty() && self.unannounced.is_empty()) {
            tokio::select! {
                said = self.asterisk.next() => match said {
                    FromAsterisk::Said(message) => self.on_asterisk(message).await?,
                    FromAsterisk::Answered { request, status } => {
                        self.answered(&request, status).await?;
                    }
                    // Whoever is still listed left with Asterisk.
                    FromAsterisk::Gone => self.asterisk_gone().await?,
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
                        && !member.awaiting_answer
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
            ari::Message::StasisEnd { channel } => self.left(&channel.id, None).await?,
            // A participant who was being called and did not answer was
            // never in the application: the end of their channel is the
            // only word of them. For anyone else it comes after the word
            // that they left, and finds nobody.
            ari::Message::ChannelDestroyed { cause, channel } => {
                self.left(&channel.id, Some(cause)).await?;
            }
            ari::Message::StasisStart { channel, .. } => self.entered(&channel.id).await?,
            ari::Message::Other => {}
        }
        Ok(())
    }

    /// Asterisk answered a request of this conversation.
    async fn answered(&mut self, request_id: &str, status_code: u16) -> Result<(), Fault> {
        match self.pending.remove(request_id) {
            Some(Pending::Dial(id, participant)) => {
                self.call_placed(id, participant, status_code).await
            }
            Some(Pending::Sound(participant, step)) => {
                self.sound_step_answered(&participant, step, status_code)
                    .await
            }
            Some(Pending::Internal) | None => Ok(()),
        }
    }

    /// A channel of this conversation entered the node's application: a
    /// participant who was being called has answered, or the media channel
    /// of a participant is ready to be joined to them.
    async fn entered(&mut self, channel: &str) -> Result<(), Fault> {
        let answered = self
            .members
            .iter_mut()
            .find(|member| member.channel == channel && member.awaiting_answer);
        if let Some(member) = answered {
            member.awaiting_answer = false;
            member.state = ParticipantState::InConversation;
            let participant = member.id.clone();
            return self
                .about(
                    &participant.clone(),
                    Event::ParticipantAnswered { participant },
                )
                .await;
        }
        self.media_channel_entered(channel);
        Ok(())
    }

    /// A channel is gone: its participant left. `cause` is the telephone
    /// network's reason, when the channel ended without ever having been in
    /// the application.
    async fn left(&mut self, channel: &str, cause: Option<i64>) -> Result<(), Fault> {
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
            self.close_sound(&sound);
        }
        let departure = if member.removed_by_us {
            Departure::Removed
        } else if member.awaiting_answer {
            unanswered(cause)
        } else {
            Departure::HungUp
        };
        let participant = member.id.clone();
        self.about(
            &participant.clone(),
            Event::ParticipantLeft {
                participant,
                departure,
            },
        )
        .await
    }

    /// Tell the application something about a participant — at once, or,
    /// while it does not know the participant yet, as soon as it does.
    async fn about(&mut self, participant: &ParticipantId, event: Event) -> Result<(), Fault> {
        match self.unannounced.get_mut(participant) {
            Some(waiting) => {
                waiting.push(event);
                Ok(())
            }
            None => self.event(event).await,
        }
    }

    async fn asterisk_gone(&mut self) -> Result<(), Fault> {
        let channels: Vec<String> = self
            .members
            .iter()
            .map(|member| member.channel.clone())
            .collect();
        for channel in channels {
            self.left(&channel, None).await?;
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
            Command::Answer { participant } => self.answer(id, &participant).await,
            Command::Reject {
                participant,
                reason,
            } => self.turn_away(id, &participant, reason).await,
            Command::Remove { participant } => {
                let Some(member) = self.member(&participant) else {
                    return self.reject(id, CommandRejection::UnknownParticipant).await;
                };
                member.removed_by_us = true;
                let channel = member.channel.clone();
                self.ask(Pending::Internal, "DELETE", &format!("channels/{channel}"));
                self.accept(id).await
            }
            Command::Dial {
                number,
                line,
                answer_limit_ms,
            } => self.dial(id, &number, &line, answer_limit_ms).await,
            Command::SendDigits {
                participant,
                digits,
            } => self.send_digits(id, &participant, digits.as_str()).await,
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
                    self.ask(Pending::Internal, "DELETE", &format!("channels/{channel}"));
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
            Command::Connect { .. } => Err(Fault::NotImplemented("connect")),
            Command::Separate { .. } => Err(Fault::NotImplemented("separate")),
            Command::HoldFor { .. } => Err(Fault::NotImplemented("hold_for")),
            Command::StartRecording { .. } => Err(Fault::NotImplemented("start_recording")),
            Command::StopRecording { .. } => Err(Fault::NotImplemented("stop_recording")),
        }
    }

    /// The participant a command is about, if they are in the state the
    /// command needs; otherwise the reason the command is rejected for.
    fn member_who(
        &mut self,
        participant: &ParticipantId,
        is_ready: impl Fn(&Member) -> bool,
    ) -> Result<&mut Member, CommandRejection> {
        match self.member(participant) {
            None => Err(CommandRejection::UnknownParticipant),
            Some(member) if is_ready(member) => Ok(member),
            Some(_) => Err(CommandRejection::ParticipantNotInRequiredState),
        }
    }

    /// Answer a participant who called in and still rings. One the node is
    /// calling answers by themselves.
    async fn answer(&mut self, id: u64, participant: &ParticipantId) -> Result<(), Fault> {
        let channel = match self.member_who(participant, Member::rings_in) {
            Ok(member) => member.channel.clone(),
            Err(reason) => return self.reject(id, reason).await,
        };
        self.ask(
            Pending::Internal,
            "POST",
            &format!("channels/{channel}/answer"),
        );
        self.accept(id).await
    }

    /// Turn away a call that still rings; the caller's network is told why.
    async fn turn_away(
        &mut self,
        id: u64,
        participant: &ParticipantId,
        reason: RejectReason,
    ) -> Result<(), Fault> {
        let channel = match self.member_who(participant, Member::rings_in) {
            Ok(member) => {
                member.removed_by_us = true;
                member.channel.clone()
            }
            Err(reason) => return self.reject(id, reason).await,
        };
        let said = match reason {
            RejectReason::Busy => "busy",
            RejectReason::Declined => "rejected",
            RejectReason::RateLimited => "congestion",
        };
        self.ask(
            Pending::Internal,
            "DELETE",
            &format!("channels/{channel}?reason={said}"),
        );
        self.accept(id).await
    }

    /// Send tone digits to a participant who is in the conversation.
    async fn send_digits(
        &mut self,
        id: u64,
        participant: &ParticipantId,
        digits: &str,
    ) -> Result<(), Fault> {
        let in_conversation = |member: &Member| member.state == ParticipantState::InConversation;
        let channel = match self.member_who(participant, in_conversation) {
            Ok(member) => member.channel.clone(),
            Err(reason) => return self.reject(id, reason).await,
        };
        self.ask(
            Pending::Internal,
            "POST",
            &format!("channels/{channel}/dtmf?dtmf={}", ari::query(digits)),
        );
        self.accept(id).await
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

    /// Send a request to Asterisk; what its answer is for is remembered.
    fn ask(&mut self, pending: Pending, method: &str, uri: &str) {
        self.next_request += 1;
        let request = self.next_request.to_string();
        self.pending.insert(request.clone(), pending);
        self.asterisk.ask(&request, method, uri);
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
        if let Some(reason) = self.open_sound(participant) {
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
        if let Some(reason) = self.open_sound(participant) {
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
    fn open_sound(&mut self, participant: &ParticipantId) -> Option<CommandRejection> {
        let Some(member) = self
            .members
            .iter_mut()
            .find(|member| member.id == *participant)
        else {
            return Some(CommandRejection::UnknownParticipant);
        };
        if member.state != ParticipantState::InConversation {
            return Some(CommandRejection::ParticipantNotInRequiredState);
        }
        if member.sound.is_some() {
            return None;
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
        self.asterisk.own(&channel);
        self.node.door.expect(
            channel.clone(),
            media::Waiter {
                participant: participant.clone(),
                conversation: self.media_inbox.clone(),
            },
        );
        let uri = format!(
            "channels/externalMedia?app={}&external_host={}&transport=websocket\
             &encapsulation=none&format={}&channelId={}&transport_data={}",
            ari::query(APPLICATION),
            media::CONNECTION,
            media::FORMAT,
            ari::query(&channel),
            ari::query(media::OPTIONS),
        );
        self.ask(
            Pending::Sound(participant.clone(), SoundStep::MediaChannel),
            "POST",
            &uri,
        );
        None
    }

    /// If a channel that entered the application is the media channel of a
    /// participant, it is joined to that participant now.
    fn media_channel_entered(&mut self, channel: &str) {
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
            return;
        };
        self.ask(
            Pending::Sound(participant.clone(), SoundStep::Bridge),
            "POST",
            &format!("bridges?type=mixing&bridgeId={}", ari::query(&bridge)),
        );
        self.ask(
            Pending::Sound(participant, SoundStep::Join),
            "POST",
            &format!(
                "bridges/{bridge}/addChannel?channel={},{}",
                ari::query(&participant_channel),
                ari::query(channel),
            ),
        );
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
    fn close_sound(&self, sound: &Sound) {
        self.node.door.forget(&sound.channel);
        for gone in [
            format!("channels/{}", sound.channel),
            format!("bridges/{}", sound.bridge),
        ] {
            self.asterisk.ask("sound-closed", "DELETE", &gone);
        }
    }

    /// The application adds a participant by dialling.
    async fn dial(
        &mut self,
        id: u64,
        number: &PhoneNumber,
        line: &LineId,
        answer_limit_ms: u64,
    ) -> Result<(), Fault> {
        let node = Arc::clone(&self.node);
        let route = match destination::judge(&node.settings, line, number) {
            Ok(route) => route,
            Err(reason) => return self.reject(id, reason).await,
        };
        let Some(place) = node.lines.take(route.line) else {
            return self
                .reject(id, CommandRejection::OutboundLimitReached)
                .await;
        };

        self.participants += 1;
        let participant = Participant {
            id: participant_id(self.participants)?,
            medium: ParticipantMedium::TelephoneNetwork,
            number: Some(number.clone()),
        };
        let channel = format!("{}.participant-{}", self.id, self.participants);
        self.asterisk.own(&channel);
        self.members.push(Member {
            id: participant.id.clone(),
            channel: channel.clone(),
            state: ParticipantState::Ringing,
            removed_by_us: false,
            sound: None,
            awaiting_answer: true,
            _place: Some(place),
        });
        self.unannounced.insert(participant.id.clone(), Vec::new());

        // Asterisk counts the wait for an answer in whole seconds.
        let wait_seconds = answer_limit_ms.div_ceil(1000).max(1);
        let uri = format!(
            "channels?endpoint={}&app={}&callerId={}&timeout={wait_seconds}&channelId={}",
            ari::query(&format!(
                "PJSIP/{}@{}",
                number.as_str(),
                operator_name(route.operator)
            )),
            ari::query(APPLICATION),
            // The number shown is the line's: no command chooses it.
            ari::query(route.line.number.as_str()),
            ari::query(&channel),
        );
        self.ask(Pending::Dial(id, participant), "POST", &uri);
        Ok(())
    }

    /// Asterisk answered the request to place a call. Only now does the
    /// application learn of the participant — or that there is none.
    async fn call_placed(
        &mut self,
        id: u64,
        participant: Participant,
        status_code: u16,
    ) -> Result<(), Fault> {
        let waiting = self.unannounced.remove(&participant.id).unwrap_or_default();
        if !(200..=299).contains(&status_code) {
            // Asterisk could not even begin: the operator's trunk is not
            // there for it. Nobody was called.
            self.members.retain(|member| member.id != participant.id);
            return self
                .reject(id, CommandRejection::NoOperatorForDestination)
                .await;
        }
        let outcome = CommandOutcome::AcceptedParticipant {
            participant: participant.id.clone(),
        };
        self.tell(&ControllerMessage::CommandResult { id, outcome })
            .await?;
        self.event(Event::ParticipantRinging { participant })
            .await?;
        for event in waiting {
            self.event(event).await?;
        }
        Ok(())
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
            self.asterisk
                .ask("abandon", "DELETE", &format!("channels/{}", member.channel));
            if let Some(sound) = &member.sound {
                self.close_sound(sound);
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

/// Why a participant who was being called is gone without having answered,
/// from the telephone network's reason for the end of their channel.
fn unanswered(cause: Option<i64>) -> Departure {
    match cause {
        Some(17) => Departure::Busy,
        // No answer in the time allowed, or no response at all.
        Some(18 | 19) | None => Departure::NotAnswered,
        Some(medium_code) => Departure::DialFailed(medium_code),
    }
}

fn participant_id(number: u64) -> Result<ParticipantId, Fault> {
    format!("p-{number}")
        .parse()
        .map_err(|_| Fault::Asterisk("participant identifier is not valid".into()))
}

/// The next text message of the conversation connection; `None` when it
/// has closed.
async fn next_text(socket: &mut ApplicationSocket) -> Result<Option<String>, Fault> {
    loop {
        match socket.next().await {
            Some(Ok(Frame::Text(text))) => return Ok(Some(text.as_str().to_owned())),
            Some(Ok(Frame::Close(_))) | None => return Ok(None),
            Some(Ok(Frame::Binary(_) | Frame::Ping(_) | Frame::Pong(_) | Frame::Frame(_))) => {}
            Some(Err(error)) => return Err(Fault::Application(error.to_string())),
        }
    }
}
