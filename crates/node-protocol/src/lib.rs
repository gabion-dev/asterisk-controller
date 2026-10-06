// crates/node-protocol/src/lib.rs
//! Message types of the Gabion node protocol.
//!
//! Everything in [`messages`] is generated at build time from the protocol
//! description (`protocol/node-protocol.schema.json`). The description is
//! strict by construction, and so are these types:
//!
//! - an object with a field the description does not name is refused;
//! - an object missing a required field is refused;
//! - a message, command, event or request of an unknown kind is refused.
//!
//! "Refused" means a decoding error. Nothing is skipped and nothing is
//! filled in with a default.
//!
//! Audio does not travel as these messages. Its binary frames are in
//! [`audio`], written by hand against `protocol/audio-frames.md` and held to
//! it by the shared vectors in `protocol/audio-frames.vectors.json`.

pub mod audio;

/// Text messages of both connections — conversation and service — and the
/// settings the application gives the node.
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
pub mod messages {
    include!(concat!(env!("OUT_DIR"), "/messages.rs"));
}
