// crates/node-protocol/src/audio.rs

//! Binary audio frames of a conversation connection.
//!
//! The format is described in `protocol/audio-frames.md`. This module is one
//! of its two implementations — the other lives in the application — and
//! the shared vectors in `protocol/audio-frames.vectors.json` are what keeps
//! the two from drifting apart: both must decode, and refuse, the same frames.

use std::fmt;

use crate::messages::{ParticipantId, SegmentId};

/// Bytes of twenty milliseconds of audio: 320 samples of 16 bits.
pub const HEARD_AUDIO_BYTES: usize = 640;

/// Most audio bytes one playback frame may carry.
pub const MAX_PLAYBACK_AUDIO_BYTES: usize = 64_000;

const KIND_HEARD: u8 = 0x01;
const KIND_PLAYBACK: u8 = 0x02;
const MAX_IDENTIFIER_BYTES: usize = 128;

/// One binary frame of a conversation connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioFrame {
    /// What a participant says. Controller to application.
    Heard {
        /// Who is speaking.
        participant: ParticipantId,
        /// Milliseconds of this participant's audio before the frame.
        position_ms: u64,
        /// Exactly twenty milliseconds of audio.
        audio: Vec<u8>,
    },
    /// Audio of a queued segment. Application to controller.
    Playback {
        /// Who is to hear it.
        participant: ParticipantId,
        /// The segment the `play` command named.
        segment: SegmentId,
        /// Whether this frame ends the segment.
        last: bool,
        /// A whole number of samples; may be empty only when `last`.
        audio: Vec<u8>,
    },
}

/// Why bytes are not an audio frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioFrameError {
    /// The first byte names no known kind of frame.
    UnknownKind(u8),
    /// The frame ends before its content does.
    Truncated,
    /// An identifier is empty, too long, or not valid.
    BadIdentifier,
    /// A heard frame does not carry exactly twenty milliseconds.
    WrongHeardLength(usize),
    /// A playback frame carries an odd number of bytes, too many, or none
    /// without being the last.
    WrongPlaybackLength(usize),
    /// The "last" byte is neither 0 nor 1.
    BadLastMarker(u8),
}

impl fmt::Display for AudioFrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownKind(kind) => write!(f, "unknown audio frame kind {kind:#04x}"),
            Self::Truncated => f.write_str("audio frame ends before its content does"),
            Self::BadIdentifier => {
                f.write_str("audio frame identifier is empty, too long, or not valid")
            }
            Self::WrongHeardLength(length) => write!(
                f,
                "heard frame carries {length} bytes of audio, not {HEARD_AUDIO_BYTES}"
            ),
            Self::WrongPlaybackLength(length) => write!(
                f,
                "playback frame carries {length} bytes of audio: it must be a whole number of \
                 samples, at most {MAX_PLAYBACK_AUDIO_BYTES} bytes, and empty only when last"
            ),
            Self::BadLastMarker(marker) => {
                write!(
                    f,
                    "playback frame's last marker is {marker:#04x}, not 0 or 1"
                )
            }
        }
    }
}

impl std::error::Error for AudioFrameError {}

