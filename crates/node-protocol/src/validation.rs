// crates/node-protocol/src/validation.rs

//! Decoding of protocol messages: the description judges, the types carry.
//!
//! A message is first checked against the protocol description itself and
//! only then turned into its generated type. The generated types alone are
//! not the judge: derived decoding lets through things the description
//! forbids — an extra field on a message that has none, `null` for a field
//! that may only be absent. The shared vectors found both. With the
//! description as the judge, what it says is what is accepted, on its word
//! and not on a generator's reading of it.
//!
//! The checker understands exactly the keywords the build allows the
//! description to use (`build.rs`, `ENFORCED_KEYWORDS`) and refuses any
//! other, so it cannot silently skip a rule either.

use std::{collections::HashMap, fmt, sync::OnceLock};

use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::messages::{
    ApplicationMessage, ApplicationServiceMessage, Command, ControllerMessage, Event, NodeMessage,
    Request, Settings,
};

const DESCRIPTION_TEXT: &str = include_str!("../../../protocol/node-protocol.schema.json");

/// A type that travels as a JSON text and has a definition of its own in
/// the protocol description.
pub trait Described: DeserializeOwned {
    /// Name of the type's definition in the description.
    const DEFINITION: &'static str;
}

macro_rules! described {
    ($($type:ident),+ $(,)?) => {
        $(impl Described for $type {
            const DEFINITION: &'static str = stringify!($type);
        })+
    };
}

described!(
    ApplicationMessage,
    ApplicationServiceMessage,
    Command,
    ControllerMessage,
    Event,
    NodeMessage,
    Request,
    Settings,
);

/// Why a text is not a message of the protocol.
#[derive(Debug)]
pub enum DecodeError {
    /// The text is not JSON.
    NotJson(serde_json::Error),
    /// The value breaks the description.
    Violation {
        /// Where in the value, as a JSON pointer; empty for the value itself.
        at: String,
        /// What is wrong there.
        problem: String,
    },
    /// The value satisfies the description but the generated type refuses
    /// it: the description and the types disagree, which is a defect of
    /// this crate, not of the sender.
    TypeDisagrees(serde_json::Error),
    /// The description embedded in this build cannot be used.
    Description(String),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotJson(error) => write!(f, "not JSON: {error}"),
            Self::Violation { at, problem } => {
                let place = if at.is_empty() { "the message" } else { at };
                write!(f, "{place}: {problem}")
            }
            Self::TypeDisagrees(error) => write!(
                f,
                "the message satisfies the protocol description but its type refuses it: {error}"
            ),
            Self::Description(problem) => write!(f, "protocol description unusable: {problem}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Decode a protocol message from its JSON text.
///
/// # Errors
///
/// Anything the description does not allow is an error. Nothing is skipped
/// and nothing is defaulted.
#[expect(
    clippy::disallowed_methods,
    reason = "this is the checked decoding itself: the text becomes a plain value, which is then judged"
)]
pub fn decode<T: Described>(text: &str) -> Result<T, DecodeError> {
    let value: Value = serde_json::from_str(text).map_err(DecodeError::NotJson)?;
    decode_value(value)
}

/// Decode a protocol message from a JSON value.
///
/// # Errors
///
/// See [`decode`].
#[expect(
    clippy::disallowed_methods,
    reason = "this is the checked decoding itself: the value has just passed the description"
)]
pub fn decode_value<T: Described>(value: Value) -> Result<T, DecodeError> {
    let description = description()?;
    let schema = description.definition(T::DEFINITION)?;
    description.check(schema, &value, "")?;
    serde_json::from_value(value).map_err(DecodeError::TypeDisagrees)
}

struct Description {
    definitions: Map<String, Value>,
    patterns: HashMap<String, regress::Regex>,
}

fn description() -> Result<&'static Description, DecodeError> {
    static DESCRIPTION: OnceLock<Result<Description, String>> = OnceLock::new();
    DESCRIPTION
        .get_or_init(Description::load)
        .as_ref()
        .map_err(|problem| DecodeError::Description(problem.clone()))
}

