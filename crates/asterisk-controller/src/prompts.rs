// crates/asterisk-controller/src/prompts.rs

//! Prompts: recorded audio the node plays by itself — the message of an
//! entry's fallback when no application can answer a caller.
//!
//! The settings name a prompt by identifier and by the digest of its audio.
//! The node fetches the audio from the application
//! (`<address>/prompts/<identifier>`, [`crate::application`]), checks it
//! against the digest and keeps it in the state directory under the digest:
//! audio that changes is a new file, and a file is never half the old prompt
//! and half the new. Prompts are fetched when settings that name them are
//! applied, and settings whose prompts cannot be had are refused — a node
//! never runs on settings with a prompt it cannot play.
//!
//! The audio is the protocol's one format — signed 16-bit linear PCM,
//! little-endian, mono, 16 kHz — which Asterisk plays as `sln16` from the
//! file's full path.

use std::{
    collections::HashSet,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use node_protocol::messages::{Prompt, Settings};

use crate::{application::Application, ari};

/// The directory of the state directory that holds prompts.
const DIRECTORY: &str = "prompts";
/// The extension Asterisk knows the protocol's audio format by.
const EXTENSION: &str = "sln16";
/// The largest prompt the node takes: about eight and a half minutes. A
/// message of a fallback is a few sentences; this only keeps the node from
/// reading without end whatever an address answers with.
const LARGEST: usize = 16 * 1024 * 1024;

/// Where Asterisk plays a prompt from: its file, without the extension.
pub fn playable(state: &Path, prompt: &Prompt) -> PathBuf {
    state.join(DIRECTORY).join(prompt.sha256.as_str())
}

fn file(state: &Path, prompt: &Prompt) -> PathBuf {
    playable(state, prompt).with_extension(EXTENSION)
}

/// The prompts the settings name that the node does not have.
pub fn missing<'a>(state: &Path, settings: &'a Settings) -> Vec<&'a Prompt> {
    settings
        .prompts
        .iter()
        .filter(|prompt| !file(state, prompt).is_file())
        .collect()
}

/// Fetch every prompt the settings name that the node does not have yet.
///
/// # Errors
///
/// A prompt cannot be fetched, or its audio is not what its digest says or
/// not whole samples; the first problem, in words.
pub async fn fetch_missing(
    state: &Path,
    application: &Application,
    settings: &Settings,
) -> Result<(), String> {
    let directory = state.join(DIRECTORY);
    for prompt in missing(state, settings) {
        let id = prompt.id.as_str();
        let audio = application
            .fetch(&format!("{DIRECTORY}/{}", ari::query(id)), LARGEST)
            .await
            .map_err(|problem| format!("prompt {id:?} cannot be fetched: {problem}"))?;
        if node_protocol::sha256_hex(&audio) != prompt.sha256.as_str() {
            return Err(format!(
                "prompt {id:?} was fetched, but its audio is not what its digest says"
            ));
        }
        if !audio.len().is_multiple_of(2) {
            return Err(format!(
                "prompt {id:?} is not whole samples of 16-bit audio"
            ));
        }
        let failed = |error: std::io::Error| format!("{}: {error}", directory.display());
        fs::create_dir_all(&directory).map_err(failed)?;
        let path = file(state, prompt);
        let written = path.with_extension("new");
        fs::write(&written, &audio).map_err(failed)?;
        fs::rename(&written, &path).map_err(failed)?;
    }
    Ok(())
}

/// Remove the prompts the settings no longer name.
pub fn forget_unnamed(state: &Path, settings: &Settings) {
    let named: HashSet<PathBuf> = settings
        .prompts
        .iter()
        .map(|prompt| file(state, prompt))
        .collect();
    let directory = state.join(DIRECTORY);
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return,
        Err(error) => {
            eprintln!("asterisk-controller: {}: {error}", directory.display());
            return;
        }
    };
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(error) => {
                eprintln!("asterisk-controller: {}: {error}", directory.display());
                continue;
            }
        };
        if !named.contains(&path)
            && let Err(error) = fs::remove_file(&path)
        {
            eprintln!("asterisk-controller: {}: {error}", path.display());
        }
    }
}
