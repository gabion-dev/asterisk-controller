// crates/node-protocol/build.rs
//! Generates the protocol message types from the protocol description.
//!
//! The description — `protocol/node-protocol.schema.json` at the repository
//! root — is the single source of these types. Nothing here is written by
//! hand, so the Rust side cannot drift from the description; the Java side
//! of the application is generated from the same file.

use std::{env, error::Error, fs, path::PathBuf};

use typify::{TypeSpace, TypeSpaceSettings};

fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let schema_path = manifest_dir.join("../../protocol/node-protocol.schema.json");
    println!("cargo::rerun-if-changed={}", schema_path.display());

    let text = fs::read_to_string(&schema_path)?;
    refuse_unenforced_keywords(&serde_json::from_str(&text)?, "")?;
    let schema: schemars::schema::RootSchema = serde_json::from_str(&text)?;

    let mut settings = TypeSpaceSettings::default();
    settings.with_struct_builder(false);
    let mut type_space = TypeSpace::new(&settings);
    type_space.add_root_schema(schema)?;

    let file = syn::parse2::<syn::File>(type_space.to_stream())?;
    let out = PathBuf::from(env::var("OUT_DIR")?).join("messages.rs");
    fs::write(out, prettyplease::unparse(&file))?;
    Ok(())
}

/// Keywords of the description that the generated types are known to
/// enforce — each one is covered by a test in `tests/strictness.rs`.
const ENFORCED_KEYWORDS: &[&str] = &[
    "$ref",
    "$schema",
    "additionalProperties",
    "const",
    "definitions",
    "description",
    "enum",
    "items",
    "maxLength",
    "minLength",
    "minimum",
    "oneOf",
    "pattern",
    "properties",
    "required",
    "title",
    "type",
];

/// Stop the build when the description uses a keyword the generated types
/// would not enforce.
///
/// The generator accepts such keywords and drops them without a word —
/// `minItems` was found that way: the description said "at least two", the
/// type took one. A description that promises more than the types check is
/// worse than a shorter description, so the promise is refused here, at
/// build time, with the place it was made.
fn refuse_unenforced_keywords(node: &serde_json::Value, path: &str) -> Result<(), Box<dyn Error>> {
    match node {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                let here = format!("{path}/{key}");
                // Under these two keywords the keys are names chosen by the
                // description, not keywords.
                let names_follow = matches!(key.as_str(), "properties" | "definitions");
                if !names_follow && !ENFORCED_KEYWORDS.contains(&key.as_str()) && !is_name(path) {
                    return Err(format!(
                        "protocol description uses `{key}` at {here}: the generated types do not \
                         enforce it. Express the rule another way, or teach the types to enforce \
                         it and add the keyword to ENFORCED_KEYWORDS together with a test."
                    )
                    .into());
                }
                refuse_unenforced_keywords(value, &here)?;
            }
            Ok(())
        }
        serde_json::Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                refuse_unenforced_keywords(item, &format!("{path}/{index}"))?;
            }
            Ok(())
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => Ok(()),
    }
}

/// Whether the object at `path` maps names to schemas (its keys are not keywords).
fn is_name(path: &str) -> bool {
    path.ends_with("/properties") || path.ends_with("/definitions")
}