fn violation(at: &str, problem: impl Into<String>) -> DecodeError {
    DecodeError::Violation {
        at: at.to_owned(),
        problem: problem.into(),
    }
}

impl Description {
    #[expect(
        clippy::disallowed_methods,
        reason = "reads the protocol description embedded in this build, not a message"
    )]
    fn load() -> Result<Self, String> {
        let root: Value = serde_json::from_str(DESCRIPTION_TEXT).map_err(|e| e.to_string())?;
        let definitions = root
            .get("definitions")
            .and_then(Value::as_object)
            .ok_or("it has no definitions")?
            .clone();
        let mut patterns = HashMap::new();
        collect_patterns(&root, &mut patterns)?;
        Ok(Self {
            definitions,
            patterns,
        })
    }

    fn definition(&self, name: &str) -> Result<&Value, DecodeError> {
        self.definitions
            .get(name)
            .ok_or_else(|| DecodeError::Description(format!("it does not define {name}")))
    }

    /// Check `value` against `schema`; `at` is where `value` sits in the message.
    fn check(&self, schema: &Value, value: &Value, at: &str) -> Result<(), DecodeError> {
        let schema = schema.as_object().ok_or_else(|| {
            DecodeError::Description(format!("a rule for {at:?} is not an object"))
        })?;

        for (keyword, rule) in schema {
            match keyword.as_str() {
                // Words that say nothing about a value, and the two that
                // are read together with "properties".
                "description"
                | "title"
                | "$schema"
                | "definitions"
                | "required"
                | "additionalProperties" => {}
                "$ref" => {
                    let name = rule
                        .as_str()
                        .and_then(|reference| reference.strip_prefix("#/definitions/"))
                        .ok_or_else(|| DecodeError::Description(format!("bad reference {rule}")))?;
                    self.check(self.definition(name)?, value, at)?;
                }
                "type" => check_type(rule, value, at)?,
                "const" => {
                    if rule != value {
                        return Err(violation(at, format!("must be {rule}")));
                    }
                }
                "enum" => {
                    let allowed = rule
                        .as_array()
                        .ok_or_else(|| DecodeError::Description("enum is not a list".into()))?;
                    if !allowed.contains(value) {
                        return Err(violation(
                            at,
                            format!("{value} is not one of the allowed values"),
                        ));
                    }
                }
                "minLength" | "maxLength" => check_length(keyword, rule, value, at)?,
                "minimum" => {
                    if let (Some(least), Some(number)) = (rule.as_i64(), value.as_i64())
                        && number < least
                    {
                        return Err(violation(at, format!("{number} is less than {least}")));
                    }
                }
                "pattern" => self.check_pattern(rule, value, at)?,
                "items" => {
                    if let Some(items) = value.as_array() {
                        for (index, item) in items.iter().enumerate() {
                            self.check(rule, item, &format!("{at}/{index}"))?;
                        }
                    }
                }
                "properties" => self.check_properties(schema, rule, value, at)?,
                "oneOf" => self.check_one_of(rule, value, at)?,
                other => {
                    return Err(DecodeError::Description(format!(
                        "it uses `{other}`, which this checker does not enforce"
                    )));
                }
            }
        }
        Ok(())
    }

    fn check_properties(
        &self,
        schema: &Map<String, Value>,
        properties: &Value,
        value: &Value,
        at: &str,
    ) -> Result<(), DecodeError> {
        let (Some(properties), Some(object)) = (properties.as_object(), value.as_object()) else {
            return Ok(());
        };
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for name in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(name) {
                    return Err(violation(at, format!("`{name}` is missing")));
                }
            }
        }
        // A description object without the closing word would let anything
        // through; the build never allows one, and neither does this.
        if schema.get("additionalProperties") != Some(&Value::Bool(false)) {
            return Err(DecodeError::Description(format!(
                "the object rule for {at:?} does not close its set of fields"
            )));
        }
        for (name, field) in object {
            let rule = properties.get(name).ok_or_else(|| {
                violation(at, format!("`{name}` is not a field the protocol knows"))
            })?;
            self.check(rule, field, &format!("{at}/{name}"))?;
        }
        Ok(())
    }

    /// Exactly one alternative must hold. For the tagged alternatives the
    /// description uses, the tag picks the one whose refusal is worth
    /// reporting.
    fn check_one_of(
        &self,
        alternatives: &Value,
        value: &Value,
        at: &str,
    ) -> Result<(), DecodeError> {
        let alternatives = alternatives
            .as_array()
            .ok_or_else(|| DecodeError::Description("oneOf is not a list".into()))?;
        let mut holding = 0_usize;
        let mut reported = None;
        for alternative in alternatives {
            match self.check(alternative, value, at) {
                Ok(()) => holding += 1,
                Err(error @ DecodeError::Description(_)) => return Err(error),
                Err(error) => {
                    if has_tag_of(alternative, value) {
                        reported = Some(error);
                    }
                }
            }
        }
        match holding {
            1 => Ok(()),
            0 => Err(reported.unwrap_or_else(|| violation(at, "is of no kind the protocol knows"))),
            _ => Err(DecodeError::Description(format!(
                "alternatives for {at:?} are not exclusive"
            ))),
        }
    }

    fn check_pattern(&self, rule: &Value, value: &Value, at: &str) -> Result<(), DecodeError> {
        let (Some(pattern), Some(text)) = (rule.as_str(), value.as_str()) else {
            return Ok(());
        };
        let regex = self.patterns.get(pattern).ok_or_else(|| {
            DecodeError::Description(format!("pattern {pattern} was not prepared"))
        })?;
        if regex.find(text).is_none() {
            return Err(violation(at, "is not of the required form"));
        }
        Ok(())
    }
}

