// crates/asterisk-controller/src/node.rs

//! What every conversation of the node shares.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError, RwLock},
};

use node_protocol::messages::Line;

use crate::{application::Application, config::Config, media, reports::Journal, settings::Applied};

/// The node, as its conversations see it.
pub struct Node {
    /// What the controller was told at start.
    pub config: Config,
    /// The way to the application.
    pub application: Application,
    /// The version of the Asterisk the controller drives.
    pub asterisk_version: String,
    /// What sets this run of the controller apart from every other: part
    /// of every name it gives in Asterisk and of every participant's
    /// identifier. Asterisk keeps a bridge's name taken for as long as
    /// anything still holds the bridge — after it was removed, too — so a
    /// controller started again never reuses a name the one before it gave.
    pub run: String,
    /// The settings the node runs on. They change while the node runs; a
    /// conversation takes the ones in force at each decision.
    pub applied: RwLock<Arc<Applied>>,
    /// What the node did on its own and the application has not received.
    pub reports: Journal,
    /// Where media connections meet the conversations that asked for them.
    pub door: media::Door,
    /// How busy each outbound line is.
    pub lines: Lines,
}

impl Node {
    /// The settings in force now.
    pub fn applied(&self) -> Arc<Applied> {
        // Settings are only ever replaced whole: a holder that panicked
        // cannot have left them half-changed.
        Arc::clone(&self.applied.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// From now on the node runs on these settings.
    pub fn apply(&self, applied: Applied) {
        *self.applied.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(applied);
    }
}

/// How many calls each outbound line is carrying now, over the whole node.
///
/// A line's limit of simultaneous outbound calls is a limit on money: it
/// holds for the line, not for a conversation, so the count lives here.
#[derive(Default)]
pub struct Lines(Arc<Mutex<HashMap<String, u64>>>);

impl Lines {
    /// A place on a line for an outbound call that is already being
    /// carried — found in Asterisk when the controller started. The line's
    /// limit was judged when the call was placed; it is not judged again.
    pub fn resume(&self, line: &str) -> Place {
        let mut busy = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        *busy.entry(line.to_owned()).or_insert(0) += 1;
        Place {
            lines: Arc::clone(&self.0),
            line: line.to_owned(),
        }
    }

    /// Take a place on a line for one more outbound call.
    ///
    /// `None` when the line already carries as many as its settings allow.
    /// The place is held for as long as the returned value lives, and given
    /// back when it is dropped — however the call ends.
    pub fn take(&self, line: &Line) -> Option<Place> {
        let mut busy = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let carried = busy.entry(line.id.as_str().to_owned()).or_insert(0);
        if *carried >= line.concurrent_outbound_limit {
            return None;
        }
        *carried += 1;
        Some(Place {
            lines: Arc::clone(&self.0),
            line: line.id.as_str().to_owned(),
        })
    }
}

/// One outbound call's place on its line.
pub struct Place {
    lines: Arc<Mutex<HashMap<String, u64>>>,
    line: String,
}

impl Drop for Place {
    fn drop(&mut self) {
        let mut busy = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(carried) = busy.get_mut(&self.line) {
            *carried = carried.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use node_protocol::messages::Settings;
    use serde_json::json;

    use super::Lines;

    #[test]
    fn a_line_carries_no_more_than_its_limit_and_a_finished_call_frees_its_place()
    -> Result<(), Box<dyn std::error::Error>> {
        let settings: Settings = node_protocol::decode_value(json!({
            "operators": [],
            "lines": [{
                "id": "main", "number": "+19715870050", "operator": "carrier",
                "allowed_countries": ["US"], "concurrent_outbound_limit": 2
            }],
            "entries": [],
            "prompts": []
        }))?;
        let line = settings.lines.first().ok_or("no line")?;
        let lines = Lines::default();
        let first = lines.take(line);
        let second = lines.take(line);
        assert!(first.is_some() && second.is_some());
        assert!(lines.take(line).is_none());
        drop(first);
        assert!(lines.take(line).is_some());
        Ok(())
    }
}
