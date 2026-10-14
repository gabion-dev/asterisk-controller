// crates/asterisk-controller/src/recordings.rs

//! Recordings of conversations, from the moment the node begins one until
//! the application has its file.
//!
//! Asterisk writes a recording into its spool directory, in the protocol's
//! one audio format, raw: a recording cut short — Asterisk killed — is still
//! readable up to where it stops. When it is written to its end the node
//! sends the file to the application, `PUT <address>/recordings/<id>`,
//! proving itself as on every connection; only the application's 2xx — given
//! once the file is in its storage — lets the node delete it. Until then the
//! file stays on the node and is sent again: when the application next
//! welcomes the node, and at least every [`RETRY_AFTER`].
//!
//! The node keeps a list of its recordings in the state directory — one
//! file per recording, written when the recording begins — so a controller
//! started again knows of every recording the one before it began: one that
//! is still being written belongs to the conversation that carries on
//! ([`crate::conversation`]), and one that ended while no controller
//! listened is found ended and sent.
//!
//! A recording whose file is not there when it is to be sent is lost: the
//! node says so loudly, takes it off the list and reports the loss to the
//! application ([`crate::reports`]) — the conversation the recording was of
//! may be over by then, so the loss is a report kept until the application
//! has it, not an event of the conversation.

use std::{
    collections::HashSet,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use node_protocol::messages::{ConversationId, RecordingId, Report};
use tokio::sync::Notify;

use crate::{asterisk::Asterisk, node::Node, reports, state_files};

/// The directory of the state directory that lists the node's recordings.
const DIRECTORY: &str = "recordings";
/// Where in the state directory Asterisk writes recordings: the `recording`
/// directory of its spool directory. Asterisk does not make it.
pub const SPOOL: &str = "spool/recording";
/// The format Asterisk writes a recording in — the protocol's audio, which
/// Asterisk knows by this name.
pub const FORMAT: &str = "sln16";
/// Bytes of the protocol's audio in one millisecond: 16 000 samples a
/// second of two bytes each.
const BYTES_PER_MS: u64 = 32;
/// How long the node waits before it sends again a file the application
/// did not take. An application that is away says nothing when it is back
/// until it welcomes the node — which is waited for too — so this bounds
/// how late a file goes to an application that never dropped the service
/// connection but did not take the file.
const RETRY_AFTER: Duration = Duration::from_secs(10);
/// The line of an entry that says the recording is written to its end.
const FINISHED: &str = "finished";
/// What begins the line of an entry naming its conversation.
const CONVERSATION: &str = "conversation ";

/// The node's recordings that the application does not have yet.
pub struct Outbox {
    directory: PathBuf,
    spool: PathBuf,
    /// Recordings a conversation of this controller is writing now: the
    /// conversation says when each ends, nobody else asks.
    owned: Mutex<HashSet<String>>,
    /// Told when there may be something to send.
    wake: Notify,
}

/// One recording on the node's list.
struct Entry {
    recording: String,
    /// The conversation the recording was of, as the list names it. The
    /// node writes every entry with it; one without was not written by the
    /// node.
    conversation: Option<String>,
    finished: bool,
}

impl Outbox {
    /// The recordings of the node whose state directory this is.
    pub fn of(state: &Path) -> Self {
        Self {
            directory: state.join(DIRECTORY),
            spool: state.join(SPOOL),
            owned: Mutex::new(HashSet::new()),
            wake: Notify::new(),
        }
    }

    fn owned(&self) -> MutexGuard<'_, HashSet<String>> {
        // Names are only inserted and removed: a holder that panicked cannot
        // have left the set half-changed.
        self.owned.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A conversation of this controller begins a recording, or carries on
    /// one a controller before it began: it is listed, and its end is the
    /// conversation's to tell.
    ///
    /// # Errors
    ///
    /// The list cannot be written: the recording is not begun, since a file
    /// nobody knows of would never be sent.
    pub fn begin(&self, recording: &str, conversation: &str) -> Result<(), String> {
        self.owned().insert(recording.to_owned());
        let path = self.directory.join(recording);
        if path.is_file() {
            return Ok(());
        }
        let failed = |error: std::io::Error| format!("{}: {error}", path.display());
        fs::create_dir_all(&self.directory).map_err(failed)?;
        state_files::replace(&path, format!("{CONVERSATION}{conversation}\n").as_bytes())
            .map_err(failed)
    }

    /// A recording is written to its end. Returns its length, from its
    /// file — what Asterisk says of a recording's duration in the protocol's
    /// format is twice the truth.
    ///
    /// # Errors
    ///
    /// Its file cannot be read, or the list cannot be written. The recording
    /// is then no longer owned, and the sender finds out about it.
    pub fn finished(&self, recording: &str) -> Result<u64, String> {
        self.owned().remove(recording);
        let length = self.length_ms(recording)?;
        let path = self.directory.join(recording);
        let failed = |error: std::io::Error| format!("{}: {error}", path.display());
        let listed = fs::read_to_string(&path).map_err(failed)?;
        if !listed.lines().any(|line| line == FINISHED) {
            state_files::replace(&path, format!("{listed}{FINISHED}\n").as_bytes())
                .map_err(failed)?;
        }
        self.wake.notify_one();
        Ok(length)
    }

    /// A recording could not be written: there is no file to send.
    pub fn failed(&self, recording: &str) {
        self.owned().remove(recording);
        self.unlist(recording);
    }

    /// A conversation lets go of a recording without knowing its end — the
    /// conversation itself is abandoned. The sender finds out from Asterisk
    /// what became of it.
    pub fn disown(&self, recording: &str) {
        self.owned().remove(recording);
        self.wake.notify_one();
    }

    /// There may be something to send: the application has just welcomed
    /// the node.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// The file Asterisk writes a recording into.
    pub fn file(&self, recording: &str) -> PathBuf {
        self.spool.join(format!("{recording}.{FORMAT}"))
    }

    /// A recording's length, from its file.
    ///
    /// # Errors
    ///
    /// The file cannot be read.
    pub fn length_ms(&self, recording: &str) -> Result<u64, String> {
        let path = self.file(recording);
        let bytes = fs::metadata(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?
            .len();
        Ok(bytes / BYTES_PER_MS)
    }

    /// Take a recording off the list.
    fn unlist(&self, recording: &str) {
        let path = self.directory.join(recording);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => eprintln!("asterisk-controller: {}: {error}", path.display()),
        }
    }

    /// The recordings on the list.
    ///
    /// # Errors
    ///
    /// The list cannot be read.
    fn listed(&self) -> Result<Vec<Entry>, String> {
        let failed = |error: std::io::Error| format!("{}: {error}", self.directory.display());
        let entries = match fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(failed(error)),
        };
        let mut listed = Vec::new();
        for entry in entries {
            let entry = entry.map_err(failed)?;
            let recording = entry.file_name().to_string_lossy().into_owned();
            // A file being replaced is written under a hidden name first.
            if recording.starts_with('.') {
                continue;
            }
            let text = fs::read_to_string(entry.path()).map_err(failed)?;
            listed.push(Entry {
                conversation: text
                    .lines()
                    .find_map(|line| line.strip_prefix(CONVERSATION))
                    .map(str::to_owned),
                finished: text.lines().any(|line| line == FINISHED),
                recording,
            });
        }
        Ok(listed)
    }
}

