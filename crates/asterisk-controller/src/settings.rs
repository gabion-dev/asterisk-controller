// crates/asterisk-controller/src/settings.rs

//! The settings of the node: what the application told it in advance.
//!
//! Operators, outbound lines, entries with their fallbacks, prompts — the
//! `Settings` of the protocol description. The node keeps the last settings
//! it applied in a file of its state directory and starts on them: a node
//! restarted while the application is away still knows what it serves.
//!
//! Settings are judged twice before anything is made of them. The protocol
//! description judges their form. This module judges what the description
//! cannot say: that what one part names exists in another, and that every
//! value can be written into Asterisk's configuration as itself. Settings
//! that fail either are refused whole, with the reason — the node never
//! applies a part of them.

use std::{collections::HashSet, fs, io::ErrorKind, net::IpAddr, path::Path};

use node_protocol::messages::{EntryKey, Fallback, Operator, Settings, SipTransport};

use crate::{destination, state_files};

/// The file of the state directory that holds the settings last applied.
pub const FILE: &str = "settings.json";

/// The settings a node runs on.
pub struct Applied {
    /// The settings themselves; empty when the node has none yet.
    pub settings: Settings,
    /// The fingerprint of the settings — the SHA-256 of their canonical
    /// text, which does not depend on how the file is written: what the
    /// application compares with what it expects. `None` when the node has
    /// no settings yet.
    pub fingerprint: Option<String>,
}

/// Read the settings stored in the state directory.
///
/// A node without the file has no settings yet: it serves nothing, and says
/// so. That is a state, not an error.
///
/// # Errors
///
/// A file that cannot be read, is not what the protocol describes, or fails
/// [`check`] is an error with the reason: a node does not start on settings
/// it would have refused. Nor on a file others than its owner may read: it
/// carries the operators' passwords, and the controller writes it readable
/// by its owner alone — whoever puts it there instead does the same.
pub fn stored(state: &Path) -> Result<Applied, String> {
    let path = state.join(FILE);
    let bytes = match fs::metadata(&path).and_then(|metadata| {
        if state_files::open_to_others(&metadata) {
            Err(std::io::Error::other(
                "others than its owner may read it, and it carries the operators' passwords; \
                 make it readable by its owner alone",
            ))
        } else {
            fs::read(&path)
        }
    }) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(Applied {
                settings: Settings {
                    entries: Vec::new(),
                    lines: Vec::new(),
                    operators: Vec::new(),
                    prompts: Vec::new(),
                },
                fingerprint: None,
            });
        }
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|error| format!("{}: not text: {error}", path.display()))?;
    let settings: Settings =
        node_protocol::decode(text).map_err(|error| format!("{}: {error}", path.display()))?;
    check(&settings).map_err(|problem| format!("{}: {problem}", path.display()))?;
    let fingerprint = node_protocol::fingerprint(&settings)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(Applied {
        settings,
        fingerprint: Some(fingerprint),
    })
}

/// Keep settings as the ones the node runs on: their canonical text, so the
/// file says exactly what their fingerprint is of, written whole or not at
/// all.
///
/// # Errors
///
/// The settings are not what the description allows, or the file cannot
/// be written.
pub fn store(state: &Path, settings: &Settings) -> Result<(), String> {
    let path = state.join(FILE);
    let failed = |problem: &dyn std::fmt::Display| format!("{}: {problem}", path.display());
    // Encoding holds the settings to the description.
    node_protocol::encode(settings).map_err(|error| failed(&error))?;
    let value = serde_json::to_value(settings).map_err(|error| failed(&error))?;
    state_files::replace(&path, node_protocol::canonical(&value).as_bytes())
        .map_err(|error| failed(&error))
}

