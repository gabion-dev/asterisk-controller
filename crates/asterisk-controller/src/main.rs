// crates/asterisk-controller/src/main.rs

//! The Asterisk controller of a Gabion telephony node.
//!
//! Runs next to Asterisk on the same machine and controls it on behalf of
//! the application. The controller writes Asterisk's configuration, connects
//! to Asterisk's control interface on the loopback address, and for every
//! call opens a conversation connection to the application and translates
//! between Asterisk's language and the Gabion node protocol. For audio
//! Asterisk opens media connections to the controller.
//!
//! Nothing reaches the controller from the network: it listens on the
//! loopback address only, and every connection to the application is opened
//! from here.

mod ari;
mod asterisk;
mod asterisk_files;
mod config;
mod conversation;
mod destination;
mod media;
mod node;
mod playout;
mod settings;

use std::{process::ExitCode, sync::Arc};

use node_protocol::messages::Settings;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        handshake::server::{ErrorResponse, Request, Response},
        http::{HeaderValue, StatusCode, header::SEC_WEBSOCKET_PROTOCOL},
    },
};

use crate::{config::Config, node::Node};

#[tokio::main]
async fn main() -> ExitCode {
    let config = match Config::from_arguments(std::env::args().skip(1)) {
        Ok(config) => config,
        Err(problem) => {
            eprintln!("asterisk-controller: {problem}");
            return ExitCode::FAILURE;
        }
    };

    // Asterisk is started on what is written here, so it is written before
    // the controller says it is ready.
    let (settings, secret) = match prepare_asterisk(&config) {
        Ok(prepared) => prepared,
        Err(problem) => {
            eprintln!("asterisk-controller: {problem}");
            return ExitCode::FAILURE;
        }
    };

    let listener = match TcpListener::bind(config.listen).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!(
                "asterisk-controller: cannot listen on {}: {error}",
                config.listen
            );
            return ExitCode::FAILURE;
        }
    };
    // The line whoever starts the node waits for: Asterisk's configuration
    // is written and its media connections will be accepted. Asterisk may be
    // started now; the controller finds it by itself.
    println!("asterisk-controller ready on {}", config.listen);

    let asterisk = asterisk::Asterisk::new(config.asterisk_http, &secret);
    let node = Arc::new(Node {
        config,
        settings,
        door: media::Door::default(),
        lines: node::Lines::default(),
    });
    tokio::spawn(asterisk::run(Arc::clone(&node), asterisk));

    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _peer)) => {
                    tokio::spawn(accept_media(stream, node.door.clone()));
                }
                Err(error) => eprintln!("asterisk-controller: accept failed: {error}"),
            },
            _ = tokio::signal::ctrl_c() => return ExitCode::SUCCESS,
        }
    }
}

/// Read the node's settings and write Asterisk's configuration from them.
/// Returns the settings and the secret of the control user.
fn prepare_asterisk(config: &Config) -> Result<(Settings, String), String> {
    let tree = asterisk_files::Tree::read(&config.asterisk_tree)?;
    let applied = settings::stored(&config.state)?;
    let secret = asterisk_files::control_secret(&config.state)?;
    let main_file = asterisk_files::write(config, &tree, &applied.settings, &secret)?;

    match &applied.fingerprint {
        Some(fingerprint) => eprintln!(
            "asterisk-controller: settings {fingerprint} are applied: {} operators, {} lines, \
             {} entries",
            applied.settings.operators.len(),
            applied.settings.lines.len(),
            applied.settings.entries.len(),
        ),
        None => eprintln!(
            "asterisk-controller: the node has no settings yet: no call enters or leaves it"
        ),
    }
    eprintln!(
        "asterisk-controller: Asterisk {} is to be started with {}",
        tree.version,
        main_file.display()
    );
    Ok((applied.settings, secret))
}

/// Complete the WebSocket handshake of a media connection of Asterisk and
/// serve it.
async fn accept_media(stream: TcpStream, door: media::Door) {
    #[expect(
        clippy::result_large_err,
        reason = "the callback's signature is the WebSocket library's"
    )]
    let agree = |request: &Request, mut response: Response| {
        if request.uri().path() != media::PATH {
            return Err(not_found(request.uri().path()));
        }
        response.headers_mut().insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static(media::SUBPROTOCOL),
        );
        Ok(response)
    };
    match accept_hdr_async(stream, agree).await {
        Ok(asterisk) => media::serve(asterisk, door).await,
        Err(error) => eprintln!("asterisk-controller: handshake with Asterisk failed: {error}"),
    }
}

/// The refusal of a connection to a path the controller does not have.
fn not_found(path: &str) -> ErrorResponse {
    let mut refusal = ErrorResponse::new(Some(format!("the controller has no {path}")));
    *refusal.status_mut() = StatusCode::NOT_FOUND;
    refusal
}