/// A recording's file is not there when it should be: the recording is
/// lost. It is taken off the list, said so loudly, and reported to the
/// application, which will never get the file — as a report, kept until the
/// application has it, because the conversation the recording was of may be
/// over. A loss whose conversation is not known cannot be reported, only
/// said: the report names the conversation, and a list entry without one
/// was not written by the node.
pub fn lose(node: &Node, recording: &str, conversation: Option<&str>) {
    let outbox = &node.recordings;
    outbox.owned().remove(recording);
    outbox.unlist(recording);
    eprintln!(
        "asterisk-controller: RECORDING {recording} IS LOST — its file is not on the node, \
         and the application will not get it"
    );
    let report = conversation
        .ok_or_else(|| "the list does not name its conversation".to_owned())
        .and_then(|conversation| {
            conversation
                .parse::<ConversationId>()
                .map_err(|_| "the conversation's identifier is not valid".to_owned())
        })
        .and_then(|conversation| {
            recording
                .parse::<RecordingId>()
                .map(|recording| Report::RecordingLost {
                    conversation,
                    recording,
                    at_unix_ms: reports::now_unix_ms(),
                })
                .map_err(|_| "the recording's identifier is not valid".to_owned())
        });
    match report {
        Ok(report) => node.reports.keep(report),
        Err(why) => eprintln!(
            "asterisk-controller: recording {recording}: the loss cannot be reported — {why}"
        ),
    }
}

