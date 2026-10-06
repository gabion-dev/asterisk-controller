// crates/node-protocol/tests/audio_vectors.rs

//! The audio frame implementation against the shared vectors.
//!
//! `protocol/audio-frames.vectors.json` is read by this test and by the
//! application's implementation of the same format. A frame decoded here
//! one way and there another would be a protocol the two sides only think
//! they share.

use std::fmt::Write as _;

use node_protocol::audio::{AudioFrame, AudioFrameError};
use serde::Deserialize;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Vectors {
    #[serde(rename = "description")]
    _description: String,
    vectors: Vec<Vector>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Vector {
    name: String,
    frame_hex: String,
    expect: Option<Expected>,
    refused: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Expected {
    Heard {
        participant: String,
        position_ms: u64,
        audio_hex: String,
    },
    Playback {
        participant: String,
        segment: String,
        last: bool,
        audio_hex: String,
    },
}

fn from_hex(hex: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    if !hex.len().is_multiple_of(2) {
        return Err("odd number of hex digits".into());
    }
    (0..hex.len())
        .step_by(2)
        .map(|at| {
            let pair = hex.get(at..at + 2).ok_or("hex digits are not ASCII")?;
            Ok(u8::from_str_radix(pair, 16)?)
        })
        .collect()
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut hex, byte| {
        // Writing to a String cannot fail.
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

fn name_of(error: AudioFrameError) -> &'static str {
    match error {
        AudioFrameError::UnknownKind(_) => "unknown_kind",
        AudioFrameError::Truncated => "truncated",
        AudioFrameError::BadIdentifier => "bad_identifier",
        AudioFrameError::WrongHeardLength(_) => "wrong_heard_length",
        AudioFrameError::WrongPlaybackLength(_) => "wrong_playback_length",
        AudioFrameError::BadLastMarker(_) => "bad_last_marker",
    }
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "reads the vector file of this repository, not a message"
)]
fn every_shared_vector_is_decoded_or_refused_as_stated() -> TestResult {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../protocol/audio-frames.vectors.json"
    ))?;
    let vectors: Vectors = serde_json::from_str(&text)?;
    assert!(
        vectors.vectors.len() >= 20,
        "the vector file lost its vectors"
    );

    for vector in vectors.vectors {
        let name = &vector.name;
        let frame = from_hex(&vector.frame_hex)?;
        let decoded = AudioFrame::decode(&frame);

        match (vector.expect, vector.refused, decoded) {
            (Some(expected), None, Ok(actual)) => {
                match (expected, &actual) {
                    (
                        Expected::Heard {
                            participant,
                            position_ms,
                            audio_hex,
                        },
                        AudioFrame::Heard {
                            participant: got,
                            position_ms: got_position,
                            audio,
                        },
                    ) => {
                        assert_eq!(got.as_str(), participant, "{name}");
                        assert_eq!(*got_position, position_ms, "{name}");
                        assert_eq!(to_hex(audio), audio_hex, "{name}");
                    }
                    (
                        Expected::Playback {
                            participant,
                            segment,
                            last,
                            audio_hex,
                        },
                        AudioFrame::Playback {
                            participant: got,
                            segment: got_segment,
                            last: got_last,
                            audio,
                        },
                    ) => {
                        assert_eq!(got.as_str(), participant, "{name}");
                        assert_eq!(got_segment.as_str(), segment, "{name}");
                        assert_eq!(*got_last, last, "{name}");
                        assert_eq!(to_hex(audio), audio_hex, "{name}");
                    }
                    (Expected::Heard { .. }, AudioFrame::Playback { .. })
                    | (Expected::Playback { .. }, AudioFrame::Heard { .. }) => {
                        return Err(format!("{name}: decoded as the other kind of frame").into());
                    }
                }
                assert_eq!(
                    actual.encode()?,
                    frame,
                    "{name}: does not encode back to itself"
                );
            }
            (None, Some(reason), Err(error)) => {
                assert_eq!(name_of(error), reason, "{name}");
            }
            (Some(_), None, Err(error)) => {
                return Err(format!("{name}: refused ({error}) but must be accepted").into());
            }
            (None, Some(reason), Ok(_)) => {
                return Err(format!("{name}: accepted but must be refused as {reason}").into());
            }
            (Some(_), Some(_), _) | (None, None, _) => {
                return Err(
                    format!("{name}: a vector states exactly one of expect and refused").into(),
                );
            }
        }
    }
    Ok(())
}