/// Judge what the protocol description cannot.
///
/// # Errors
///
/// The first problem found, in words the application can show to whoever
/// entered the settings.
pub fn check(settings: &Settings) -> Result<(), String> {
    let mut operators = HashSet::new();
    for operator in &settings.operators {
        let id = operator.id.as_str();
        if !operators.insert(id) {
            return Err(format!("operator {id:?} is listed twice"));
        }
        check_operator(operator).map_err(|problem| format!("operator {id:?}: {problem}"))?;
    }

    let mut lines = HashSet::new();
    for line in &settings.lines {
        let id = line.id.as_str();
        if !lines.insert(id) {
            return Err(format!("line {id:?} is listed twice"));
        }
        if !operators.contains(line.operator.as_str()) {
            return Err(format!(
                "line {id:?} goes through operator {:?}, which is not among the operators",
                line.operator.as_str()
            ));
        }
    }

    let mut prompts = HashSet::new();
    for prompt in &settings.prompts {
        if !prompts.insert(prompt.id.as_str()) {
            return Err(format!("prompt {:?} is listed twice", prompt.id.as_str()));
        }
    }

    let mut entries = HashSet::new();
    for entry in &settings.entries {
        let name = match &entry.key {
            EntryKey::DialedNumber(number) => format!("the entry of number {}", number.as_str()),
            EntryKey::UserEndpoints => "the entry of user endpoints".to_owned(),
            EntryKey::WebPasses => "the entry of web passes".to_owned(),
        };
        if !entries.insert(name.clone()) {
            return Err(format!("{name} is listed twice"));
        }
        match &entry.fallback {
            Fallback::Transfer { line, number, .. } => {
                if !lines.contains(line.as_str()) {
                    return Err(format!(
                        "{name} falls back to a transfer on line {:?}, which is not among the lines",
                        line.as_str()
                    ));
                }
                // A fallback's transfer is an outbound call like any other,
                // and one the node makes with nobody to ask.
                if let Err(reason) = destination::judge(settings, line, number) {
                    return Err(format!(
                        "{name} falls back to a transfer to {}, which line {:?} may not dial: \
                         {reason}",
                        number.as_str(),
                        line.as_str()
                    ));
                }
            }
            Fallback::Message { prompt } => {
                if !prompts.contains(prompt.as_str()) {
                    return Err(format!(
                        "{name} falls back to prompt {:?}, which is not among the prompts",
                        prompt.as_str()
                    ));
                }
            }
        }
    }
    Ok(())
}

fn check_operator(operator: &Operator) -> Result<(), String> {
    if !is_host(operator.host.as_str()) {
        return Err(format!(
            "host {:?} is neither a host name nor an address",
            operator.host.as_str()
        ));
    }
    if u16::try_from(operator.port.get()).is_err() {
        return Err(format!("port {} does not exist", operator.port));
    }
    if operator.transport == SipTransport::Tls {
        return Err(
            "transport tls needs a certificate of the node, and settings cannot carry one yet"
                .into(),
        );
    }
    for network in &operator.source_networks {
        if !is_network(network.as_str()) {
            return Err(format!(
                "source network {:?} is not an address range",
                network.as_str()
            ));
        }
    }
    if let Some(credentials) = &operator.credentials {
        writable(credentials.username.as_str()).map_err(|problem| format!("username {problem}"))?;
        writable(credentials.password.as_str()).map_err(|problem| format!("password {problem}"))?;
    }
    Ok(())
}