impl AudioFrame {
    /// Decode one binary frame.
    ///
    /// # Errors
    ///
    /// Anything the format description refuses is an error; no frame is
    /// repaired or partly accepted.
    pub fn decode(bytes: &[u8]) -> Result<Self, AudioFrameError> {
        let (&kind, rest) = bytes.split_first().ok_or(AudioFrameError::Truncated)?;
        match kind {
            KIND_HEARD => {
                let (participant, rest) = take_identifier(rest)?;
                let (position, audio) = rest
                    .split_first_chunk::<8>()
                    .ok_or(AudioFrameError::Truncated)?;
                if audio.len() != HEARD_AUDIO_BYTES {
                    return Err(AudioFrameError::WrongHeardLength(audio.len()));
                }
                Ok(Self::Heard {
                    participant: participant
                        .parse()
                        .map_err(|_| AudioFrameError::BadIdentifier)?,
                    position_ms: u64::from_be_bytes(*position),
                    audio: audio.to_vec(),
                })
            }
            KIND_PLAYBACK => {
                let (participant, rest) = take_identifier(rest)?;
                let (segment, rest) = take_identifier(rest)?;
                let (&marker, audio) = rest.split_first().ok_or(AudioFrameError::Truncated)?;
                let last = match marker {
                    0 => false,
                    1 => true,
                    other => return Err(AudioFrameError::BadLastMarker(other)),
                };
                if !playback_length_is_valid(audio.len(), last) {
                    return Err(AudioFrameError::WrongPlaybackLength(audio.len()));
                }
                Ok(Self::Playback {
                    participant: participant
                        .parse()
                        .map_err(|_| AudioFrameError::BadIdentifier)?,
                    segment: segment
                        .parse()
                        .map_err(|_| AudioFrameError::BadIdentifier)?,
                    last,
                    audio: audio.to_vec(),
                })
            }
            other => Err(AudioFrameError::UnknownKind(other)),
        }
    }

    /// Encode the frame.
    ///
    /// # Errors
    ///
    /// A frame whose audio the format does not allow is refused here as it
    /// would be on arrival: what cannot be decoded is never sent.
    pub fn encode(&self) -> Result<Vec<u8>, AudioFrameError> {
        match self {
            Self::Heard {
                participant,
                position_ms,
                audio,
            } => {
                if audio.len() != HEARD_AUDIO_BYTES {
                    return Err(AudioFrameError::WrongHeardLength(audio.len()));
                }
                let mut bytes = vec![KIND_HEARD];
                put_identifier(&mut bytes, participant.as_str())?;
                bytes.extend_from_slice(&position_ms.to_be_bytes());
                bytes.extend_from_slice(audio);
                Ok(bytes)
            }
            Self::Playback {
                participant,
                segment,
                last,
                audio,
            } => {
                if !playback_length_is_valid(audio.len(), *last) {
                    return Err(AudioFrameError::WrongPlaybackLength(audio.len()));
                }
                let mut bytes = vec![KIND_PLAYBACK];
                put_identifier(&mut bytes, participant.as_str())?;
                put_identifier(&mut bytes, segment.as_str())?;
                bytes.push(u8::from(*last));
                bytes.extend_from_slice(audio);
                Ok(bytes)
            }
        }
    }
}

fn playback_length_is_valid(length: usize, last: bool) -> bool {
    length.is_multiple_of(2) && length <= MAX_PLAYBACK_AUDIO_BYTES && (length > 0 || last)
}

/// Split a length-prefixed identifier off the front of `bytes`.
fn take_identifier(bytes: &[u8]) -> Result<(&str, &[u8]), AudioFrameError> {
    let (&length, rest) = bytes.split_first().ok_or(AudioFrameError::Truncated)?;
    let length = usize::from(length);
    if length == 0 || length > MAX_IDENTIFIER_BYTES {
        return Err(AudioFrameError::BadIdentifier);
    }
    let (identifier, rest) = rest
        .split_at_checked(length)
        .ok_or(AudioFrameError::Truncated)?;
    let identifier = std::str::from_utf8(identifier).map_err(|_| AudioFrameError::BadIdentifier)?;
    Ok((identifier, rest))
}

fn put_identifier(bytes: &mut Vec<u8>, identifier: &str) -> Result<(), AudioFrameError> {
    let length = u8::try_from(identifier.len()).map_err(|_| AudioFrameError::BadIdentifier)?;
    if length == 0 || usize::from(length) > MAX_IDENTIFIER_BYTES {
        return Err(AudioFrameError::BadIdentifier);
    }
    bytes.push(length);
    bytes.extend_from_slice(identifier.as_bytes());
    Ok(())
}
