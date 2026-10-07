// crates/node-protocol/tests/fingerprint_vectors.rs

//! The canonical text and the fingerprint against the shared vectors.
//!
//! `protocol/settings-fingerprint.vectors.json` is read by this test and by
//! the check of the Java library. Settings the two sides fingerprint
//! differently would look to the application like a node that runs on other
//! settings than it was given.

use node_protocol::{canonical, decode_value, fingerprint, messages::Settings, sha256_hex};
use serde::Deserialize;
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
    value: Value,
    canonical: String,
    sha256: String,
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "reads the vector file of this repository, not a message"
)]
fn every_value_has_the_canonical_text_and_digest_stated() -> TestResult {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../protocol/settings-fingerprint.vectors.json"
    ))?;
    let vectors: Vectors = serde_json::from_str(&text)?;
    assert!(
        vectors.vectors.len() >= 5,
        "the vector file lost its vectors"
    );

    let mut problems = Vec::new();
    let mut settings_seen = 0;
    for vector in vectors.vectors {
        let written = canonical(&vector.value);
        if written != vector.canonical {
            problems.push(format!(
                "{}: canonical text is\n  {written}\ninstead of\n  {}",
                vector.name, vector.canonical
            ));
        }
        if sha256_hex(vector.canonical.as_bytes()) != vector.sha256 {
            problems.push(format!("{}: the digest of the text differs", vector.name));
        }
        // A value that is settings must also have that digest as its
        // fingerprint, through the protocol's own types.
        if let Ok(settings) = decode_value::<Settings>(vector.value.clone()) {
            settings_seen += 1;
            if fingerprint(&settings)? != vector.sha256 {
                problems.push(format!("{}: the fingerprint differs", vector.name));
            }
        }
    }
    assert!(settings_seen >= 2, "the vectors lost their settings");
    assert!(problems.is_empty(), "\n{}\n", problems.join("\n"));
    Ok(())
}
