// crates/asterisk-controller/src/reports.rs

//! The journal of reports: what the node did on its own, kept until the
//! application has it.
//!
//! Nothing the node does on its own goes unreported — and a report must
//! outlive the controller that made it: one that dies between carrying out
//! a fallback and telling the application would otherwise leave no trace.
//! So a report is written into the node's state directory first and sent
//! after, on the service connection whenever there is one; the
//! application's word that it has the report is what removes it. A report
//! sent again after a lost connection keeps its identifier, and the
//! application knows it by that.
//!
//! Each line of the journal is the report as the service connection carries
//! it, so what is read back is held to the protocol description like any
//! message.

use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
    time::{SystemTime, UNIX_EPOCH},
};

use node_protocol::messages::{NodeMessage, Report};
use tokio::sync::Notify;

use crate::state_files;

/// The file of the state directory that holds reports not yet received.
const FILE: &str = "reports.jsonl";

/// The node's reports not yet received by the application.
pub struct Journal {
    path: PathBuf,
    /// Writers and the one who forgets take turns: the file is appended
    /// to by conversations and rewritten by the service connection.
    turn: Mutex<()>,
    /// Told whenever a report is kept, so that it is sent at once.
    kept: Notify,
}

impl Journal {
    /// The journal of the node whose state directory this is.
    pub fn of(state: &Path) -> Self {
        Self {
            path: state.join(FILE),
            turn: Mutex::new(()),
            kept: Notify::new(),
        }
    }

    /// Keep a report until the application has it.
    ///
    /// A report that cannot be written is said so in the controller's
    /// output, loudly: the node has done something it cannot tell.
    pub fn keep(&self, report: Report) {
        let id = uuid::Uuid::new_v4().to_string();
        let written = id
            .parse()
            .map_err(|_| "a report identifier is not valid".to_owned())
            .and_then(|id| {
                node_protocol::encode(&NodeMessage::Report { id, report })
                    .map_err(|error| error.to_string())
            })
            .and_then(|line| {
                let _turn = self.turn.lock().unwrap_or_else(PoisonError::into_inner);
                state_files::append_line(&self.path, &line)
                    .map_err(|error| format!("{}: {error}", self.path.display()))
            });
        match written {
            Ok(()) => self.kept.notify_one(),
            Err(problem) => eprintln!(
                "asterisk-controller: a REPORT IS LOST — {problem}; what the node did on its \
                 own will not reach the application"
            ),
        }
    }

    /// The reports kept and not yet received, in the order they were made,
    /// each as the service connection carries it.
    ///
    /// # Errors
    ///
    /// The journal cannot be read, or holds something that is not a report.
    pub fn kept(&self) -> Result<Vec<(String, NodeMessage)>, String> {
        let _turn = self.turn.lock().unwrap_or_else(PoisonError::into_inner);
        self.read()
    }

    /// Read the journal; the caller holds the turn.
    fn read(&self) -> Result<Vec<(String, NodeMessage)>, String> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("{}: {error}", self.path.display())),
        };
        text.lines()
            .filter(|line| !line.is_empty())
            .map(|line| {
                let message: NodeMessage = node_protocol::decode(line)
                    .map_err(|error| format!("{}: {error}", self.path.display()))?;
                match &message {
                    NodeMessage::Report { id, .. } => Ok((id.as_str().to_owned(), message)),
                    NodeMessage::Hello { .. }
                    | NodeMessage::SettingsApplied { .. }
                    | NodeMessage::SettingsRefused { .. }
                    | NodeMessage::RequestResult { .. }
                    | NodeMessage::Ping { .. }
                    | NodeMessage::Pong { .. } => Err(format!(
                        "{}: holds a message that is not a report",
                        self.path.display()
                    )),
                }
            })
            .collect()
    }

    /// The application has a report: forget it. A report already forgotten
    /// is forgotten again without a word — the application may confirm a
    /// report it was sent twice.
    ///
    /// # Errors
    ///
    /// The journal cannot be rewritten.
    pub fn forget(&self, id: &str) -> Result<(), String> {
        let _turn = self.turn.lock().unwrap_or_else(PoisonError::into_inner);
        let failed = |error: std::io::Error| format!("{}: {error}", self.path.display());
        let reports = self.read()?;
        if reports.iter().all(|(kept, _)| kept != id) {
            return Ok(());
        }
        let mut kept = String::new();
        for (_, report) in reports.iter().filter(|(kept, _)| kept != id) {
            let line = node_protocol::encode(report).map_err(|error| error.to_string())?;
            kept.push_str(&line);
            kept.push('\n');
        }
        state_files::replace(&self.path, kept.as_bytes()).map_err(failed)
    }

    /// Wait until a report is kept.
    pub async fn changed(&self) {
        self.kept.notified().await;
    }
}

/// Now, as reports carry it: milliseconds since the Unix epoch.
pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}
