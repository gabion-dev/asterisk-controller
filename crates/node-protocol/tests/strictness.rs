// crates/node-protocol/tests/strictness.rs
//! The protocol types refuse what the protocol description does not allow.
//!
//! These tests pin the property the rest of the controller relies on: a
//! message that is not exactly what the description says never becomes a
//! value. Nothing is skipped and nothing is defaulted.

use node_protocol::messages::{
    ApplicationMessage, ApplicationServiceMessage, Command, ControllerMessage, Event, NodeMessage,
    Opening, Origin, Request, Settings,
};
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn hello() -> Value {
    json!({
        "type": "hello",
        "protocol": 1,
        "node": "node-1",
        "controller_version": "0.1.0",
        "conversation": "c-1",
        "opening": {
            "type": "started",
            "origin": { "type": "dialed_number", "dialed": "+19715870050" },
            "first": { "id": "p-1", "medium": "telephone_network", "number": "+15035550100" }
        }
    })
}

#[test]
fn a_well_formed_hello_is_accepted() -> TestResult {
    let message: ControllerMessage = serde_json::from_value(hello())?;
    let ControllerMessage::Hello {
        opening, protocol, ..
    } = message
    else {
        return Err("the hello did not decode as a hello".into());
    };
    assert_eq!(protocol.get(), 1);
    let Opening::Started { origin, first } = opening else {
        return Err("the opening did not decode as a new conversation".into());
    };
    assert!(matches!(origin, Origin::DialedNumber { .. }));
    assert_eq!(first.id.as_str(), "p-1");
    Ok(())
}

/// The object at `pointer` inside `value`, for a test to add or remove a field.
fn object_at<'a>(
    value: &'a mut Value,
    pointer: &str,
) -> Result<&'a mut serde_json::Map<String, Value>, Box<dyn std::error::Error>> {
    value
        .pointer_mut(pointer)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| format!("the fixture has no object at {pointer:?}").into())
}

#[test]
fn a_field_the_description_does_not_name_is_refused() -> TestResult {
    let mut nested = hello();
    object_at(&mut nested, "/opening/first")?.insert("nickname".into(), json!("x"));
    assert!(serde_json::from_value::<ControllerMessage>(nested).is_err());

    let mut top_level = hello();
    object_at(&mut top_level, "")?.insert("extra".into(), json!(true));
    assert!(serde_json::from_value::<ControllerMessage>(top_level).is_err());
    Ok(())
}

#[test]
fn a_missing_required_field_is_refused_not_defaulted() -> TestResult {
    let mut message = hello();
    object_at(&mut message, "")?.remove("protocol");
    assert!(serde_json::from_value::<ControllerMessage>(message).is_err());

    let dial_without_limit = json!({
        "type": "command", "id": 7,
        "command": { "type": "dial", "number": "+15035550100", "line": "main" }
    });
    assert!(serde_json::from_value::<ApplicationMessage>(dial_without_limit).is_err());
    Ok(())
}

#[test]
fn an_unknown_kind_is_refused_at_every_level() {
    let unknown_message = json!({ "type": "greetings" });
    assert!(serde_json::from_value::<ControllerMessage>(unknown_message.clone()).is_err());
    assert!(serde_json::from_value::<ApplicationMessage>(unknown_message).is_err());

    let unknown_command = json!({ "type": "transfer", "participant": "p-1" });
    assert!(serde_json::from_value::<Command>(unknown_command).is_err());

    let unknown_event = json!({ "type": "voicemail_detected", "participant": "p-1" });
    assert!(serde_json::from_value::<Event>(unknown_event).is_err());

    let unknown_departure = json!({
        "type": "participant_left", "participant": "p-1",
        "departure": { "type": "voicemail" }
    });
    assert!(serde_json::from_value::<Event>(unknown_departure).is_err());
}

#[test]
fn a_value_of_the_wrong_form_is_refused() {
    for number in [
        "5035550100",
        "+0123456789",
        "+1503555010012345678",
        "+1 503 555 0100",
    ] {
        let dial =
            json!({ "type": "dial", "number": number, "line": "main", "answer_limit_ms": 20000 });
        assert!(
            serde_json::from_value::<Command>(dial).is_err(),
            "{number} was accepted"
        );
    }

    let empty_identifier = json!({ "type": "answer", "participant": "" });
    assert!(serde_json::from_value::<Command>(empty_identifier).is_err());

    let two_digits = json!({ "type": "digit_received", "participant": "p-1", "digit": "12" });
    assert!(serde_json::from_value::<Event>(two_digits).is_err());

    let too_long = json!({ "type": "answer", "participant": "p".repeat(129) });
    assert!(serde_json::from_value::<Command>(too_long).is_err());

    let negative_limit = json!({ "type": "hold_for", "participant": "p-1", "limit_ms": -1 });
    assert!(serde_json::from_value::<Command>(negative_limit).is_err());
}