/// Whether `value` carries the `type` tag this alternative is for.
fn has_tag_of(alternative: &Value, value: &Value) -> bool {
    let tag = alternative.pointer("/properties/type/const");
    tag.is_some() && tag == value.get("type")
}

fn check_type(rule: &Value, value: &Value, at: &str) -> Result<(), DecodeError> {
    let holds = match rule.as_str() {
        Some("object") => value.is_object(),
        Some("array") => value.is_array(),
        Some("string") => value.is_string(),
        // A whole number written as a JSON integer: 1.5 and 1.0 are not one.
        Some("integer") => value.is_i64() || value.is_u64(),
        _ => {
            return Err(DecodeError::Description(format!(
                "type {rule} is not one this checker knows"
            )));
        }
    };
    if holds {
        Ok(())
    } else {
        Err(violation(at, format!("must be of type {rule}")))
    }
}

fn check_length(keyword: &str, rule: &Value, value: &Value, at: &str) -> Result<(), DecodeError> {
    let (Some(bound), Some(text)) = (rule.as_u64(), value.as_str()) else {
        return Ok(());
    };
    let length = u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
    let holds = if keyword == "minLength" {
        length >= bound
    } else {
        length <= bound
    };
    if holds {
        Ok(())
    } else if keyword == "minLength" {
        Err(violation(at, format!("is shorter than {bound} characters")))
    } else {
        Err(violation(at, format!("is longer than {bound} characters")))
    }
}

fn collect_patterns(
    node: &Value,
    patterns: &mut HashMap<String, regress::Regex>,
) -> Result<(), String> {
    match node {
        Value::Object(map) => {
            for (key, value) in map {
                if let ("pattern", Some(pattern)) = (key.as_str(), value.as_str()) {
                    let regex = regress::Regex::new(pattern)
                        .map_err(|error| format!("pattern {pattern} does not compile: {error}"))?;
                    patterns.insert(pattern.to_owned(), regex);
                }
                collect_patterns(value, patterns)?;
            }
            Ok(())
        }
        Value::Array(items) => items
            .iter()
            .try_for_each(|item| collect_patterns(item, patterns)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Ok(()),
    }
}
