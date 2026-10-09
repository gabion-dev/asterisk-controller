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
//!
//! Participants the application connects to each other are put into a bridge
//! of their own — a group. A participant's audio path does not follow them
//! there: everyone in the group would hear what the application says to one
//! of them, and the application would hear them all at once. A tap on the
//! participant's channel takes their place beside the media channel instead.
//! What the participant says comes out of the tap, and what is put into it
//! the participant alone hears, on top of the others. The media channel and
//! its connection stay as they are, so the application sees no change.
//!
//! A conversation outlives the controller too. What only the controller
//! knows of a participant — which conversation they are in, who they are
//! in it, where it came from, how long they are held when their audio is
//! lost — it writes on their channel in Asterisk as it learns it; a
//! controller that starts beside a running Asterisk reads it back and
//! carries the conversation on from there, as one without an application.
//!
//! A conversation outlives the application's connection. When the instance
//! that owns it is lost — or none takes it to begin with — the controller
//! stands in: it holds together people who are connected to each other and
//! looks for an instance that will take the conversation, gives a caller who
//! was alone with the application the fallback of their entry, and hangs up
//! whoever is left with nobody to talk to and nobody in charge. An instance
//! that takes the conversation is told what it is like now, and nothing of
//! what its predecessor had asked for.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    num::NonZeroU64,
    sync::Arc,
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use node_protocol::{
    DecodeError,
    audio::{AudioFrame, AudioFrameError},
    messages::{
        ApplicationMessage, Command, CommandOutcome, CommandRejection, ControllerMessage,
        ConversationId, Departure, EndReason, EntryKey, Event, Fallback, FallbackFailure,
        FallbackKind, LineId, Opening, Origin, Participant, ParticipantId, ParticipantMedium,
        ParticipantSnapshot, ParticipantState as StateTold, PhoneNumber, RejectReason, Report,
        RequestOutcome, RequestRejection, SegmentId,
    },
};
use tokio::{sync::mpsc, time::Instant};
use tokio_tungstenite::tungstenite::{
    Message as Frame,
    protocol::{CloseFrame, frame::coding::CloseCode},
};

use crate::{
    application::{Endpoint, PING_EVERY, SILENCE_LIMIT, Socket as ApplicationSocket},
    ari,
    asterisk::{FromAsterisk, Line},
    asterisk_files::{APPLICATION, FALLBACK_LABEL, NETWORK_CONTEXT, operator_name},
    config::PROTOCOL_VERSION,
    destination,
    media::{self, FromMedia},
    node::{Node, Place},
    playout::{FRAME_BYTES, Outcome, Playout, ToMedium},
    prompts, reports,
};

/// Milliseconds of audio in one heard frame.
const HEARD_FRAME_MS: u64 = 20;
/// How long a participant's audio may stop arriving before the node says it
/// is lost. Audio arrives in quiet moments too — packets of silence — so
/// what is waited for is not speech; five seconds is the author's decision,
/// longer than the four seconds a real network was seen to drop audio for
/// and recover by itself.
const MEDIUM_LOST_AFTER: Duration = Duration::from_secs(5);
/// How often Asterisk is asked how much audio each participant has sent.
/// Asterisk says nothing when audio stops: its count is the state, read at
/// this pace — which also bounds how late the end of a held participant's
/// limit is noticed.
const MEDIUM_READ_EVERY: Duration = Duration::from_secs(1);
/// How much a media connection may say before the conversation has taken it
/// in. Past this the connection waits, and Asterisk with it: audio is never
/// piled up without bound behind an application that does not read.
const MEDIA_BACKLOG: usize = 256;
/// How long the controller waits before it looks again for an instance of
/// the application, while it keeps people connected with none. Until an
/// instance listens there is no event to wait for: its listening is the
/// state, and this is how often the state is read.
const LOOK_FOR_OWNER: Duration = Duration::from_secs(1);

/// Why a conversation could not be carried on.
#[derive(Debug)]
pub enum Fault {
    /// Asterisk's connection failed or said something unreadable.
    Asterisk(String),
    /// No instance of the application owns the conversation: none could be
    /// reached, the one that was reached would not take it, or the one that
    /// had it is lost. The conversation goes on without one.
    Owner(String),
    /// The application did what an owner may not do.
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
            Self::Owner(problem) | Self::Application(problem) => {
                write!(f, "application: {problem}")
            }
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
    /// Whether the audio they send is watched: they have answered, the node
    /// is not dropping them, and their channel carries audio Asterisk
    /// counts — a SIP channel. A channel of another kind has no such count.
    fn medium_watched(&self) -> bool {
        self.state == ParticipantState::InConversation
            && !self.removed_by_us
            && !self.watch.unwatchable
            && self
                .name
                .as_deref()
                .is_some_and(|name| name.starts_with("PJSIP/"))
    }

    /// Whether they called in and have not been answered yet.
    fn rings_in(&self) -> bool {
        self.state == ParticipantState::Ringing && !self.awaiting_answer
    }

    /// The participant, as the application is told of them.
    fn known(&self) -> Participant {
        Participant {
            id: self.id.clone(),
            medium: ParticipantMedium::TelephoneNetwork,
            number: self.number.clone(),
        }
    }
}

/// How far the fallback of the conversation's entry has got. It is carried
/// out once, by the controller, for a caller left alone with no application.
enum FallbackProgress {
    NotBegun,
    /// The number of the fallback is being called; this is who was dialled.
    Calling(ParticipantId),
    /// The one who was dialled has answered, and the caller is being
    /// answered to be connected to them.
    Answering(ParticipantId),
    /// It has been carried out, or could not be: there is nothing more to
    /// do for a caller who is alone.
    Done,
}

struct Member {
    id: ParticipantId,
    /// Their number, when the medium gave it in full form. One it did not
    /// is not guessed at: the participant then has none.
    number: Option<PhoneNumber>,
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
    /// The group they are connected in, while they are connected to others.
    group: Option<u64>,
    /// Where Asterisk has been asked to put their channel.
    put: Put,
    /// Asterisk's name of their channel, once Asterisk has said it.
    name: Option<String>,
    /// What is known of the audio they send.
    watch: Watch,
    /// Why they leave, when the node drops them for a reason of its own.
    leaving: Option<Departure>,
}

/// What the node knows of the audio a participant sends: how many packets
/// Asterisk has received from them, and what follows from that. Whether
/// they are held is an axis of its own, beside whether they have answered.
struct Watch {
    /// The count last read; none yet.
    received: Option<u64>,
    /// When the count last grew — or watching began.
    grew_at: Instant,
    /// Their far end has put the call on hold: no audio is expected.
    far_end_on_hold: bool,
    /// Their audio stopped arriving and they are held, since then.
    held_since: Option<Instant>,
    /// How long they stay held before they count as gone (`hold_for`).
    hold_limit: Duration,
    /// A count has been asked for and not answered yet.
    asking: bool,
    /// Asterisk keeps no count for their channel.
    unwatchable: bool,
}

impl Watch {
    /// Watching begins, with the hold limit the participant has.
    fn new(hold_limit: Duration) -> Self {
        Self {
            received: None,
            grew_at: Instant::now(),
            far_end_on_hold: false,
            held_since: None,
            hold_limit,
            asking: false,
            unwatchable: false,
        }
    }
}

/// Where a participant's channel has been asked to be.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Put {
    /// In no bridge.
    Nowhere,
    /// Beside their media channel, in the bridge of their audio path.
    BesideMedia,
    /// In the bridge of a group.
    InGroup(u64),
}

/// The audio path between the application and one participant.
///
/// In Asterisk it is a media channel and a bridge that joins it to the
/// participant's channel: what the participant says comes out of the media
/// channel, what is put into it the participant hears. While the participant
/// is connected to others, a tap on their channel is in that bridge in their
/// place.
struct Sound {
    /// Asterisk's identifier of the media channel.
    channel: String,
    /// Asterisk's identifier of the bridge.
    bridge: String,
    /// Where audio for the participant is written; `None` until Asterisk
    /// has opened the media connection.
    sink: Option<media::Sink>,
    /// How far building the path in Asterisk has got.
    built: Built,
    /// Who has been asked into the bridge beside the media channel.
    beside: Beside,
    /// The tap on the participant's channel, while there is one.
    tap: Option<Tap>,
    /// The media channel has been joined to the participant, or to a tap on
    /// them. Until then nothing is handed to the media channel: it would
    /// play to no one and report it as played.
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

/// How far building an audio path in Asterisk has got.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Built {
    /// The media channel has been asked for.
    Asked,
    /// The media channel is in the node's application, and the bridge has
    /// been asked for.
    Entered,
    /// The media channel has been asked into the bridge.
    Bridged,
}

/// Who is in the bridge of an audio path beside the media channel.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Beside {
    Nobody,
    /// The participant: they are connected to no one else.
    Participant,
    /// The tap on the participant: they are in a group.
    Tap,
}

/// A tap on a participant's channel: Asterisk's snoop channel that hears
/// what the participant says and whispers to them what is put into it.
struct Tap {
    /// Asterisk's identifier of the tap.
    channel: String,
    /// The tap is in the node's application: it can be put into a bridge.
    entered: bool,
}

/// What the controller writes on a participant's channel in Asterisk, for a
/// controller that comes after it: which conversation the participant is
/// in, who they are in it, where the conversation came from — for one the
/// application asked for, its request and handler too — their number, the
/// line they were called on and how long they are held when their audio is
/// lost.
pub const NOTES: &[&str] = &[
    NOTE_CONVERSATION,
    NOTE_PARTICIPANT,
    NOTE_ORIGIN,
    NOTE_NUMBER,
    NOTE_LINE,
    NOTE_HOLD_LIMIT,
    NOTE_REQUEST,
    NOTE_HANDLER,
];
const NOTE_CONVERSATION: &str = "GABION_CONVERSATION";
const NOTE_PARTICIPANT: &str = "GABION_PARTICIPANT";
const NOTE_ORIGIN: &str = "GABION_ORIGIN";
const NOTE_NUMBER: &str = "GABION_NUMBER";
const NOTE_LINE: &str = "GABION_LINE";
const NOTE_HOLD_LIMIT: &str = "GABION_HOLD_LIMIT_MS";
const NOTE_REQUEST: &str = "GABION_REQUEST";
const NOTE_HANDLER: &str = "GABION_HANDLER";
/// How a conversation the application asked for is written as its origin;
/// its request and handler are notes of their own, whatever they hold.
const STARTED_BY_CODE: &str = "started_by_code";

/// A conversation found in Asterisk by a controller that started beside it.
pub struct Found {
    /// Its identifier.
    pub id: String,
    /// How it came to exist.
    pub origin: Origin,
    /// Its participants.
    pub members: Vec<FoundMember>,
}

impl Found {
    /// A conversation nobody has been found in yet.
    pub fn new(id: String, origin: Origin) -> Self {
        Self {
            id,
            origin,
            members: Vec::new(),
        }
    }
}

