// crates/node-protocol/tests/message_vectors.rs

//! The generated message types against the shared vectors.
//!
//! `protocol/messages.vectors.json` is read by this test and by the check
//! of the Java types generated from the same description. What one side
//! accepts and the other refuses would be a protocol the two only think
//! they share; the vectors make that impossible to miss.

use node_protocol::{
    DecodeError, Described, decode_value,
    messages::{
        ApplicationMessage, ApplicationServiceMessage, Command, ControllerMessage, Event,
        NodeMessage, Request, Settings,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    #[serde(rename = "type")]
    type_name: String,
    accept: bool,
    json: Value,
}

/// Decode `json` as `T` and encode it back; `None` when it is refused.
fn round_trip<T: Described + Serialize>(
    json: &Value,
) -> Result<Option<Value>, Box<dyn std::error::Error>> {
    match decode_value::<T>(json.clone()) {
        Ok(value) => Ok(Some(serde_json::to_value(&value)?)),
        Err(DecodeError::Violation { .. }) => Ok(None),
        // A fault of the crate itself must fail the test, not count as a refusal.
        Err(fault) => Err(fault.into()),
    }
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "reads the vector file of this repository, not a message"
)]
fn every_shared_vector_is_accepted_or_refused_as_stated() -> TestResult {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../protocol/messages.vectors.json"
    ))?;
    let vectors: Vectors = serde_json::from_str(&text)?;
    assert!(
        vectors.vectors.len() >= 60,
        "the vector file lost its vectors"
    );

    let mut problems = Vec::new();
    for vector in vectors.vectors {
        let json = &vector.json;
        let outcome = match vector.type_name.as_str() {
            "ControllerMessage" => round_trip::<ControllerMessage>(json)?,
            "ApplicationMessage" => round_trip::<ApplicationMessage>(json)?,
            "NodeMessage" => round_trip::<NodeMessage>(json)?,
            "ApplicationServiceMessage" => round_trip::<ApplicationServiceMessage>(json)?,
            "Command" => round_trip::<Command>(json)?,
            "Event" => round_trip::<Event>(json)?,
            "Request" => round_trip::<Request>(json)?,
            "Settings" => round_trip::<Settings>(json)?,
            other => return Err(format!("{}: unknown type {other}", vector.name).into()),
        };
        match (vector.accept, outcome) {
            (true, Some(encoded)) if encoded == *json => {}
            (true, Some(encoded)) => problems.push(format!(
                "{}: accepted, but encodes back as {encoded} instead of {json}",
                vector.name
            )),
            (true, None) => problems.push(format!("{}: refused but must be accepted", vector.name)),
            (false, Some(_)) => {
                problems.push(format!("{}: accepted but must be refused", vector.name));
            }
            (false, None) => {}
        }
    }
    assert!(problems.is_empty(), "\n{}\n", problems.join("\n"));
    Ok(())
}