/// Whether text is a host name or an IP address — and so nothing else: it
/// goes into an address Asterisk dials.
fn is_host(text: &str) -> bool {
    text.parse::<IpAddr>().is_ok()
        || text.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

/// Whether text is an address range: an address, a slash, a prefix length
/// that address family has.
fn is_network(text: &str) -> bool {
    let Some((address, length)) = text.split_once('/') else {
        return false;
    };
    let (Ok(address), Ok(length)) = (address.parse::<IpAddr>(), length.parse::<u8>()) else {
        return false;
    };
    length
        <= match address {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        }
}

/// Whether a free-form value can be written into Asterisk's configuration
/// and be read back as itself.
///
/// A value there is the rest of a line. So it cannot span lines, and
/// Asterisk drops the spaces around it. A semicolon is written escaped and
/// a leading `>` is kept by writing the value after `= `
/// ([`crate::asterisk_files`]); neither is an obstacle.
fn writable(value: &str) -> Result<(), String> {
    if value.chars().any(char::is_control) {
        Err("contains a control character".into())
    } else if value.trim() != value {
        Err("begins or ends with a space, which Asterisk's configuration would drop".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::check;
    use node_protocol::messages::Settings;
    use serde_json::{Value, json};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn settings() -> Value {
        json!({
            "operators": [{
                "id": "chime", "host": "abc.voiceconnector.chime.aws", "port": 5060,
                "transport": "udp",
                "credentials": { "username": "node", "password": "a;b c" },
                "source_networks": ["3.80.16.0/23", "2001:db8::/32"],
                "countries": ["US"]
            }],
            "lines": [{
                "id": "main", "number": "+19715870050", "operator": "chime",
                "allowed_countries": ["US"], "concurrent_outbound_limit": 2
            }],
            "entries": [
                { "key": { "type": "dialed_number", "number": "+19715870050" },
                  "fallback": { "type": "transfer", "number": "+15035550100",
                                "line": "main", "answer_limit_ms": 20000 } },
                { "key": { "type": "web_passes" },
                  "fallback": { "type": "message", "prompt": "closed" } }
            ],
            "prompts": [{ "id": "closed", "sha256": "0".repeat(64) }]
        })
    }

    /// The problem `check` finds after one value of the settings is replaced.
    fn problem_after(pointer: &str, value: Value) -> Result<String, Box<dyn std::error::Error>> {
        let mut changed = settings();
        *changed
            .pointer_mut(pointer)
            .ok_or_else(|| format!("no {pointer} in the settings"))? = value;
        let changed: Settings = node_protocol::decode_value(changed)?;
        Ok(check(&changed).err().unwrap_or_default())
    }

    #[test]
    fn settings_whose_parts_agree_pass() -> TestResult {
        let settings: Settings = node_protocol::decode_value(settings())?;
        assert_eq!(check(&settings), Ok(()));
        Ok(())
    }

    #[test]
    fn what_one_part_names_must_exist_in_another() -> TestResult {
        for (pointer, value, expected) in [
            (
                "/lines/0/operator",
                json!("other"),
                "not among the operators",
            ),
            (
                "/entries/0/fallback/line",
                json!("other"),
                "not among the lines",
            ),
            (
                "/entries/1/fallback/prompt",
                json!("other"),
                "not among the prompts",
            ),
            // A fallback's transfer is judged like any call the node places.
            (
                "/entries/0/fallback/number",
                json!("+442071838750"),
                "may not dial",
            ),
            (
                "/entries/0/fallback/number",
                json!("+19002345678"),
                "may not dial",
            ),
        ] {
            let problem = problem_after(pointer, value)?;
            assert!(problem.contains(expected), "{pointer}: {problem:?}");
        }
        Ok(())
    }

    #[test]
    fn nothing_is_listed_twice() -> TestResult {
        for part in ["operators", "lines", "prompts"] {
            let mut doubled = settings();
            let list = doubled
                .get_mut(part)
                .and_then(Value::as_array_mut)
                .ok_or("no such list")?;
            let first = list.first().cloned().ok_or("an empty list")?;
            list.push(first);
            let doubled: Settings = node_protocol::decode_value(doubled)?;
            let problem = check(&doubled).err().unwrap_or_default();
            assert!(problem.contains("listed twice"), "{part}: {problem:?}");
        }
        Ok(())
    }

    #[test]
    fn a_value_asterisk_would_read_as_something_else_is_refused() -> TestResult {
        for (pointer, value, expected) in [
            (
                "/operators/0/host",
                json!("host name"),
                "neither a host name nor an address",
            ),
            (
                "/operators/0/host",
                json!("a.b]\n[x"),
                "neither a host name nor an address",
            ),
            ("/operators/0/port", json!(70000), "does not exist"),
            ("/operators/0/transport", json!("tls"), "certificate"),
            (
                "/operators/0/source_networks/0",
                json!("3.80.16.0/40"),
                "not an address range",
            ),
            (
                "/operators/0/source_networks/0",
                json!("1.2.3/8"),
                "not an address range",
            ),
            (
                "/operators/0/credentials/password",
                json!("a\nb"),
                "control character",
            ),
            (
                "/operators/0/credentials/password",
                json!("ab "),
                "begins or ends with a space",
            ),
        ] {
            let problem = problem_after(pointer, value)?;
            assert!(problem.contains(expected), "{pointer}: {problem:?}");
        }
        Ok(())
    }
}