/// A participant found in Asterisk.
pub struct FoundMember {
    /// Who they are in the conversation.
    pub participant: ParticipantId,
    /// Their channel.
    pub channel: String,
    /// Asterisk's name of their channel.
    pub name: String,
    /// Their number, when known.
    pub number: Option<PhoneNumber>,
    /// They have answered.
    pub answered: bool,
    /// The node is calling them and they have not answered.
    pub awaiting_answer: bool,
    /// The line they were called on.
    pub line: Option<String>,
    /// The bridge of the group they are in.
    pub group: Option<String>,
    /// How long they are held when their audio is lost: the limit an owner
    /// set, kept on their channel.
    pub hold_limit: Duration,
}

/// A channel of the node's application, as a participant of a conversation
/// — when what a controller wrote on it says so.
pub fn found(
    channel: &ari::Channel,
    notes: &HashMap<&str, String>,
) -> Option<(String, Origin, FoundMember)> {
    let conversation = notes.get(NOTE_CONVERSATION)?.clone();
    let participant = notes.get(NOTE_PARTICIPANT)?.parse().ok()?;
    let origin = match notes.get(NOTE_ORIGIN)?.as_str() {
        STARTED_BY_CODE => Origin::StartedByCode {
            request: notes.get(NOTE_REQUEST)?.parse().ok()?,
            handler: notes.get(NOTE_HANDLER)?.parse().ok()?,
        },
        // The words of the dialplan.
        arguments => {
            let arguments: Vec<String> = arguments.split(',').map(str::to_owned).collect();
            entry(&arguments).ok()?
        }
    };
    let in_application = channel.is_in(APPLICATION);
    let member = FoundMember {
        participant,
        channel: channel.id.clone(),
        name: channel.name.clone(),
        number: notes
            .get(NOTE_NUMBER)
            .and_then(|number| number.parse().ok()),
        answered: in_application && channel.state == ari::STATE_UP,
        // One the node is calling is not in the application until they answer.
        awaiting_answer: !in_application,
        line: notes.get(NOTE_LINE).cloned(),
        group: None,
        // Written by a controller only as a count of milliseconds; with none
        // written, no owner set a limit.
        hold_limit: notes
            .get(NOTE_HOLD_LIMIT)
            .and_then(|ms| ms.parse().ok())
            .map_or(Duration::ZERO, Duration::from_millis),
    };
    Some((conversation, origin, member))
}

/// The conversation a group's bridge belongs to, from the bridge's
/// identifier; `None` for a bridge that is no group's.
pub fn group_of(bridge: &str) -> Option<String> {
    let (conversation, _) = bridge.split_once('.')?;
    bridge.contains(".group-").then(|| conversation.to_owned())
}

/// Carry on a conversation found in Asterisk by a controller that started
/// beside it.
pub async fn carry_on(found: Found, asterisk: Line, node: Arc<Node>) {
    let id = found.id.clone();
    let mut conversation = Conversation::resume(found, asterisk, node);
    let outcome = conversation.live(None).await;
    if let Err(fault) = &outcome {
        conversation.abandon(fault).await;
    }
    match outcome {
        Ok(()) => eprintln!("conversation {id}: ended"),
        Err(fault) => eprintln!("conversation {id}: FAILED — {fault}"),
    }
}

/// A conversation the application asks the node to start by dialling its
/// first participant, and where the answer to the request goes.
pub struct Asked {
    /// How the conversation comes to exist: the request and its handler.
    pub origin: Origin,
    /// The number of the first participant.
    pub number: PhoneNumber,
    /// The line they are called on.
    pub line: LineId,
    /// How long they may take to answer.
    pub answer_limit_ms: u64,
    /// Where the answer to the request goes.
    pub answer: tokio::sync::oneshot::Sender<RequestOutcome>,
}

/// Start the conversation the application asked for, and carry it to its
/// end.
///
/// The request is answered once Asterisk has said whether the call to the
/// first participant is placed: with the conversation, or with why there is
/// none. Only then is the conversation offered — opened as one started by
/// code, its first participant ringing — to whichever instance takes it,
/// not necessarily the one that asked.
pub async fn carry_by_code(id: String, asterisk: Line, asked: Asked, node: Arc<Node>) {
    let Asked {
        origin,
        number,
        line,
        answer_limit_ms,
        answer,
    } = asked;
    let refused = |reason| RequestOutcome::Rejected { reason };
    let Ok(started) = id.parse() else {
        eprintln!("conversation {id}: FAILED — the conversation's identifier is not valid");
        let _ = answer.send(refused(RequestRejection::CallNotPlaced));
        return;
    };
    let mut conversation = Conversation::blank(id.clone(), asterisk, node, origin.clone());
    let outcome = match conversation
        .place_first(&number, &line, answer_limit_ms)
        .await
    {
        Ok(Ok(first)) => {
            // The one who asked may be gone by now; the conversation is
            // offered all the same.
            let _ = answer.send(RequestOutcome::ConversationStarted {
                conversation: started,
            });
            let result = conversation
                .live(Some(Opening::Started { origin, first }))
                .await;
            if let Err(fault) = &result {
                conversation.abandon(fault).await;
            }
            result
        }
        // Nobody was called: there is no conversation.
        Ok(Err(rejection)) => {
            let _ = answer.send(refused(request_rejection(rejection)));
            return;
        }
        // Whoever was being called is dropped with the conversation: for
        // the one who asked, the call was not placed.
        Err(fault) => {
            let _ = answer.send(refused(RequestRejection::CallNotPlaced));
            conversation.abandon(&fault).await;
            Err(fault)
        }
    };
    match outcome {
        Ok(()) => eprintln!("conversation {id}: ended"),
        Err(fault) => eprintln!("conversation {id}: FAILED — {fault}"),
    }
}

/// Why a request to start a conversation is refused, from why the call to
/// its first participant was not placed.
fn request_rejection(reason: CommandRejection) -> RequestRejection {
    match reason {
        CommandRejection::UnknownLine => RequestRejection::UnknownLine,
        CommandRejection::DestinationNotAllowed => RequestRejection::DestinationNotAllowed,
        CommandRejection::NoOperatorForDestination => RequestRejection::NoOperatorForDestination,
        CommandRejection::OutboundLimitReached => RequestRejection::OutboundLimitReached,
        // A call is refused for none of the others; were one, the call was
        // not placed all the same.
        CommandRejection::CallNotPlaced
        | CommandRejection::UnknownParticipant
        | CommandRejection::ParticipantNotInRequiredState
        | CommandRejection::TooFewParticipants
        | CommandRejection::UnknownRecording
        | CommandRejection::SegmentAlreadyQueued
        | CommandRejection::TelephonyServerUnavailable => RequestRejection::CallNotPlaced,
    }
}

/// A step of putting a participant and their audio path where they should
/// be, as asked of Asterisk.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    MediaChannel,
    Bridge,
    Join,
    Tap,
    EnterGroup,
    LeaveGroup,
    Answer,
    TurnAway,
    HangUp,
    SendDigits,
    Note,
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MediaChannel => "create a media channel",
            Self::Bridge => "create a bridge",
            Self::Join => "join the media channel and the participant",
            Self::Tap => "tap the participant's channel",
            Self::EnterGroup => "put the participant into a group",
            Self::LeaveGroup => "take the participant out of a group",
            Self::Answer => "answer the call",
            Self::TurnAway => "turn the call away",
            Self::HangUp => "hang up the participant",
            Self::SendDigits => "send tone digits to the participant",
            Self::Note => "write on the participant's channel what the controller knows of them",
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
    /// A call the node places: Asterisk's word that it is being placed
    /// makes the participant known to whoever asked for the call.
    Dial(Asker, Participant),
    /// A step of putting a participant and their audio path in place.
    Step(ParticipantId, Step),
    /// Whether a participant's channel is still in the node's application:
    /// asked when Asterisk has refused a step about them with this status.
    Verdict(ParticipantId, Step, u16),
    /// The bridge of a group.
    Group,
    /// Whether a participant found at a restart is still there.
    Exists(ParticipantId),
    /// A caller sent to the message of their entry's fallback: their
    /// channel, and who they are.
    Message(String, ParticipantId),
    /// How much audio a participant has sent.
    Medium(ParticipantId),
    /// Housekeeping of the controller; nobody waits for the answer.
    Internal,
}

/// Who a call the node places is for: who learns of its participant when
/// Asterisk says it is being placed.
#[derive(Clone, Copy)]
enum Asker {
    /// The owner's `dial` command of this number, waiting for its answer.
    Command(u64),
    /// The application's request to start the conversation: the call is
    /// its first participant.
    Request,
    /// Nobody: the call is the controller's own, or the instance that
    /// asked is gone.
    Nobody,
}

