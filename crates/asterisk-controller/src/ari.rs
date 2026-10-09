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
    /// The far end of a channel has put the call on hold: no audio is to
    /// be expected from it until it takes the call off hold.
    ChannelHold {
        /// The channel whose far end did so.
        channel: Channel,
    },
    /// The far end of a channel has taken the call off hold.
    ChannelUnhold {
        /// The channel.
        channel: Channel,
    },
    /// Another connection has subscribed to the application: from now on
    /// Asterisk says nothing more about it on this one.
    ApplicationReplaced,
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
            | Self::ChannelDestroyed { channel, .. }
            | Self::ChannelHold { channel }
            | Self::ChannelUnhold { channel } => Some(&channel.id),
            Self::ApplicationReplaced | Self::Other => None,
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

/// The dialplan application a channel is in.
#[derive(Debug, Deserialize)]
pub struct Dialplan {
    app_name: String,
    app_data: String,
}

/// Whether a channel, as Asterisk describes it when asked about the channel,
/// is in the Stasis application `application` now.
///
/// A channel that has left the application — hung up, and not yet destroyed —
/// is still described, but as doing something else.
///
/// # Errors
///
/// Text that is not such a description is an error.
pub fn is_in_application(text: &str, application: &str) -> Result<bool, serde_json::Error> {
    Ok(read_channel(text)?.is_in(application))
}

/// Read Asterisk's description of a channel, as it answers when asked
/// about the channel.
///
/// # Errors
///
/// Text that is not such a description is an error.
#[expect(
    clippy::disallowed_methods,
    reason = "this is Asterisk's language, not the node protocol: there is no description to judge it"
)]
pub fn read_channel(text: &str) -> Result<Channel, serde_json::Error> {
    serde_json::from_str(text)
}

/// The value of a channel variable, as Asterisk answers when asked for it.
#[derive(Debug, Deserialize)]
struct Variable {
    value: String,
}

/// Read the value of a channel variable from Asterisk's answer.
///
/// # Errors
///
/// Text that is not such an answer is an error.
#[expect(
    clippy::disallowed_methods,
    reason = "this is Asterisk's language, not the node protocol: there is no description to judge it"
)]
pub fn read_variable(text: &str) -> Result<String, serde_json::Error> {
    serde_json::from_str(text).map(|Variable { value }| value)
}

/// A bridge, as Asterisk describes it.
#[derive(Debug, Deserialize)]
pub struct Bridge {
    /// The channels in it.
    pub channels: Vec<String>,
}

/// Read Asterisk's description of a bridge.
///
/// # Errors
///
/// Text that is not such a description is an error.
#[expect(
    clippy::disallowed_methods,
    reason = "this is Asterisk's language, not the node protocol: there is no description to judge it"
)]
pub fn read_bridge(text: &str) -> Result<Bridge, serde_json::Error> {
    serde_json::from_str(text)
}

/// A channel, as Asterisk describes it.
#[derive(Debug, Deserialize)]
pub struct Channel {
    /// Asterisk's identifier of the channel.
    pub id: String,
    /// Asterisk's name of the channel, its technology first
    /// (`PJSIP/operator-00000001`): what Asterisk finds a channel by fastest.
    pub name: String,
    /// State name, e.g. `Ring` or `Up`.
    pub state: String,
    /// Who is calling.
    pub caller: Caller,
    /// What it is doing.
    dialplan: Dialplan,
}

impl Channel {
    /// Whether the channel is in the Stasis application `application`.
    pub fn is_in(&self, application: &str) -> bool {
        // The application's own arguments follow its name after a comma.
        let entered = self.dialplan.app_data.split(',').next();
        self.dialplan.app_name == "Stasis" && entered == Some(application)
    }
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

/// What Asterisk counts of a channel's audio, as much as the controller reads.
#[derive(Debug, Deserialize)]
struct RtpStatistics {
    /// Packets received from the far end.
    rxcount: u64,
}

/// Read how many audio packets a channel has received, from Asterisk's
/// answer about its RTP statistics.
///
/// # Errors
///
/// Text that is not such an answer is an error.
#[expect(
    clippy::disallowed_methods,
    reason = "this is Asterisk's language, not the node protocol: there is no description to judge it"
)]
pub fn read_received_packets(text: &str) -> Result<u64, serde_json::Error> {
    serde_json::from_str::<RtpStatistics>(text).map(|statistics| statistics.rxcount)
}

#[cfg(test)]
mod tests {
    use super::{is_in_application, read_received_packets};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// A channel as Asterisk describes it, doing `app_name(app_data)`.
    fn doing(app_name: &str, app_data: &str) -> String {
        serde_json::json!({
            "id": "1759792000.7",
            "name": "PJSIP/operator-00000007",
            "state": "Up",
            "caller": { "name": "", "number": "" },
            "dialplan": {
                "context": "default", "exten": "s", "priority": 1,
                "app_name": app_name, "app_data": app_data
            }
        })
        .to_string()
    }

    #[test]
    fn the_packets_a_channel_received_are_read_from_its_statistics() -> TestResult {
        let statistics = serde_json::json!({
            "channel_uniqueid": "1759792000.7", "rxcount": 512, "txcount": 498,
            "rxjitter": 0.0, "rxploss": 0, "local_ssrc": 1, "remote_ssrc": 2
        });
        assert_eq!(read_received_packets(&statistics.to_string())?, 512);
        assert!(read_received_packets("{}").is_err());
        Ok(())
    }

    #[test]
    fn a_channel_in_the_application_is_in_it_with_arguments_or_without() -> TestResult {
        assert!(is_in_application(&doing("Stasis", "gabion"), "gabion")?);
        assert!(is_in_application(
            &doing("Stasis", "gabion,dialed_number,+19715870050"),
            "gabion"
        )?);
        Ok(())
    }

    #[test]
    fn a_channel_doing_anything_else_is_not_in_it() -> TestResult {
        // It has left for the rest of the dialplan, is in another
        // application, or in one whose name only begins the same.
        for description in [
            doing("Hangup", ""),
            doing("", ""),
            doing("Stasis", "the-far-end"),
            doing("Stasis", "gabion-other,x"),
            doing("Wait", "gabion"),
        ] {
            assert!(!is_in_application(&description, "gabion")?, "{description}");
        }
        Ok(())
    }

    #[test]
    fn what_is_not_a_description_of_a_channel_is_an_error() {
        assert!(is_in_application("{\"message\":\"Channel not found\"}", "gabion").is_err());
        assert!(is_in_application("", "gabion").is_err());
    }
}
