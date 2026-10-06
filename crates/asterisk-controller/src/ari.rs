// crates/asterisk-controller/src/ari.rs

//! Asterisk's own control language (ARI), as much of it as the controller
//! speaks.
//!
//! This is the one place that knows the shape of what Asterisk says. It is
//! not the Gabion node protocol and is not held to its strictness: Asterisk
//! adds fields and events between versions, and the controller reads the
//! ones it knows and leaves the rest.

use std::fmt::Write as _;

use serde::Deserialize;

/// Something Asterisk says about the node's application.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum Message {
    /// A channel entered the application.
    StasisStart {
        /// Arguments the dialplan gave the application.
        args: Vec<String>,
        /// The channel.
        channel: Channel,
    },
    /// A channel left the application.
    StasisEnd {
        /// The channel.
        channel: Channel,
    },
    /// A channel changed state.
    ChannelStateChange {
        /// The channel, in its new state.
        channel: Channel,
    },
    /// A channel received a tone digit.
    ChannelDtmfReceived {
        /// The digit.
        digit: String,
        /// The channel.
        channel: Channel,
    },
    /// A channel is no more.
    ChannelDestroyed {
        /// Why, as the telephone network numbers its reasons.
        cause: i64,
        /// The channel.
        channel: Channel,
    },
    /// Anything else Asterisk says.
    #[serde(other)]
    Other,
}

impl Message {
    /// The channel a message is about, when it is about one.
    pub fn channel(&self) -> Option<&str> {
        match self {
            Self::StasisStart { channel, .. }
            | Self::StasisEnd { channel }
            | Self::ChannelStateChange { channel }
            | Self::ChannelDtmfReceived { channel, .. }
            | Self::ChannelDestroyed { channel, .. } => Some(&channel.id),
            Self::Other => None,
        }
    }
}

/// What is in a Stasis application, as Asterisk describes it.
#[derive(Debug, Deserialize)]
pub struct Application {
    /// Channels the application is told about.
    pub channel_ids: Vec<String>,
    /// Bridges the application is told about.
    pub bridge_ids: Vec<String>,
}

/// Read Asterisk's description of an application.
///
/// # Errors
///
/// Text that is not such a description is an error.
#[expect(
    clippy::disallowed_methods,
    reason = "this is Asterisk's language, not the node protocol: there is no description to judge it"
)]
pub fn read_application(text: &str) -> Result<Application, serde_json::Error> {
    serde_json::from_str(text)
}

/// A channel, as Asterisk describes it.
#[derive(Debug, Deserialize)]
pub struct Channel {
    /// Asterisk's identifier of the channel.
    pub id: String,
    /// State name, e.g. `Ring` or `Up`.
    pub state: String,
    /// Who is calling.
    pub caller: Caller,
}

/// The calling party of a channel.
#[derive(Debug, Deserialize)]
pub struct Caller {
    /// The caller's number as the medium gave it; may be empty.
    pub number: String,
}

/// Channel state in which the party has answered.
pub const STATE_UP: &str = "Up";

/// Read one message of Asterisk.
///
/// # Errors
///
/// Text that is not JSON, or a known message without a field the controller
/// needs, is an error: the controller does not act on what it could not read.
#[expect(
    clippy::disallowed_methods,
    reason = "this is Asterisk's language, not the node protocol: there is no description to judge it"
)]
pub fn read(text: &str) -> Result<Message, serde_json::Error> {
    serde_json::from_str(text)
}

/// A value as it must be written into the query of a request: everything
/// but letters, digits and `-._~` is escaped, so a channel name with its
/// slashes and semicolons stays one value.
pub fn query(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            escaped.push(char::from(byte));
        } else {
            // Writing into a `String` cannot fail.
            let _ = write!(escaped, "%{byte:02X}");
        }
    }
    escaped
}