/// A request to Asterisk that is still to be made: what its answer is for,
/// its method and its address.
type Request = (Pending, &'static str, String);

struct Conversation {
    id: String,
    node: Arc<Node>,
    asterisk: Line,
    /// The connection of the instance that owns the conversation; `None`
    /// while no instance does.
    application: Option<ApplicationSocket>,
    /// When the application was last heard on its connection.
    heard_at: Instant,
    /// When the next ping is due on the application's connection.
    ping_at: Instant,
    /// How many pings have been sent: numbers the next one.
    pings: u64,
    /// How the conversation came to exist.
    origin: Origin,
    fallback: FallbackProgress,
    /// Why the fallback in progress is failing, as first learned: the reason
    /// its report gives when it is given up.
    fallback_failure: Option<FallbackFailure>,
    /// The connection to Asterisk is lost, and it has not been found again:
    /// nothing is known of the participants, and nothing is asked of it.
    server_lost: bool,
    /// When Asterisk is next asked how much audio each participant has sent.
    medium_read_at: Instant,
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
    /// How many taps have been made: names the next one.
    taps_made: u64,
    /// How many groups this controller has made: numbers the next one.
    groups_made: u64,
    /// The bridge of each group, by its number. Groups found at a restart
    /// keep the bridges the controller before made for them.
    group_bridges: HashMap<u64, String>,
    /// How many participants this controller has added: numbers the next.
    participants: u64,
    /// The one who came through the entry — the one the entry's fallback is
    /// for; `None` once they are gone, or when nobody did.
    caller: Option<ParticipantId>,
    /// What every name this controller gives in Asterisk for the
    /// conversation begins with: the conversation and the controller's run.
    names: String,
    /// Participants being called whom the application does not know yet,
    /// with what has happened to them meanwhile. The application learns of
    /// such a participant from the answer to its `dial` command, and that
    /// answer waits for Asterisk's; events about them wait with it.
    unannounced: HashMap<ParticipantId, Vec<Event>>,
    /// The identifiers of the commands the owner has sent on its connection.
    /// The description makes each unique on a connection: answers are told
    /// apart by them, and one used twice would make two answers alike.
    command_ids: HashSet<u64>,
    /// For a conversation the application asked for: Asterisk's word on the
    /// call to its first participant, once it has come.
    first_placed: Option<Result<Participant, CommandRejection>>,
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
    let outcome = match begin(&id, asterisk, arrival, node) {
        Ok((mut conversation, opening)) => {
            // From here on a participant is on the line. Whatever goes wrong
            // — the application breaks the protocol, Asterisk refuses a
            // step — the participant is not left there with no one in
            // control.
            let result = conversation.live(Some(opening)).await;
            if let Err(fault) = &result {
                conversation.abandon(fault).await;
            }
            result
        }
        Err(fault) => Err(fault),
    };
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

/// A conversation that begins with a call that arrived, and how it is
/// opened to the application.
fn begin(
    id: &str,
    asterisk: Line,
    arrival: Arrival,
    node: Arc<Node>,
) -> Result<(Conversation, Opening), Fault> {
    let Arrival { channel, origin } = arrival;
    let first = Member {
        id: participant_id(&node, 1)?,
        number: channel.caller.number.parse().ok(),
        name: Some(channel.name),
        watch: Watch::new(Duration::ZERO),
        leaving: None,
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
        group: None,
        put: Put::Nowhere,
    };
    let opening = Opening::Started {
        origin: origin.clone(),
        first: first.known(),
    };

    let mut conversation = Conversation::blank(id.to_owned(), asterisk, node, origin);
    conversation.caller = Some(first.id.clone());
    conversation.members.push(first);
    conversation.participants = 1;
    Ok((conversation, opening))
}

/// What a controller writes on a participant's channel about how the
/// conversation came to exist: for a call that arrived, its entry in the
/// words of the dialplan; for one the application asked for, its request
/// and handler.
fn origin_notes(origin: &Origin) -> Vec<(&'static str, String)> {
    match origin {
        Origin::DialedNumber { dialed } => {
            vec![(NOTE_ORIGIN, format!("dialed_number,{}", dialed.as_str()))]
        }
        Origin::StartedByCode { request, handler } => vec![
            (NOTE_ORIGIN, STARTED_BY_CODE.to_owned()),
            (NOTE_REQUEST, request.as_str().to_owned()),
            (NOTE_HANDLER, handler.as_str().to_owned()),
        ],
        Origin::UserEndpoint { .. } | Origin::WebPass { .. } => Vec::new(),
    }
}

/// The next frame of a conversation connection; `None` when it has closed,
/// and when there is none.
async fn next_frame(
    application: &mut Option<ApplicationSocket>,
) -> Option<Result<Frame, tokio_tungstenite::tungstenite::Error>> {
    match application {
        Some(socket) => socket.next().await,
        None => None,
    }
}

impl Conversation {
    /// A conversation with nobody in it yet.
    fn blank(id: String, asterisk: Line, node: Arc<Node>, origin: Origin) -> Self {
        let names = format!("{id}.{}", node.run);
        let (media_inbox, from_media) = mpsc::channel(MEDIA_BACKLOG);
        Self {
            id,
            node,
            asterisk,
            application: None,
            heard_at: Instant::now(),
            ping_at: Instant::now(),
            pings: 0,
            origin,
            fallback: FallbackProgress::NotBegun,
            fallback_failure: None,
            server_lost: false,
            medium_read_at: Instant::now(),
            members: Vec::new(),
            pending: HashMap::new(),
            next_request: 0,
            ended_by_handler: false,
            from_media,
            media_inbox,
            sounds_opened: 0,
            taps_made: 0,
            groups_made: 0,
            group_bridges: HashMap::new(),
            participants: 0,
            caller: None,
            names,
            unannounced: HashMap::new(),
            command_ids: HashSet::new(),
            first_placed: None,
        }
    }

    /// A conversation as a controller that started beside a running Asterisk
    /// found it there.
    fn resume(found: Found, asterisk: Line, node: Arc<Node>) -> Self {
        // The groups keep the bridges they have; this controller numbers
        // them its own way.
        let mut group_bridges: HashMap<u64, String> = HashMap::new();
        let mut number_of = |bridge: String| {
            let known = group_bridges
                .iter()
                .find_map(|(number, known)| (*known == bridge).then_some(*number));
            known.unwrap_or_else(|| {
                let number = u64::try_from(group_bridges.len()).unwrap_or(u64::MAX) + 1;
                group_bridges.insert(number, bridge);
                number
            })
        };
        // The caller is the one who came through the entry: the one the node
        // did not call, so the one with no line.
        let caller = found
            .members
            .iter()
            .find(|member| member.line.is_none())
            .map(|member| member.participant.clone());
        let members: Vec<Member> = found
            .members
            .into_iter()
            .map(|member| {
                let group = member.group.map(&mut number_of);
                Member {
                    id: member.participant,
                    number: member.number,
                    name: Some(member.name),
                    watch: Watch::new(member.hold_limit),
                    leaving: None,
                    channel: member.channel,
                    state: if member.answered {
                        ParticipantState::InConversation
                    } else {
                        ParticipantState::Ringing
                    },
                    removed_by_us: false,
                    sound: None,
                    awaiting_answer: member.awaiting_answer,
                    _place: member.line.map(|line| node.lines.resume(&line)),
                    group,
                    put: group.map_or(Put::Nowhere, Put::InGroup),
                }
            })
            .collect();
        let groups: Vec<u64> = group_bridges.keys().copied().collect();
        let mut conversation = Self::blank(found.id, asterisk, node, found.origin);
        conversation.members = members;
        conversation.groups_made = u64::try_from(group_bridges.len()).unwrap_or(u64::MAX);
        conversation.group_bridges = group_bridges;
        conversation.caller = caller;
        // A group that has lost all but one of its people is no group.
        for group in groups {
            conversation.settle_group(group);
        }
        // Whoever left between being found and being heard about is found
        // out now: from here on their leaving would be heard.
        let channels: Vec<(ParticipantId, String)> = conversation
            .members
            .iter()
            .map(|member| (member.id.clone(), member.channel.clone()))
            .collect();
        for (participant, channel) in channels {
            conversation.ask(
                Pending::Exists(participant),
                "GET",
                &format!("channels/{}", ari::query(&channel)),
            );
        }
        conversation
    }

    /// Write on the channel of a participant what only the controller
    /// knows of them, for a controller that comes after this one.
    fn note(&mut self, participant: &ParticipantId) {
        let Some(member) = self.member(participant) else {
            return;
        };
        let channel = member.channel.clone();
        let number = member.number.clone();
        let mut notes = vec![
            (NOTE_CONVERSATION, self.id.clone()),
            (NOTE_PARTICIPANT, participant.as_str().to_owned()),
        ];
        notes.extend(origin_notes(&self.origin));
        notes.extend(number.map(|number| (NOTE_NUMBER, number.as_str().to_owned())));
        for (name, value) in notes {
            self.write_note(participant, &channel, name, &value);
        }
    }

    /// Write one note on a participant's channel. A note Asterisk refuses
    /// for one who stays would be missed by a controller that comes after —
    /// which would remove their call as a leftover — so a refusal is judged
    /// like any step about them.
    fn write_note(&mut self, participant: &ParticipantId, channel: &str, name: &str, value: &str) {
        self.ask(
            Pending::Step(participant.clone(), Step::Note),
            "POST",
            &format!(
                "channels/{}/variable?variable={name}&value={}",
                ari::query(channel),
                ari::query(value),
            ),
        );
    }

    /// Carry the conversation to its end: with an instance of the
    /// application that owns it, and without one in between.
    ///
    /// A conversation that has just begun is offered to the application
    /// with `opening`; one found at a restart, with none, is offered as it
    /// is.
    async fn live(&mut self, opening: Option<Opening>) -> Result<(), Fault> {
        let (mut owned, mut look_first) = match opening {
            Some(opening) => {
                if let Some(first) = self.caller.clone() {
                    self.note(&first);
                }
                // What Asterisk said meanwhile of a participant the opening
                // made known comes after the opening.
                let owned = match self.find_owner(opening).await {
                    Ok(()) => self.announce_waiting().await,
                    not_taken => not_taken,
                };
                // A conversation nobody took has just had its one look.
                (owned, false)
            }
            None => (
                Err(Fault::Owner(
                    "the conversation was found at the controller's start".into(),
                )),
                true,
            ),
        };
        loop {
            let why = match owned {
                Ok(()) => match self.run().await {
                    // An instance that was lost may have a neighbour that
                    // is there: one look for it comes before anything else.
                    Err(Fault::Owner(why)) => {
                        look_first = true;
                        why
                    }
                    ended => return ended,
                },
                Err(Fault::Owner(why)) => why,
                Err(fault) => return Err(fault),
            };
            eprintln!("conversation {}: no application — {why}", self.id);
            self.release_owner();
            if !self.stand_in(look_first).await? {
                return Ok(());
            }
            // With an application the fallback has no part; should this one
            // be lost too, a caller alone is given it again.
            self.fallback = FallbackProgress::NotBegun;
            self.fallback_failure = None;
            owned = Ok(());
        }
    }

    /// Open a conversation connection and offer the conversation on it.
    ///
    /// # Errors
    ///
    /// [`Fault::Owner`] when no instance took it: none could be reached, or
    /// the one that was reached declined or went away.
    async fn find_owner(&mut self, opening: Opening) -> Result<(), Fault> {
        let socket = self
            .node
            .application
            .open(Endpoint::Conversation)
            .await
            .map_err(Fault::Owner)?;
        self.application = Some(socket);
        self.command_ids.clear();
        self.heard_at = Instant::now();
        self.ping_at = self.heard_at + PING_EVERY;
        let offered = async {
            let hello = self.hello(opening)?;
            self.tell(&hello).await?;
            self.await_acceptance().await
        }
        .await;
        if matches!(offered, Err(Fault::Owner(_))) {
            self.application = None;
        }
        if offered.is_ok() && self.server_lost {
            self.event(Event::TelephonyServerLost).await?;
        }
        offered
    }

    /// Tell a new owner what waited for it: what Asterisk said of a
    /// participant before the owner knew them.
    async fn announce_waiting(&mut self) -> Result<(), Fault> {
        let waiting: Vec<Event> = self
            .unannounced
            .drain()
            .flat_map(|(_, events)| events)
            .collect();
        for event in waiting {
            self.event(event).await?;
        }
        Ok(())
    }

    /// Place the call to the first participant of a conversation the
    /// application asked for, and wait for Asterisk's word on it: the
    /// participant, or why there is none. What Asterisk says of the call
    /// meanwhile waits for the owner with them.
    async fn place_first(
        &mut self,
        number: &PhoneNumber,
        line: &LineId,
        answer_limit_ms: u64,
    ) -> Result<Result<Participant, CommandRejection>, Fault> {
        if let Some(rejection) = self
            .dial(Asker::Request, number, line, answer_limit_ms)
            .await?
        {
            return Ok(Err(rejection));
        }
        loop {
            if let Some(placed) = self.first_placed.take() {
                return Ok(placed);
            }
            match self.asterisk.next().await {
                FromAsterisk::Gone => {
                    return Err(Fault::Asterisk(
                        "the conversation's line closed before the call was placed".into(),
                    ));
                }
                said @ (FromAsterisk::Said(_)
                | FromAsterisk::Answered { .. }
                | FromAsterisk::Lost
                | FromAsterisk::Back(_)) => self.asterisk_said(said).await?,
            }
        }
    }

    /// The first message of a conversation connection.
    fn hello(&self, opening: Opening) -> Result<ControllerMessage, Fault> {
        let config = &self.node.config;
        let invalid =
            |what: &str| Fault::Asterisk(format!("the controller's own {what} is not valid"));
        Ok(ControllerMessage::Hello {
            protocol: NonZeroU64::new(PROTOCOL_VERSION)
                .ok_or_else(|| invalid("protocol version"))?,
            node: config.node.parse().map_err(|_| invalid("node name"))?,
            controller_version: env!("CARGO_PKG_VERSION")
                .parse()
                .map_err(|_| invalid("version"))?,
            conversation: self
                .id
                .parse()
                .map_err(|_| invalid("conversation identifier"))?,
            opening,
        })
    }

    /// The conversation as it is now, for an instance that takes it over:
    /// who is in it, in what state, and who is connected to whom. Nothing
    /// an earlier owner had asked for is part of it.
    fn resumed(&self) -> Opening {
        let participants = self
            .members
            .iter()
            .map(|member| ParticipantSnapshot {
                participant: member.known(),
                state: match member.state {
                    ParticipantState::Ringing => StateTold::Ringing,
                    ParticipantState::InConversation if member.watch.held_since.is_some() => {
                        StateTold::Held
                    }
                    ParticipantState::InConversation => StateTold::InConversation,
                },
                hold_limit_ms: u64::try_from(member.watch.hold_limit.as_millis())
                    .unwrap_or(u64::MAX),
            })
            .collect();
        let mut groups: Vec<u64> = self
            .members
            .iter()
            .filter_map(|member| member.group)
            .collect();
        groups.sort_unstable();
        groups.dedup();
        let connected = groups
            .into_iter()
            .map(|group| {
                self.members
                    .iter()
                    .filter(|member| member.group == Some(group))
                    .map(|member| member.id.clone())
                    .collect()
            })
            .collect();
        Opening::Resumed {
            origin: self.origin.clone(),
            participants,
            connected,
            recordings: Vec::new(),
        }
    }

    /// Wait for the application to take the conversation.
    async fn await_acceptance(&mut self) -> Result<(), Fault> {
        loop {
            let due = self.liveness_due();
            let frame = tokio::select! {
                frame = next_frame(&mut self.application) => frame,
                () = tokio::time::sleep_until(due) => {
                    self.keep_alive().await?;
                    continue;
                }
            };
            self.heard_at = Instant::now();
            let said = match frame {
                Some(Ok(Frame::Text(text))) => text,
                Some(Ok(Frame::Close(_))) | None => {
                    return Err(Fault::Owner(
                        "closed the connection before accepting".into(),
                    ));
                }
                Some(Ok(Frame::Binary(_) | Frame::Ping(_) | Frame::Pong(_) | Frame::Frame(_))) => {
                    continue;
                }
                Some(Err(error)) => return Err(Fault::Owner(error.to_string())),
            };
            match node_protocol::decode::<ApplicationMessage>(said.as_str())
                .map_err(Fault::Protocol)?
            {
                ApplicationMessage::Accept => return Ok(()),
                ApplicationMessage::Decline { reason } => {
                    return Err(Fault::Owner(format!("declined the conversation: {reason}")));
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
    ///
    /// # Errors
    ///
    /// [`Fault::Owner`] when the application's connection is lost: the
    /// conversation is not over then, only without an owner.
    async fn run(&mut self) -> Result<(), Fault> {
        // The conversation lasts while it has a participant — or is still
        // owed Asterisk's word about one it tried to call: the application
        // has a `dial` command waiting for its answer.
        while !(self.members.is_empty() && self.unannounced.is_empty()) {
            let due = self.liveness_due();
            tokio::select! {
                said = self.asterisk.next() => self.asterisk_said(said).await?,
                said = next_frame(&mut self.application) => match said {
                    Some(Ok(Frame::Text(text))) => {
                        self.heard_at = Instant::now();
                        let message = node_protocol::decode(text.as_str()).map_err(Fault::Protocol)?;
                        self.on_application(message).await?;
                    }
                    Some(Ok(Frame::Binary(bytes))) => {
                        self.heard_at = Instant::now();
                        self.on_audio(&bytes).await?;
                    }
                    Some(Ok(Frame::Ping(_) | Frame::Pong(_) | Frame::Frame(_))) => {
                        self.heard_at = Instant::now();
                    }
                    Some(Ok(Frame::Close(_))) | None => {
                        return Err(Fault::Owner("lost the conversation connection".into()));
                    }
                    Some(Err(error)) => return Err(Fault::Owner(error.to_string())),
                },
                Some(said) = self.from_media.recv() => self.on_media(said).await?,
                () = tokio::time::sleep_until(due), if self.application.is_some() => {
                    self.keep_alive().await?;
                }
                () = tokio::time::sleep_until(self.medium_read_at), if self.watching_media() => {
                    self.read_media();
                }
            }
        }

        let reason = if self.ended_by_handler {
            EndReason::EndedByHandler
        } else {
            EndReason::LastParticipantLeft
        };
        self.event(Event::ConversationEnded { reason }).await?;
        if let Some(application) = &mut self.application {
            let _ = application.close(None).await;
        }
        Ok(())
    }

    /// When the liveness of the application's connection is next to be
    /// looked after: a ping is due, or the silence reaches its limit.
    fn liveness_due(&self) -> Instant {
        self.ping_at.min(self.heard_at + SILENCE_LIMIT)
    }

    /// Look after the liveness of the application's connection, as the
    /// protocol has it: a ping at least every [`PING_EVERY`], and an owner
    /// silent for [`SILENCE_LIMIT`] is an owner lost — also one that took
    /// the connection and never answered.
    ///
    /// # Errors
    ///
    /// [`Fault::Owner`] when the silence has reached its limit.
    async fn keep_alive(&mut self) -> Result<(), Fault> {
        let now = Instant::now();
        if now >= self.heard_at + SILENCE_LIMIT {
            return Err(Fault::Owner(format!(
                "heard nothing from the application for {} seconds",
                SILENCE_LIMIT.as_secs()
            )));
        }
        if now >= self.ping_at {
            self.pings += 1;
            self.ping_at = now + PING_EVERY;
            let n = self.pings;
            self.tell(&ControllerMessage::Ping { n }).await?;
        }
        Ok(())
    }

    /// Asterisk said something to this conversation.
    async fn asterisk_said(&mut self, said: FromAsterisk) -> Result<(), Fault> {
        match said {
            FromAsterisk::Said(message) => self.on_asterisk(message).await,
            FromAsterisk::Answered {
                request,
                status,
                body,
            } => self.answered(&request, status, &body).await,
            // Whoever is still listed left with Asterisk.
            FromAsterisk::Gone => self.asterisk_gone().await,
            FromAsterisk::Lost => self.telephony_server_lost().await,
            FromAsterisk::Back(found) => self.telephony_server_back(found).await,
        }
    }

    /// Asterisk is lost. Until it is found again nothing is known of the
    /// participants — their calls may go on or may have ended — and nothing
    /// is asked of it. The audio paths are lost with it: what was queued is
    /// dropped, and listening ends; the owner is told, and asks again when
    /// it is back.
    async fn telephony_server_lost(&mut self) -> Result<(), Fault> {
        if self.server_lost {
            return Ok(());
        }
        self.server_lost = true;
        let mut dropped = Vec::new();
        for member in &mut self.members {
            if let Some(mut sound) = member.sound.take() {
                // The bridge they were in goes with the path.
                if member.put == Put::BesideMedia {
                    member.put = Put::Nowhere;
                }
                let mut outcomes = Vec::new();
                sound.playout.abandon(&mut outcomes);
                dropped.push((member.id.clone(), sound, outcomes));
            }
        }
        let owned = self.application.is_some();
        for (participant, sound, outcomes) in dropped {
            self.close_sound(&sound);
            if owned {
                self.report(&participant, outcomes).await?;
            }
        }
        if owned {
            self.event(Event::TelephonyServerLost).await?;
        }
        Ok(())
    }

    /// Asterisk is found again. Whoever is not in what was found of the
    /// conversation ended their call meanwhile, how is not known; the
    /// others are where they were.
    async fn telephony_server_back(&mut self, found: Option<Found>) -> Result<(), Fault> {
        if !self.server_lost {
            return Ok(());
        }
        self.server_lost = false;
        let still: HashSet<String> = found
            .map(|found| {
                found
                    .members
                    .into_iter()
                    .map(|member| member.channel)
                    .collect()
            })
            .unwrap_or_default();
        let gone: Vec<String> = self
            .members
            .iter()
            .filter(|member| !still.contains(&member.channel))
            .map(|member| member.channel.clone())
            .collect();
        for channel in gone {
            self.left_as(&channel, None, Some(Departure::Lost)).await?;
        }
        if self.application.is_some() {
            self.event(Event::TelephonyServerBack).await?;
        }
        Ok(())
    }

    /// The application is gone, and with it everything that lived only
    /// while it listened: the audio paths it had opened — what it was
    /// hearing and what it had queued to play — and the answers it was
    /// waiting for.
    fn release_owner(&mut self) {
        self.application = None;
        self.unannounced.clear();
        for pending in self.pending.values_mut() {
            if let Pending::Dial(asker @ Asker::Command(_), _) = pending {
                *asker = Asker::Nobody;
            }
        }
        let mut sounds = Vec::new();
        let mut participants = Vec::new();
        for member in &mut self.members {
            if let Some(sound) = member.sound.take() {
                sounds.push(sound);
                // The bridge they were in goes with the path.
                if member.put == Put::BesideMedia {
                    member.put = Put::Nowhere;
                }
            }
            participants.push(member.id.clone());
        }
        for sound in &sounds {
            self.close_sound(sound);
        }
        // One who was on their way into a group, waiting for a tap to take
        // their place, has no place to be held any more.
        for participant in &participants {
            self.arrange(participant);
        }
    }

    /// Keep the conversation while no instance of the application owns it,
    /// until one takes it or nobody is left on the line.
    ///
    /// People who are connected to each other are held together, and an
    /// instance is looked for as long as they are. A caller who was alone
    /// with the application is given the fallback of their entry. Whoever
    /// is left with nobody to talk to and nobody in charge is hung up.
    ///
    /// Returns whether an instance took the conversation.
    async fn stand_in(&mut self, mut look: bool) -> Result<bool, Fault> {
        let mut look_at = Instant::now() + LOOK_FOR_OWNER;
        let mut said_why = String::new();
        let mut holding_reported = false;
        loop {
            // A caller sent to the message has left the conversation, but
            // the fallback is reported by Asterisk's answer, which may come
            // after they have gone.
            if self.members.is_empty() && !self.message_answer_awaited() {
                return Ok(false);
            }
            if look {
                match self.find_owner(self.resumed()).await {
                    Ok(()) => return Ok(true),
                    // Said once, not every second, while it stays the same.
                    Err(Fault::Owner(why)) if why == said_why => {}
                    Err(Fault::Owner(why)) => {
                        eprintln!("conversation {}: still no application — {why}", self.id);
                        said_why = why;
                    }
                    Err(fault) => return Err(fault),
                }
                look = false;
                look_at = Instant::now() + LOOK_FOR_OWNER;
            }
            // Without Asterisk there is nothing to do for anyone, and nothing
            // known of them.
            if !self.server_lost {
                self.keep_without_owner().await?;
            }
            if !holding_reported && self.someone_is_connected() {
                holding_reported = true;
                self.keep_report(|conversation, at_unix_ms| Report::HoldingWithoutOwner {
                    conversation,
                    at_unix_ms,
                });
            }
            tokio::select! {
                said = self.asterisk.next() => self.asterisk_said(said).await?,
                Some(said) = self.from_media.recv() => self.on_media(said).await?,
                () = tokio::time::sleep_until(look_at), if self.someone_is_connected() => look = true,
                () = tokio::time::sleep_until(self.medium_read_at), if self.watching_media() => {
                    self.read_media();
                }
            }
        }
    }

    /// Whether anyone's audio is watched now.
    fn watching_media(&self) -> bool {
        !self.server_lost && self.members.iter().any(Member::medium_watched)
    }

    /// Ask Asterisk how much audio each watched participant has sent, and
    /// let go of the held whose limit has passed.
    fn read_media(&mut self) {
        let now = Instant::now();
        self.medium_read_at = now + MEDIUM_READ_EVERY;
        let mut asks = Vec::new();
        let mut gone = Vec::new();
        for member in self
            .members
            .iter_mut()
            .filter(|member| member.medium_watched())
        {
            if let Some(since) = member.watch.held_since
                && now >= since + member.watch.hold_limit
            {
                gone.push(member.id.clone());
                continue;
            }
            if !member.watch.asking
                && let Some(name) = &member.name
            {
                member.watch.asking = true;
                asks.push((member.id.clone(), name.clone()));
            }
        }
        for (participant, name) in asks {
            self.ask(
                Pending::Medium(participant),
                "GET",
                &format!("channels/{}/rtp_statistics", ari::query(&name)),
            );
        }
        for participant in gone {
            self.did_not_return(&participant);
        }
    }

    /// Asterisk said how much audio a participant has sent. A count that
    /// grew is audio arriving: one who was held is back. One that stood
    /// still for [`MEDIUM_LOST_AFTER`] — while their far end has not put the
    /// call on hold — is audio lost: they are held, for as long as their
    /// limit allows; with none, they are gone at once.
    async fn medium_read(
        &mut self,
        participant: &ParticipantId,
        status_code: u16,
        body: &str,
    ) -> Result<(), Fault> {
        let conversation = self.id.clone();
        let Some(member) = self.member(participant) else {
            return Ok(());
        };
        member.watch.asking = false;
        let received = match status_code {
            200 => ari::read_received_packets(body).map_err(|error| {
                Fault::Asterisk(format!(
                    "described the audio of a channel unreadably: {error}"
                ))
            })?,
            // Asterisk keeps no count for this channel: it is not watched.
            403 => {
                eprintln!(
                    "conversation {conversation}: Asterisk keeps no audio count for {} ({body}); \
                     whether their audio is lost is not watched",
                    participant.as_str()
                );
                member.watch.unwatchable = true;
                return Ok(());
            }
            // Gone, or Asterisk could not be asked: the word of either
            // comes on its own.
            _ => return Ok(()),
        };
        let now = Instant::now();
        let grew = member.watch.received.is_none_or(|before| received > before);
        member.watch.received = Some(received);
        if grew {
            member.watch.grew_at = now;
            if member.watch.held_since.take().is_some() {
                let participant = participant.clone();
                return self
                    .about(
                        &participant.clone(),
                        Event::ParticipantReturned { participant },
                    )
                    .await;
            }
            return Ok(());
        }
        let lost = member.watch.held_since.is_none()
            && !member.watch.far_end_on_hold
            && now >= member.watch.grew_at + MEDIUM_LOST_AFTER;
        if !lost {
            return Ok(());
        }
        member.watch.held_since = Some(now);
        let no_limit = member.watch.hold_limit.is_zero();
        let who = participant.clone();
        self.about(
            participant,
            Event::MediumLost {
                participant: who.clone(),
            },
        )
        .await?;
        self.about(participant, Event::ParticipantHeld { participant: who })
            .await?;
        if no_limit {
            self.did_not_return(participant);
        }
        Ok(())
    }

    /// The far end of a channel put the call on hold, or took it off: while
    /// on hold no audio is expected, and when off, waiting begins anew.
    fn far_end_hold(&mut self, channel: &str, on_hold: bool) {
        if let Some(member) = self
            .members
            .iter_mut()
            .find(|member| member.channel == channel)
        {
            member.watch.far_end_on_hold = on_hold;
            member.watch.grew_at = Instant::now();
        }
    }

    /// A held participant whose audio did not come back within their limit
    /// is gone: the node drops their channel, and they leave as one who did
    /// not return. With no instance to say it to, it is reported.
    fn did_not_return(&mut self, participant: &ParticipantId) {
        let Some(member) = self.member(participant) else {
            return;
        };
        if member.removed_by_us {
            return;
        }
        member.removed_by_us = true;
        member.leaving = Some(Departure::DidNotReturn);
        let channel = member.channel.clone();
        eprintln!(
            "conversation {}: {} did not return; their call is ended",
            self.id,
            participant.as_str()
        );
        self.ask(
            Pending::Step(participant.clone(), Step::HangUp),
            "DELETE",
            &format!("channels/{channel}"),
        );
        if self.application.is_none() {
            self.report_dropped(participant.clone(), false);
        }
    }

    fn message_answer_awaited(&self) -> bool {
        self.pending
            .values()
            .any(|pending| matches!(pending, Pending::Message(..)))
    }

    fn someone_is_connected(&self) -> bool {
        self.members.iter().any(|member| member.group.is_some())
    }

    /// Do for the participants what their state calls for while there is
    /// no application. Called after every change; asks for nothing twice.
    async fn keep_without_owner(&mut self) -> Result<(), Fault> {
        if self.someone_is_connected() {
            // People connected to each other are kept. One who is connected
            // to nobody — also one still being called — has nobody to talk
            // to and nobody in charge.
            self.hang_up(|member| member.group.is_none());
            return Ok(());
        }
        let Some(caller) = self.caller.clone() else {
            // The one the fallback is for is gone: nobody is kept.
            self.give_up(FallbackFailure::CallerLeft);
            return Ok(());
        };
        let state_of = |members: &[Member], who: &ParticipantId| {
            members
                .iter()
                .find(|member| member.id == *who && !member.removed_by_us)
                .map(|member| member.state)
        };
        match &self.fallback {
            FallbackProgress::NotBegun => self.begin_fallback(&caller).await,
            FallbackProgress::Calling(dialled) => {
                let dialled = dialled.clone();
                match (
                    state_of(&self.members, &caller),
                    state_of(&self.members, &dialled),
                ) {
                    // The one who was dialled answered: now the caller is
                    // answered too, as a transfer answers them.
                    (Some(ParticipantState::Ringing), Some(ParticipantState::InConversation)) => {
                        if let Some(member) = self.member(&caller) {
                            let channel = member.channel.clone();
                            self.ask(
                                Pending::Internal,
                                "POST",
                                &format!("channels/{channel}/answer"),
                            );
                        }
                        self.fallback = FallbackProgress::Answering(dialled);
                    }
                    (
                        Some(ParticipantState::InConversation),
                        Some(ParticipantState::InConversation),
                    ) => self.transferred(&caller, &dialled),
                    (Some(_), Some(ParticipantState::Ringing)) => {}
                    // One of the two is gone.
                    (None, _) => self.give_up(FallbackFailure::CallerLeft),
                    (_, None) => self.give_up(FallbackFailure::NotConnected {
                        departure: Departure::HungUp,
                    }),
                }
                Ok(())
            }
            FallbackProgress::Answering(dialled) => {
                let dialled = dialled.clone();
                match (
                    state_of(&self.members, &caller),
                    state_of(&self.members, &dialled),
                ) {
                    (Some(ParticipantState::InConversation), Some(_)) => {
                        self.transferred(&caller, &dialled);
                    }
                    (Some(ParticipantState::Ringing), Some(_)) => {}
                    (None, _) => self.give_up(FallbackFailure::CallerLeft),
                    (_, None) => self.give_up(FallbackFailure::NotConnected {
                        departure: Departure::HungUp,
                    }),
                }
                Ok(())
            }
            FallbackProgress::Done => {
                self.hang_up(|_| true);
                Ok(())
            }
        }
    }

    /// Begin the fallback of the conversation's entry for the caller who
    /// came through it. Anyone else the application had called is hung up:
    /// the fallback is the caller's.
    async fn begin_fallback(&mut self, caller: &ParticipantId) -> Result<(), Fault> {
        self.fallback = FallbackProgress::Done;
        let is_here = self
            .members
            .iter()
            .any(|member| member.id == *caller && !member.removed_by_us);
        let applied = self.node.applied();
        let fallback =
            applied
                .settings
                .entries
                .iter()
                .find_map(|entry| match (&entry.key, &self.origin) {
                    (EntryKey::DialedNumber(number), Origin::DialedNumber { dialed })
                        if number == dialed =>
                    {
                        Some(&entry.fallback)
                    }
                    _ => None,
                });
        match fallback {
            Some(Fallback::Transfer {
                number,
                line,
                answer_limit_ms,
            }) if is_here => {
                self.hang_up(|member| member.id != *caller);
                match self
                    .dial(Asker::Nobody, number, line, *answer_limit_ms)
                    .await?
                {
                    None => {
                        eprintln!(
                            "conversation {}: the caller is being transferred to {}, the fallback of their entry",
                            self.id,
                            number.as_str()
                        );
                        self.fallback = FallbackProgress::Calling(participant_id(
                            &self.node,
                            self.participants,
                        )?);
                    }
                    Some(rejection) => {
                        eprintln!(
                            "conversation {}: the fallback of the entry — a transfer to {} — cannot be carried out; the caller is hung up",
                            self.id,
                            number.as_str()
                        );
                        self.fallback_failed(
                            FallbackKind::Transfer,
                            FallbackFailure::Refused { rejection },
                        );
                        self.hang_up(|_| true);
                    }
                }
            }
            Some(Fallback::Message { prompt }) if is_here => {
                let here = applied
                    .settings
                    .prompts
                    .iter()
                    .find(|named| named.id == *prompt)
                    .is_some_and(|named| prompts::is_here(&self.node.config.state, named));
                if here {
                    self.play_message(caller);
                } else {
                    // The dialplan would turn them away as well; the caller
                    // is hung up here, and reported so.
                    eprintln!(
                        "conversation {}: the prompt of the entry's message {:?} is not here; \
                         the caller is hung up",
                        self.id,
                        prompt.as_str()
                    );
                    self.fallback_failed(FallbackKind::Message, FallbackFailure::PromptMissing);
                    self.hang_up(|_| true);
                }
            }
            Some(Fallback::Transfer { .. } | Fallback::Message { .. }) | None => {
                eprintln!(
                    "conversation {}: nobody is connected and there is no application; everyone is hung up",
                    self.id
                );
                self.hang_up(|_| true);
            }
        }
        Ok(())
    }

    /// Give the caller the message of their entry's fallback: the controller
    /// sends them to the step of the dialplan where the fallback begins —
    /// the same steps Asterisk carries out alone when the controller is
    /// away — and the message is played and the call ended there. Anyone
    /// else the application had called is hung up: the fallback is the
    /// caller's.
    fn play_message(&mut self, caller: &ParticipantId) {
        let Origin::DialedNumber { dialed } = &self.origin else {
            self.hang_up(|_| true);
            return;
        };
        let dialed = dialed.as_str().to_owned();
        self.hang_up(|member| member.id != *caller);
        let Some(member) = self.member(caller) else {
            return;
        };
        // From here on the call is the dialplan's, not the conversation's.
        member.removed_by_us = true;
        let channel = member.channel.clone();
        self.ask(
            Pending::Message(channel.clone(), caller.clone()),
            "POST",
            &format!(
                "channels/{}/continue?context={}&extension={}&label={}",
                ari::query(&channel),
                ari::query(NETWORK_CONTEXT),
                ari::query(&dialed),
                ari::query(FALLBACK_LABEL),
            ),
        );
        eprintln!(
            "conversation {}: the caller is given the message of their entry's fallback",
            self.id
        );
    }

    /// The fallback's transfer is done: the caller and the one who was
    /// dialled are connected. From now on they are people connected to each
    /// other, and an application is looked for while they are.
    fn transferred(&mut self, caller: &ParticipantId, dialled: &ParticipantId) {
        self.fallback = FallbackProgress::Done;
        self.gather(&[caller.clone(), dialled.clone()]);
        let origin = self.origin.clone();
        self.keep_report(|conversation, at_unix_ms| Report::FallbackApplied {
            conversation,
            origin,
            fallback: FallbackKind::Transfer,
            at_unix_ms,
        });
    }

    /// The fallback could not be carried through: nobody is kept. A transfer
    /// that was under way is reported failed — for the reason first learned,
    /// or, when none was, for `otherwise`.
    fn give_up(&mut self, otherwise: FallbackFailure) {
        if matches!(
            self.fallback,
            FallbackProgress::Calling(_) | FallbackProgress::Answering(_)
        ) {
            let failure = self.fallback_failure.take().unwrap_or(otherwise);
            self.fallback_failed(FallbackKind::Transfer, failure);
        }
        self.fallback = FallbackProgress::Done;
        self.hang_up(|_| true);
    }

    /// Report that the fallback of the caller's entry could not be carried
    /// out, and why.
    fn fallback_failed(&self, fallback: FallbackKind, failure: FallbackFailure) {
        let origin = self.origin.clone();
        self.keep_report(|conversation, at_unix_ms| Report::FallbackFailed {
            conversation,
            origin,
            fallback,
            failure,
            at_unix_ms,
        });
    }

    /// Have Asterisk drop the channels of the participants `whom` picks.
    /// One who is already being dropped is not asked about again.
    ///
    /// The node does this only on its own, with no application to decide:
    /// each one is reported — as a call that was cancelled for one still
    /// being called, as a hang-up for anyone else.
    fn hang_up(&mut self, whom: impl Fn(&Member) -> bool) {
        let gone: Vec<(String, ParticipantId, bool)> = self
            .members
            .iter_mut()
            .filter(|member| !member.removed_by_us && whom(member))
            .map(|member| {
                member.removed_by_us = true;
                (
                    member.channel.clone(),
                    member.id.clone(),
                    member.awaiting_answer,
                )
            })
            .collect();
        for (channel, participant, being_called) in gone {
            self.ask(
                Pending::Step(participant.clone(), Step::HangUp),
                "DELETE",
                &format!("channels/{channel}"),
            );
            self.report_dropped(participant, being_called);
        }
    }

    /// Report a participant the node dropped on its own: as a call that was
    /// cancelled for one still being called, as a hang-up for anyone else.
    fn report_dropped(&self, participant: ParticipantId, being_called: bool) {
        self.keep_report(|conversation, at_unix_ms| {
            if being_called {
                Report::RingingCancelled {
                    conversation,
                    participant,
                    at_unix_ms,
                }
            } else {
                Report::ParticipantHungUp {
                    conversation,
                    participant,
                    at_unix_ms,
                }
            }
        });
    }

    /// Keep a report of what the node did on its own in this conversation,
    /// until the application has it.
    fn keep_report(&self, report: impl FnOnce(ConversationId, u64) -> Report) {
        match self.id.parse() {
            Ok(conversation) => self
                .node
                .reports
                .keep(report(conversation, reports::now_unix_ms())),
            Err(_) => eprintln!(
                "conversation {}: a REPORT IS LOST — the conversation's identifier is not valid",
                self.id
            ),
        }
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
            ari::Message::StasisStart { channel, .. } => {
                // A channel the node called is named by Asterisk only now.
                if let Some(member) = self
                    .members
                    .iter_mut()
                    .find(|member| member.channel == channel.id)
                {
                    member.name.get_or_insert(channel.name.clone());
                }
                self.entered(&channel.id).await?;
            }
            ari::Message::ChannelHold { channel } => self.far_end_hold(&channel.id, true),
            ari::Message::ChannelUnhold { channel } => self.far_end_hold(&channel.id, false),
            // Not about a channel: never routed to a conversation.
            ari::Message::ApplicationReplaced | ari::Message::Other => {}
        }
        Ok(())
    }

    /// Asterisk answered a request of this conversation.
    async fn answered(
        &mut self,
        request_id: &str,
        status_code: u16,
        body: &str,
    ) -> Result<(), Fault> {
        let pending = self.pending.remove(request_id);
        // Asterisk could not be asked at all: that is no word of its own,
        // and is not judged as a refusal. The connection to it is being
        // lost; what is still there is learned when it is found again. Only
        // a call that was to be placed is known now: it was not.
        if status_code == 0 {
            return match pending {
                Some(Pending::Dial(asker, participant)) => {
                    self.call_placed(asker, participant, status_code).await
                }
                Some(Pending::Medium(participant)) => {
                    self.medium_read(&participant, status_code, body).await
                }
                Some(_) => {
                    eprintln!(
                        "conversation {}: Asterisk could not be asked ({body}); what it has is \
                         learned when it is found again",
                        self.id
                    );
                    Ok(())
                }
                None => Ok(()),
            };
        }
        match pending {
            Some(Pending::Dial(asker, participant)) => {
                self.call_placed(asker, participant, status_code).await
            }
            Some(Pending::Medium(participant)) => {
                self.medium_read(&participant, status_code, body).await
            }
            Some(Pending::Step(participant, step)) => {
                self.step_answered(&participant, step, status_code).await
            }
            Some(Pending::Verdict(participant, step, refused_with)) => {
                self.verdict(&participant, step, refused_with, status_code, body)
            }
            Some(Pending::Group) if !(200..=299).contains(&status_code) => Err(Fault::Asterisk(
                format!("answered with status {status_code} when asked for the bridge of a group"),
            )),
            // A participant found at the restart left before the controller
            // was there to hear it: this is the one word of them.
            Some(Pending::Exists(participant)) => match status_code {
                200 => Ok(()),
                404 => {
                    let channel = self
                        .member(&participant)
                        .map(|member| member.channel.clone());
                    match channel {
                        Some(channel) => self.left(&channel, None).await,
                        None => Ok(()),
                    }
                }
                other => Err(Fault::Asterisk(format!(
                    "answered with status {other} when asked whether {} is still there",
                    participant.as_str()
                ))),
            },
            // A caller Asterisk would not send to the message is not left on
            // a line nobody controls.
            Some(Pending::Message(channel, caller)) if !(200..=299).contains(&status_code) => {
                eprintln!(
                    "conversation {}: Asterisk answered {status_code} when asked to play the \
                     message of the fallback ({body}); the caller is hung up",
                    self.id
                );
                self.ask(Pending::Internal, "DELETE", &format!("channels/{channel}"));
                self.fallback_failed(FallbackKind::Message, FallbackFailure::NotPlayed);
                self.keep_report(|conversation, at_unix_ms| Report::ParticipantHungUp {
                    conversation,
                    participant: caller,
                    at_unix_ms,
                });
                Ok(())
            }
            // The caller is in the dialplan's steps of the fallback: only now
            // is it carried out.
            Some(Pending::Message(..)) => {
                let origin = self.origin.clone();
                self.keep_report(|conversation, at_unix_ms| Report::FallbackApplied {
                    conversation,
                    origin,
                    fallback: FallbackKind::Message,
                    at_unix_ms,
                });
                Ok(())
            }
            Some(Pending::Group | Pending::Internal) | None => Ok(()),
        }
    }

    /// A channel of this conversation entered the node's application: a
    /// participant who was being called has answered, or the media channel
    /// of a participant, or a tap on them, is ready to be put into a bridge.
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
        self.path_channel_entered(channel);
        Ok(())
    }

    /// A channel is gone: its participant left. `cause` is the telephone
    /// network's reason, when the channel ended without ever having been in
    /// the application.
    async fn left(&mut self, channel: &str, cause: Option<i64>) -> Result<(), Fault> {
        self.left_as(channel, cause, None).await
    }

    /// The same, for a participant whose departure is known otherwise than
    /// from their channel's end: `known` is it.
    async fn left_as(
        &mut self,
        channel: &str,
        cause: Option<i64>,
        known: Option<Departure>,
    ) -> Result<(), Fault> {
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
        if let Some(group) = member.group {
            self.settle_group(group);
        }
        let departure = known.or_else(|| member.leaving.take()).unwrap_or_else(|| {
            if member.removed_by_us {
                Departure::Removed
            } else if member.awaiting_answer {
                unanswered(cause)
            } else {
                Departure::HungUp
            }
        });
        // A transfer under way fails when either of its two leaves before
        // they are connected; the first to leave says why.
        if let FallbackProgress::Calling(dialled) | FallbackProgress::Answering(dialled) =
            &self.fallback
        {
            if *dialled == member.id {
                self.fallback_failure
                    .get_or_insert(FallbackFailure::NotConnected {
                        departure: departure.clone(),
                    });
            } else if self.caller.as_ref() == Some(&member.id) {
                self.fallback_failure
                    .get_or_insert(FallbackFailure::CallerLeft);
            }
        }
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
            ApplicationMessage::Command { id, command } => {
                if !self.command_ids.insert(id) {
                    return Err(Fault::Application(format!(
                        "used the command identifier {id} a second time on this connection"
                    )));
                }
                self.command(id, command).await
            }
            ApplicationMessage::Ping { n } => self.tell(&ControllerMessage::Pong { n }).await,
            ApplicationMessage::Pong { .. } => Ok(()),
            ApplicationMessage::Accept | ApplicationMessage::Decline { .. } => Err(
                Fault::Application("accepted or declined a conversation it already owns".into()),
            ),
        }
    }

    async fn command(&mut self, id: u64, command: Command) -> Result<(), Fault> {
        if self.server_lost {
            return self
                .reject(id, CommandRejection::TelephonyServerUnavailable)
                .await;
        }
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
                self.ask(
                    Pending::Step(participant, Step::HangUp),
                    "DELETE",
                    &format!("channels/{channel}"),
                );
                self.accept(id).await
            }
            Command::Dial {
                number,
                line,
                answer_limit_ms,
            } => self
                .dial(Asker::Command(id), &number, &line, answer_limit_ms)
                .await
                .map(|_| ()),
            Command::SendDigits {
                participant,
                digits,
            } => self.send_digits(id, &participant, digits.as_str()).await,
            Command::End => {
                self.ended_by_handler = true;
                let channels: Vec<(ParticipantId, String)> = self
                    .members
                    .iter_mut()
                    .map(|member| {
                        member.removed_by_us = true;
                        (member.id.clone(), member.channel.clone())
                    })
                    .collect();
                for (participant, channel) in channels {
                    self.ask(
                        Pending::Step(participant, Step::HangUp),
                        "DELETE",
                        &format!("channels/{channel}"),
                    );
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
            Command::Connect { participants } => self.connect(id, &participants).await,
            Command::Separate { participant } => self.separate(id, &participant).await,
            Command::HoldFor {
                participant,
                limit_ms,
            } => self.hold_for(id, &participant, limit_ms).await,
            Command::StartRecording { .. } => Err(Fault::NotImplemented("start_recording")),
            Command::StopRecording { .. } => Err(Fault::NotImplemented("stop_recording")),
        }
    }

    /// How long a participant whose audio is lost stays held before they
    /// count as gone. One already held whose new limit has passed is gone
    /// now.
    ///
    /// The limit is the participant's, not the owner's: it stays when the
    /// owner changes, and is written on their channel, so a controller
    /// started again holds them as long.
    async fn hold_for(
        &mut self,
        id: u64,
        participant: &ParticipantId,
        limit_ms: u64,
    ) -> Result<(), Fault> {
        let answered = |member: &Member| member.state == ParticipantState::InConversation;
        let (expired, channel) = match self.member_who(participant, answered) {
            Ok(member) => {
                member.watch.hold_limit = Duration::from_millis(limit_ms);
                let expired = member
                    .watch
                    .held_since
                    .is_some_and(|since| Instant::now() >= since + member.watch.hold_limit);
                (expired, member.channel.clone())
            }
            Err(reason) => return self.reject(id, reason).await,
        };
        self.write_note(
            participant,
            &channel,
            NOTE_HOLD_LIMIT,
            &limit_ms.to_string(),
        );
        self.accept(id).await?;
        if expired {
            self.did_not_return(participant);
        }
        Ok(())
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
            Pending::Step(participant.clone(), Step::Answer),
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
            Pending::Step(participant.clone(), Step::TurnAway),
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
            Pending::Step(participant.clone(), Step::SendDigits),
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

    /// The same with a body.
    fn ask_with(&mut self, pending: Pending, method: &str, uri: &str, body: String) {
        self.next_request += 1;
        let request = self.next_request.to_string();
        self.pending.insert(request.clone(), pending);
        self.asterisk.ask_with(&request, method, uri, Some(body));
    }

    async fn event(&mut self, event: Event) -> Result<(), Fault> {
        self.tell(&ControllerMessage::Event { event }).await
    }

    /// Tell the application something. While no instance owns the
    /// conversation there is nobody to tell, and nothing is kept for later:
    /// an instance that takes the conversation is told what it is like then.
    async fn tell(&mut self, message: &ControllerMessage) -> Result<(), Fault> {
        let Some(application) = &mut self.application else {
            return Ok(());
        };
        let text = node_protocol::encode(message).map_err(Fault::Protocol)?;
        application
            .send(Frame::text(text))
            .await
            .map_err(|error| Fault::Owner(error.to_string()))
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
        let channel = format!("{}.media-{}", self.names, self.sounds_opened);
        member.sound = Some(Sound {
            channel: channel.clone(),
            bridge: format!("{}.bridge-{}", self.names, self.sounds_opened),
            sink: None,
            built: Built::Asked,
            beside: Beside::Nobody,
            tap: None,
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
            Pending::Step(participant.clone(), Step::MediaChannel),
            "POST",
            &uri,
        );
        None
    }

    /// A channel that entered the application may be a participant's media
    /// channel or a tap on them: either can be put into a bridge from now on.
    fn path_channel_entered(&mut self, channel: &str) {
        let mut bridge = None;
        let whose = self.members.iter_mut().find_map(|member| {
            let sound = member.sound.as_mut()?;
            if sound.channel == channel && sound.built == Built::Asked {
                sound.built = Built::Entered;
                bridge = Some(sound.bridge.clone());
            } else {
                let tap = sound.tap.as_mut().filter(|tap| tap.channel == channel)?;
                tap.entered = true;
            }
            Some(member.id.clone())
        });
        let Some(participant) = whose else {
            return;
        };
        if let Some(bridge) = bridge {
            self.ask(
                Pending::Step(participant.clone(), Step::Bridge),
                "POST",
                &format!("bridges?type=mixing&bridgeId={}", ari::query(&bridge)),
            );
        }
        self.arrange(&participant);
    }

    /// Ask Asterisk for whatever is still missing between where a
    /// participant's channel and audio path are and where they should be.
    ///
    /// Called whenever that may have changed: the participant was connected
    /// or taken out of a connection, their audio path was opened, a channel
    /// of the path entered the application. A channel can be put into a
    /// bridge only once it is in the application, so what cannot be asked
    /// for yet is asked for when this is called again.
    ///
    /// The participant changes bridges in an order that leaves the media
    /// channel alone for a moment rather than with two sources at once:
    /// tried against the pinned Asterisk, the first loses at most one frame
    /// of twenty milliseconds, and the second doubles the audio.
    fn arrange(&mut self, participant: &ParticipantId) {
        let next_tap = self.taps_made + 1;
        let bridges = &self.group_bridges;
        let names = &self.names;
        let Some(member) = self
            .members
            .iter_mut()
            .find(|member| member.id == *participant)
        else {
            return;
        };
        // Their channel is being taken down by the controller itself.
        if member.removed_by_us {
            return;
        }
        let mut requests = Vec::new();
        let mut tap_made = None;
        match member.group {
            Some(group) => {
                let Some(bridge) = bridges.get(&group) else {
                    return;
                };
                arrange_in_group(
                    member,
                    (group, bridge),
                    names,
                    next_tap,
                    &mut requests,
                    &mut tap_made,
                );
            }
            None => arrange_alone(member, bridges, &mut requests),
        }
        if let Some(tap) = tap_made {
            self.taps_made = next_tap;
            // Said before the tap is asked for: not a word about it may go astray.
            self.asterisk.own(&tap);
        }
        for (pending, method, uri) in requests {
            self.ask(pending, method, &uri);
        }
    }

    /// Asterisk answered a step of putting a participant in place.
    async fn step_answered(
        &mut self,
        participant: &ParticipantId,
        step: Step,
        status_code: u16,
    ) -> Result<(), Fault> {
        // A participant who has left meanwhile has no place to be put in:
        // whatever Asterisk answered about it no longer matters.
        let Some(member) = self.member(participant) else {
            return Ok(());
        };
        if !(200..=299).contains(&status_code) {
            // Asterisk refuses a step about a participant who is hanging up
            // — and its answer may come before its word that they left. So
            // a refusal alone decides nothing: Asterisk is asked whether
            // their channel is still in the application.
            let channel = member.channel.clone();
            self.ask(
                Pending::Verdict(participant.clone(), step, status_code),
                "GET",
                &format!("channels/{}", ari::query(&channel)),
            );
            return Ok(());
        }
        if step == Step::Join {
            if let Some(sound) = &mut member.sound {
                sound.joined = true;
            }
            return self.feed(participant).await;
        }
        Ok(())
    }

    /// Asterisk said whether the channel of a participant is still in the
    /// application, after it had refused a step about them.
    ///
    /// A participant who is leaving is not a failure: the word that they
    /// left is on its way, and with it everything is put right. A step
    /// refused for a participant who stays is one: they would be left where
    /// they should not be, and nobody would know.
    fn verdict(
        &mut self,
        participant: &ParticipantId,
        step: Step,
        refused_with: u16,
        status_code: u16,
        body: &str,
    ) -> Result<(), Fault> {
        if self.member(participant).is_none() {
            return Ok(());
        }
        let refused = || {
            format!(
                "answered with status {refused_with} when asked to {step} for {}",
                participant.as_str(),
            )
        };
        let still_here = match status_code {
            200 => ari::is_in_application(body, APPLICATION).map_err(|error| {
                Fault::Asterisk(format!(
                    "{}, and described their channel unreadably: {error}",
                    refused()
                ))
            })?,
            // Their channel is no more.
            404 => false,
            other => {
                return Err(Fault::Asterisk(format!(
                    "{}, and with {other} when asked about their channel",
                    refused()
                )));
            }
        };
        if still_here {
            return Err(Fault::Asterisk(refused()));
        }
        eprintln!(
            "conversation {}: Asterisk {}, who is leaving",
            self.id,
            refused()
        );
        Ok(())
    }

    /// The application connects participants to each other.
    ///
    /// Whoever of them is connected to others already stays so: their groups
    /// become one with everyone named. Taking someone out of a connection
    /// is `separate`, and nothing else does it.
    async fn connect(&mut self, id: u64, participants: &[ParticipantId]) -> Result<(), Fault> {
        let mut named: Vec<&ParticipantId> = participants.iter().collect();
        named.sort_unstable_by(|one, other| one.as_str().cmp(other.as_str()));
        named.dedup();
        if named.len() < 2 || named.len() != participants.len() {
            return self.reject(id, CommandRejection::TooFewParticipants).await;
        }
        for participant in participants {
            let in_conversation =
                |member: &Member| member.state == ParticipantState::InConversation;
            if let Err(reason) = self.member_who(participant, in_conversation) {
                return self.reject(id, reason).await;
            }
        }
        self.gather(participants);
        self.accept(id).await
    }

    /// Put participants into one group, together with whoever any of them
    /// is connected to already.
    fn gather(&mut self, participants: &[ParticipantId]) {
        let mut groups: Vec<u64> = participants
            .iter()
            .filter_map(|participant| {
                self.members
                    .iter()
                    .find(|member| member.id == *participant)
                    .and_then(|member| member.group)
            })
            .collect();

        // They gather in the group of the first of them who has one.
        let group = if let Some(group) = groups.first() {
            *group
        } else {
            self.groups_made += 1;
            let bridge = format!("{}.group-{}", self.names, self.groups_made);
            self.group_bridges.insert(self.groups_made, bridge.clone());
            self.ask(
                Pending::Group,
                "POST",
                &format!("bridges?type=mixing&bridgeId={}", ari::query(&bridge)),
            );
            self.groups_made
        };
        let joining: Vec<ParticipantId> = self
            .members
            .iter_mut()
            .filter(|member| {
                participants.contains(&member.id)
                    || member.group.is_some_and(|theirs| groups.contains(&theirs))
            })
            .map(|member| {
                member.group = Some(group);
                member.id.clone()
            })
            .collect();
        for participant in &joining {
            self.arrange(participant);
        }
        // The other groups have nobody left in them.
        groups.sort_unstable();
        groups.dedup();
        for emptied in groups.into_iter().filter(|other| *other != group) {
            self.settle_group(emptied);
        }
    }

    /// The application takes a participant out of the group they are in.
    /// One who is connected to nobody is already where the command wants them.
    async fn separate(&mut self, id: u64, participant: &ParticipantId) -> Result<(), Fault> {
        let Some(member) = self.member(participant) else {
            return self.reject(id, CommandRejection::UnknownParticipant).await;
        };
        if let Some(group) = member.group.take() {
            self.arrange(participant);
            self.settle_group(group);
        }
        self.accept(id).await
    }

    /// A group has lost a participant. One who is left in it alone is
    /// connected to nobody, and a group of nobody is removed.
    fn settle_group(&mut self, group: u64) {
        let mut left_in_it = self
            .members
            .iter_mut()
            .filter(|member| member.group == Some(group));
        let (first, second) = (left_in_it.next(), left_in_it.next());
        if second.is_some() {
            return;
        }
        let alone = first.map(|member| {
            member.group = None;
            member.id.clone()
        });
        if let Some(participant) = alone {
            self.arrange(&participant);
        }
        if let Some(bridge) = self.group_bridges.remove(&group) {
            self.ask(Pending::Internal, "DELETE", &format!("bridges/{bridge}"));
        }
    }

    /// End a participant's audio path in Asterisk. Asterisk then closes the
    /// media connection itself.
    fn close_sound(&self, sound: &Sound) {
        self.node.door.forget(&sound.channel);
        // A tap ends with the channel it is on; one that Asterisk has not
        // ended yet is ended here.
        let tap = sound
            .tap
            .iter()
            .map(|tap| format!("channels/{}", tap.channel));
        let path = [
            format!("channels/{}", sound.channel),
            format!("bridges/{}", sound.bridge),
        ];
        for gone in tap.chain(path) {
            self.asterisk.ask("sound-closed", "DELETE", &gone);
        }
    }

    /// Add a participant by dialling: for the application's `dial` command
    /// or its request to start the conversation, or for the controller
    /// itself, which carries out the transfer of a fallback.
    ///
    /// Whoever asks, the node judges the number the same way and counts the
    /// call against its line.
    async fn dial(
        &mut self,
        asker: Asker,
        number: &PhoneNumber,
        line: &LineId,
        answer_limit_ms: u64,
    ) -> Result<Option<CommandRejection>, Fault> {
        let applied = self.node.applied();
        let refused = match destination::judge(&applied.settings, line, number) {
            Ok(route) => match self.node.lines.take(route.line) {
                Some(place) => Ok((route, place)),
                None => Err(CommandRejection::OutboundLimitReached),
            },
            Err(reason) => Err(reason),
        };
        let (route, place) = match refused {
            Ok(placed) => placed,
            Err(reason) => {
                if let Asker::Command(id) = asker {
                    self.reject(id, reason).await?;
                }
                return Ok(Some(reason));
            }
        };

        self.participants += 1;
        let participant = Participant {
            id: participant_id(&self.node, self.participants)?,
            medium: ParticipantMedium::TelephoneNetwork,
            number: Some(number.clone()),
        };
        let channel = format!("{}.participant-{}", self.names, self.participants);
        // Written on the channel as it is made: until they answer, nothing
        // can be written on it.
        let mut notes = serde_json::Map::new();
        for (name, value) in [
            (NOTE_CONVERSATION, self.id.clone()),
            (NOTE_PARTICIPANT, participant.id.as_str().to_owned()),
            (NOTE_NUMBER, number.as_str().to_owned()),
            (NOTE_LINE, line.as_str().to_owned()),
        ]
        .into_iter()
        .chain(origin_notes(&self.origin))
        {
            notes.insert(name.to_owned(), serde_json::Value::String(value));
        }
        let body = serde_json::json!({ "variables": notes }).to_string();
        self.asterisk.own(&channel);
        self.members.push(Member {
            name: None,
            watch: Watch::new(Duration::ZERO),
            leaving: None,
            id: participant.id.clone(),
            number: participant.number.clone(),
            channel: channel.clone(),
            state: ParticipantState::Ringing,
            removed_by_us: false,
            sound: None,
            awaiting_answer: true,
            _place: Some(place),
            group: None,
            put: Put::Nowhere,
        });
        if matches!(asker, Asker::Command(_) | Asker::Request) {
            self.unannounced.insert(participant.id.clone(), Vec::new());
        }

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
        self.ask_with(Pending::Dial(asker, participant), "POST", &uri, body);
        Ok(None)
    }

    /// Asterisk answered the request to place a call. Only now does whoever
    /// asked for it learn of the participant — or that there is none. With
    /// nobody asking — the call is the controller's own, or the instance
    /// that asked is gone — there is nobody to tell.
    async fn call_placed(
        &mut self,
        asker: Asker,
        participant: Participant,
        status_code: u16,
    ) -> Result<(), Fault> {
        let waiting = self.unannounced.remove(&participant.id).unwrap_or_default();
        if !(200..=299).contains(&status_code) {
            // Asterisk did not begin the call; nobody was called. Only its
            // "no such endpoint" says the operator's trunk is not there for
            // it — any other refusal, or no answer from it at all, is a call
            // the node did not place.
            self.members.retain(|member| member.id != participant.id);
            let why = if status_code == 404 {
                CommandRejection::NoOperatorForDestination
            } else {
                CommandRejection::CallNotPlaced
            };
            if matches!(&self.fallback, FallbackProgress::Calling(dialled) if *dialled == participant.id)
            {
                self.fallback_failure
                    .get_or_insert(FallbackFailure::Refused { rejection: why });
            }
            return match asker {
                Asker::Command(id) => self.reject(id, why).await,
                Asker::Request => {
                    self.first_placed = Some(Err(why));
                    Ok(())
                }
                Asker::Nobody => Ok(()),
            };
        }
        let id = match asker {
            Asker::Command(id) => id,
            // The conversation is offered with them; what Asterisk says of
            // them meanwhile waits for its owner.
            Asker::Request => {
                self.first_placed = Some(Ok(participant));
                return Ok(());
            }
            Asker::Nobody => return Ok(()),
        };
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
        let Some(application) = &mut self.application else {
            return Ok(());
        };
        for frame in frames {
            application
                .send(Frame::binary(frame))
                .await
                .map_err(|error| Fault::Owner(error.to_string()))?;
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
    ///
    /// With no instance owning the conversation, nobody is told by a closing
    /// connection: each participant dropped here is then something the node
    /// did on its own, and is reported as [`Self::hang_up`] reports it.
    async fn abandon(&mut self, fault: &Fault) {
        let unowned = self.application.is_none();
        for member in &self.members {
            self.asterisk
                .ask("abandon", "DELETE", &format!("channels/{}", member.channel));
            if let Some(sound) = &member.sound {
                self.close_sound(sound);
            }
            if unowned && !member.removed_by_us {
                let participant = member.id.clone();
                let being_called = member.awaiting_answer;
                self.report_dropped(participant, being_called);
            }
        }
        let mut groups: Vec<u64> = self
            .members
            .iter()
            .filter_map(|member| member.group)
            .collect();
        groups.sort_unstable();
        groups.dedup();
        for group in groups {
            if let Some(bridge) = self.group_bridges.get(&group) {
                self.asterisk
                    .ask("abandon", "DELETE", &format!("bridges/{bridge}"));
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
        if let Some(application) = &mut self.application {
            let _ = application.close(Some(close)).await;
        }
    }
}

/// What is still to be asked of Asterisk for a participant who is in a
/// group: their channel in the group's bridge and, if they have an audio
/// path, a tap on them beside its media channel.
fn arrange_in_group(
    member: &mut Member,
    (group, bridge): (u64, &str),
    names: &str,
    next_tap: u64,
    requests: &mut Vec<Request>,
    tap_made: &mut Option<String>,
) {
    let enter_group = |member: &mut Member, requests: &mut Vec<Request>| {
        if member.put != Put::InGroup(group) {
            member.put = Put::InGroup(group);
            requests.push((
                Pending::Step(member.id.clone(), Step::EnterGroup),
                "POST",
                format!(
                    "bridges/{bridge}/addChannel?channel={}",
                    ari::query(&member.channel),
                ),
            ));
        }
    };
    let Some(sound) = &mut member.sound else {
        return enter_group(member, requests);
    };
    if sound.built == Built::Asked {
        // The path is not built yet; nothing holds the participant back.
        return enter_group(member, requests);
    }
    let Some(tap) = &sound.tap else {
        let tap = format!("{names}.tap-{next_tap}");
        requests.push((
            Pending::Step(member.id.clone(), Step::Tap),
            "POST",
            format!(
                "channels/{}/snoop?spy=in&whisper=out&app={}&snoopId={}",
                ari::query(&member.channel),
                ari::query(APPLICATION),
                ari::query(&tap),
            ),
        ));
        *tap_made = Some(tap.clone());
        sound.tap = Some(Tap {
            channel: tap,
            entered: false,
        });
        // Until the tap can take their place the participant stays where
        // they are.
        return;
    };
    if !tap.entered {
        return;
    }
    let tap = tap.channel.clone();
    let beside_media = (sound.beside != Beside::Tap).then(|| {
        sound.beside = Beside::Tap;
        join(&member.id, sound, &tap)
    });
    enter_group(member, requests);
    requests.extend(beside_media);
}

/// What is still to be asked of Asterisk for a participant who is connected
/// to nobody: no tap on them, their channel beside the media channel of
/// their audio path if they have one, and in no bridge if they have none.
fn arrange_alone(member: &mut Member, bridges: &HashMap<u64, String>, requests: &mut Vec<Request>) {
    let leave_group = |member: &mut Member, requests: &mut Vec<Request>| {
        if let Put::InGroup(group) = member.put {
            member.put = Put::Nowhere;
            if let Some(bridge) = bridges.get(&group) {
                requests.push((
                    Pending::Step(member.id.clone(), Step::LeaveGroup),
                    "POST",
                    format!(
                        "bridges/{bridge}/removeChannel?channel={}",
                        ari::query(&member.channel),
                    ),
                ));
            }
        }
    };
    let Some(sound) = &mut member.sound else {
        return leave_group(member, requests);
    };
    if let Some(tap) = sound.tap.take() {
        if sound.beside == Beside::Tap {
            sound.beside = Beside::Nobody;
        }
        requests.push((
            Pending::Internal,
            "DELETE",
            format!("channels/{}", ari::query(&tap.channel)),
        ));
    }
    if sound.built == Built::Asked {
        return leave_group(member, requests);
    }
    if sound.beside != Beside::Participant {
        sound.beside = Beside::Participant;
        // Asterisk moves a channel that is in another bridge.
        member.put = Put::BesideMedia;
        let channel = member.channel.clone();
        requests.push(join(&member.id, sound, &channel));
    }
}

/// The request that puts `companion` — the participant, or a tap on them —
/// into the bridge of an audio path, and the media channel with it if it is
/// not there yet.
fn join(participant: &ParticipantId, sound: &mut Sound, companion: &str) -> Request {
    let mut channels = ari::query(companion);
    if sound.built == Built::Entered {
        sound.built = Built::Bridged;
        channels.push(',');
        channels.push_str(&ari::query(&sound.channel));
    }
    (
        Pending::Step(participant.clone(), Step::Join),
        "POST",
        format!("bridges/{}/addChannel?channel={channels}", sound.bridge),
    )
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

/// The identifier of the `number`th participant this controller adds to a
/// conversation. It carries the controller's run: a participant is never
/// given an identifier that one before them had, also when a controller
/// that is started again carries the conversation on.
fn participant_id(node: &Node, number: u64) -> Result<ParticipantId, Fault> {
    format!("p-{}-{number}", node.run)
        .parse()
        .map_err(|_| Fault::Asterisk("participant identifier is not valid".into()))
}
