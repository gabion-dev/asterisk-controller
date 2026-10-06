// crates/asterisk-controller/src/ari.rs

//! Asterisk's own control language (ARI), as much of it as the controller
//! speaks.
//!
//! This is the one place that knows the shape of what Asterisk says. It is
//! not the Gabion node protocol and is not held to its strictness: Asterisk
//! adds fields and events between versions, and the controller reads the
//! ones it knows and leaves the rest.

use serde::Deserialize;
use serde_json::json;

/// A message Asterisk sends on a call's control connection.
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
    /// The answer to a request the controller sent on this connection.
    #[serde(rename = "RESTResponse")]
    RestResponse {
        /// The identifier the controller gave the request.
        request_id: String,
        /// HTTP status of the answer.
        status_code: u16,
    },
    /// Anything else Asterisk says.
    #[serde(other)]
    Other,
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

/// The text of a request to Asterisk over the call's own connection.
pub fn request(request_id: &str, method: &str, uri: &str) -> String {
    json!({
        "type": "RESTRequest",
        "request_id": request_id,
        "method": method,
        "uri": uri,
    })
    .to_string()
}