/// Send the node's recordings to the application for as long as the
/// controller runs.
pub async fn run(node: Arc<Node>, asterisk: Arc<Asterisk>) {
    let outbox = &node.recordings;
    loop {
        let all_sent = match outbox.listed() {
            Ok(listed) => {
                let mut all_sent = true;
                for entry in listed {
                    if !send_or_settle(&node, &asterisk, &entry).await {
                        all_sent = false;
                    }
                }
                all_sent
            }
            Err(problem) => {
                eprintln!("asterisk-controller: the node's recordings cannot be read — {problem}");
                false
            }
        };
        if all_sent {
            outbox.wake.notified().await;
        } else {
            tokio::select! {
                () = outbox.wake.notified() => {}
                () = tokio::time::sleep(RETRY_AFTER) => {}
            }
        }
    }
}

/// Do for one listed recording what its state calls for: send one that is
/// written, find out about one still listed as being written that no
/// conversation of this controller owns. Returns whether nothing is left to
/// do for it now.
async fn send_or_settle(node: &Node, asterisk: &Asterisk, entry: &Entry) -> bool {
    let outbox = &node.recordings;
    let recording = &entry.recording;
    if !entry.finished {
        if outbox.owned().contains(recording) {
            return true;
        }
        // Begun by a controller before this one, or let go of by a
        // conversation that was abandoned, and carried on by nobody here:
        // Asterisk says whether it is still written.
        match asterisk.recording_is_live(recording).await {
            Ok(true) => return true,
            Ok(false) => {
                if !outbox.file(recording).is_file() {
                    lose(node, recording, entry.conversation.as_deref());
                    return true;
                }
                if let Err(problem) = outbox.finished(recording) {
                    eprintln!("asterisk-controller: recording {recording}: {problem}");
                    return false;
                }
            }
            Err(problem) => {
                eprintln!("asterisk-controller: recording {recording}: {problem}");
                return false;
            }
        }
    }
    let file = outbox.file(recording);
    if !file.is_file() {
        lose(node, recording, entry.conversation.as_deref());
        return true;
    }
    let sent = node
        .application
        .put(
            &format!("recordings/{recording}"),
            &file,
            entry.conversation.as_deref(),
        )
        .await;
    match sent {
        Ok(()) => {
            if let Err(problem) = asterisk.delete_stored_recording(recording).await {
                // The application has the file; Asterisk keeps a copy it
                // will not be asked about again.
                eprintln!(
                    "asterisk-controller: recording {recording} is with the application, but \
                     its file could not be deleted from the node — {problem}"
                );
            }
            outbox.unlist(recording);
            eprintln!("asterisk-controller: recording {recording} is with the application");
            true
        }
        Err(problem) => {
            eprintln!(
                "asterisk-controller: recording {recording} is not with the application yet — \
                 {problem}"
            );
            false
        }
    }
}
