// crates/asterisk-controller/src/drain.rs

//! Letting the node empty before it is replaced.
//!
//! Told with SIGUSR1 — by the launcher, which whoever replaces the node
//! tells first ([`crate::install`]) — the controller takes no new call from
//! then on: a call that arrives is turned away as congestion, which the
//! operator hears as 503 and may take to another of its routes, and the
//! application's request to start a conversation is refused as
//! `node_not_ready`. Conversations already going on are carried to their
//! end as before.
//!
//! How many are left is written to [`FILE`] in the run directory each time
//! it changes, until none is left: whoever replaces the node waits for `0`,
//! then restarts it. Nothing turns a drain back: a node that empties is
//! replaced.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::{asterisk::Asterisk, node::Node, state_files};

/// The file of the run directory that says how many conversations are left
/// while the node empties.
pub const FILE: &str = "draining";

/// How often the conversations left are counted.
const COUNTED_EVERY: Duration = Duration::from_millis(500);

/// Whether the node is emptying.
#[derive(Default)]
pub struct Drain(AtomicBool);

impl Drain {
    /// Whether the node takes no new call.
    pub fn is_on(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Take no new call from now on. Returns whether this turned it on.
    fn turn_on(&self) -> bool {
        !self.0.swap(true, Ordering::SeqCst)
    }
}

/// The node is to empty: take no new call, and say how many conversations
/// are left until none is.
pub async fn begin(node: Arc<Node>, asterisk: Arc<Asterisk>) {
    if !node.drain.turn_on() {
        return;
    }
    let file = node.config.run.join(FILE);
    let mut said = None;
    loop {
        let left = asterisk.conversations();
        if said != Some(left) {
            if let Err(error) = state_files::replace(&file, format!("{left}\n").as_bytes()) {
                eprintln!(
                    "asterisk-controller: draining — {} cannot be written: {error}",
                    file.display()
                );
            }
            if left == 0 {
                eprintln!("asterisk-controller: draining — no conversation is left");
                return;
            }
            eprintln!(
                "asterisk-controller: draining — new calls are turned away; {left} \
                 conversation(s) go on"
            );
            said = Some(left);
        }
        tokio::time::sleep(COUNTED_EVERY).await;
    }
}