#[test]
fn what_is_written_reads_back_the_same() -> TestResult {
    let original = json!({
        "type": "command", "id": 3,
        "command": { "type": "dial", "number": "+15035550100", "line": "main", "answer_limit_ms": 20000 }
    });
    let message: ApplicationMessage = serde_json::from_value(original.clone())?;
    assert_eq!(serde_json::to_value(&message)?, original);

    let event = json!({
        "type": "event",
        "event": { "type": "participant_left", "participant": "p-2",
                   "departure": { "type": "dial_failed", "medium_code": 486 } }
    });
    let message: ControllerMessage = serde_json::from_value(event.clone())?;
    assert_eq!(serde_json::to_value(&message)?, event);
    Ok(())
}

fn settings() -> Value {
    json!({
        "operators": [{
            "id": "chime", "host": "abc.voiceconnector.chime.aws", "port": 5060, "transport": "udp",
            "source_networks": ["3.80.16.0/23"], "countries": ["US", "CA"]
        }],
        "lines": [{
            "id": "main", "number": "+19715870050", "operator": "chime",
            "allowed_countries": ["US"], "concurrent_outbound_limit": 2
        }],
        "entries": [{
            "key": { "type": "dialed_number", "number": "+19715870050" },
            "fallback": { "type": "message", "prompt": "away" }
        }],
        "prompts": [{
            "id": "away",
            "sha256": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        }]
    })
}

#[test]
fn well_formed_settings_are_accepted() -> TestResult {
    let parsed: Settings = serde_json::from_value(settings())?;
    assert_eq!(parsed.operators.len(), 1);
    assert_eq!(parsed.entries.len(), 1);
    Ok(())
}

#[test]
fn an_entry_cannot_be_given_without_a_fallback() -> TestResult {
    let mut without_fallback = settings();
    object_at(&mut without_fallback, "/entries/0")?.remove("fallback");
    assert!(serde_json::from_value::<Settings>(without_fallback).is_err());

    let mut unknown_fallback = settings();
    object_at(&mut unknown_fallback, "/entries/0")?
        .insert("fallback".into(), json!({ "type": "queue" }));
    assert!(serde_json::from_value::<Settings>(unknown_fallback).is_err());
    Ok(())
}

#[test]
fn settings_values_of_the_wrong_form_are_refused() -> TestResult {
    for (pointer, field, value) in [
        ("/operators/0", "countries", json!(["usa"])),
        ("/operators/0", "source_networks", json!(["everywhere"])),
        ("/operators/0", "transport", json!("sctp")),
        ("/operators/0", "port", json!(0)),
        ("/lines/0", "number", json!("9715870050")),
        ("/prompts/0", "sha256", json!("9F86")),
    ] {
        let mut changed = settings();
        object_at(&mut changed, pointer)?.insert(field.into(), value);
        assert!(
            serde_json::from_value::<Settings>(changed).is_err(),
            "{pointer}/{field} was accepted"
        );
    }
    Ok(())
}

#[test]
fn the_service_connection_is_as_strict_as_the_conversation() -> TestResult {
    let hello_without_settings = json!({
        "type": "hello", "protocol": 1, "node": "node-1",
        "controller_version": "0.1.0", "asterisk_version": "22.11.0"
    });
    let message: NodeMessage = serde_json::from_value(hello_without_settings)?;
    let NodeMessage::Hello {
        applied_settings, ..
    } = message
    else {
        return Err("the hello did not decode as a hello".into());
    };
    assert!(applied_settings.is_none());

    let browser_leg = json!({
        "type": "request", "id": 1,
        "request": {
            "type": "browser_leg_begin", "leg": "leg-1",
            "origin": { "type": "web_pass", "pass": "pass-1" },
            "offer": "v=0"
        }
    });
    let message: ApplicationServiceMessage = serde_json::from_value(browser_leg)?;
    assert!(matches!(
        message,
        ApplicationServiceMessage::Request {
            request: Request::BrowserLegBegin { .. },
            ..
        }
    ));

    // A browser leg has no destination to give: the description has no such field.
    let leg_with_destination = json!({
        "type": "browser_leg_begin", "leg": "leg-1",
        "origin": { "type": "web_pass", "pass": "pass-1" },
        "offer": "v=0", "number": "+15035550100"
    });
    assert!(serde_json::from_value::<Request>(leg_with_destination).is_err());

    // Nor can it claim to be a call from the telephone network.
    let leg_as_dialed_number = json!({
        "type": "browser_leg_begin", "leg": "leg-1",
        "origin": { "type": "dialed_number", "dialed": "+19715870050" },
        "offer": "v=0"
    });
    assert!(serde_json::from_value::<Request>(leg_as_dialed_number).is_err());

    let unknown_request = json!({ "type": "originate", "number": "+15035550100" });
    assert!(serde_json::from_value::<Request>(unknown_request).is_err());
    Ok(())
}
