// crates/node-protocol/src/canonical.rs

//! The canonical text of a value and the fingerprint of settings.
//!
//! The application and the node compare settings by their fingerprint, so
//! the fingerprint must depend on what the settings say and on nothing else
//! — not on the library that encoded them or the order it put their fields
//! in. `protocol/settings-fingerprint.md` defines the canonical text;
//! `protocol/settings-fingerprint.vectors.json` holds what every
//! implementation must reproduce.
//!
//! The text is written here rule by rule rather than taken from the JSON
//! library's own output: that output orders object members only as long as
//! no crate in the build switches the library to keeping insertion order,
//! and nothing would say so when one did.

use std::fmt::Write as _;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{DecodeError, encode, messages::Settings};

/// The canonical text of a JSON value.
#[must_use]
pub fn canonical(value: &Value) -> String {
    let mut text = String::new();
    write_value(&mut text, value);
    text
}

/// The fingerprint of settings: the SHA-256 of their canonical text, in
/// lowercase hexadecimal.
///
/// # Errors
///
/// Settings the protocol description does not allow have no fingerprint:
/// they are refused as they would be on arrival.
pub fn fingerprint(settings: &Settings) -> Result<String, DecodeError> {
    // Encoding holds the settings to the description; its text is not used.
    encode(settings)?;
    let value = serde_json::to_value(settings).map_err(DecodeError::TypeDisagrees)?;
    Ok(sha256_hex(canonical(&value).as_bytes()))
}

/// SHA-256 of `bytes` in lowercase hexadecimal.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        // Writing into a `String` cannot fail.
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn write_value(text: &mut String, value: &Value) {
    match value {
        Value::Null => text.push_str("null"),
        Value::Bool(true) => text.push_str("true"),
        Value::Bool(false) => text.push_str("false"),
        // The protocol has integers only; the JSON library writes them in
        // plain decimal.
        Value::Number(number) => text.push_str(&number.to_string()),
        Value::String(string) => write_string(text, string),
        Value::Array(items) => {
            text.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    text.push(',');
                }
                write_value(text, item);
            }
            text.push(']');
        }
        Value::Object(members) => {
            let mut ordered: Vec<_> = members.iter().collect();
            ordered.sort_by(|(left, _), (right, _)| left.encode_utf16().cmp(right.encode_utf16()));
            text.push('{');
            for (index, (name, member)) in ordered.into_iter().enumerate() {
                if index > 0 {
                    text.push(',');
                }
                write_string(text, name);
                text.push(':');
                write_value(text, member);
            }
            text.push('}');
        }
    }
}

fn write_string(text: &mut String, string: &str) {
    text.push('"');
    for character in string.chars() {
        match character {
            '"' => text.push_str("\\\""),
            '\\' => text.push_str("\\\\"),
            '\u{8}' => text.push_str("\\b"),
            '\t' => text.push_str("\\t"),
            '\n' => text.push_str("\\n"),
            '\u{c}' => text.push_str("\\f"),
            '\r' => text.push_str("\\r"),
            '\u{0}'..='\u{1f}' => {
                // Writing into a `String` cannot fail.
                let _ = write!(text, "\\u{:04x}", u32::from(character));
            }
            other => text.push(other),
        }
    }
    text.push('"');
}
