//! Message types of the Gabion node protocol.
//!
//! Everything in [`conversation`] is generated at build time from the
//! protocol description (`protocol/conversation.schema.json`). The
//! description is strict by construction, and so are these types:
//!
//! - an object with a field the description does not name is refused;
//! - an object missing a required field is refused;
//! - a message, command or event of an unknown kind is refused.
//!
//! "Refused" means a decoding error. Nothing is skipped and nothing is
//! filled in with a default.

/// Messages of a conversation connection: the controller's and the
/// application's text messages, with the commands, events and values they
/// carry.
#[allow(
    missing_docs,
    clippy::all,
    clippy::pedantic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::wildcard_enum_match_arm,
    reason = "generated code: its shape is decided by the generator, its content by the protocol description"
)]
pub mod conversation {
    include!(concat!(env!("OUT_DIR"), "/conversation.rs"));
}
