// crates/asterisk-controller/src/main.rs

//! The Asterisk controller of a Gabion telephony node.
//!
//! Runs next to Asterisk on the same machine and controls it on behalf of
//! the application. Asterisk opens a control connection to the controller
//! for every call; for every call the controller opens a conversation
//! connection to the application and translates between Asterisk's language
//! and the Gabion node protocol. For audio Asterisk opens media connections
//! to the same address, told apart by the path it asks for.
//!
//! Nothing reaches the controller from the network: it listens on the
//! loopback address only, and every connection to the application is opened
//! from here.

mod ari;
mod config;
mod conversation;
mod media;
mod playout;

use std::{process::ExitCode, sync::Arc};

use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        handshake::server::{ErrorResponse, Request, Response},
        http::{HeaderValue, StatusCode, header::SEC_WEBSOCKET_PROTOCOL},
    },
};

use crate::config::Config;

/// The path Asterisk asks for when it opens a call's control connection.
const CONTROL_PATH: &str = "/";
/// The WebSocket subprotocol Asterisk speaks on a control connection.
const CONTROL_SUBPROTOCOL: &str = "ari";

/// What Asterisk opened a connection for.
#[derive(Clone, Copy)]
enum Purpose {
    /// To have a call controlled.
    Control,
    /// To carry the audio of a media channel.
    Media,
}

#[tokio::main]
async fn main() -> ExitCode {
    let config = match Config::from_arguments(std::env::args().skip(1)) {
        Ok(config) => Arc::new(config),
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
    // The line a supervisor waits for: the controller accepts Asterisk.
    println!("asterisk-controller ready on {}", config.listen);

    let door = media::Door::default();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _peer)) => {
                    tokio::spawn(accept(stream, Arc::clone(&config), door.clone()));
                }
                Err(error) => eprintln!("asterisk-controller: accept failed: {error}"),
            },
            _ = tokio::signal::ctrl_c() => return ExitCode::SUCCESS,
        }
    }
}

/// Complete the WebSocket handshake of one connection of Asterisk and serve
/// it as what its path says it is.
async fn accept(stream: TcpStream, config: Arc<Config>, door: media::Door) {
    let mut purpose = None;
    #[expect(
        clippy::result_large_err,
        reason = "the callback's signature is the WebSocket library's"
    )]
    let agree = |request: &Request, mut response: Response| {
        let (asked_for, subprotocol) = match request.uri().path() {
            CONTROL_PATH => (Purpose::Control, CONTROL_SUBPROTOCOL),
            media::PATH => (Purpose::Media, media::SUBPROTOCOL),
            other => return Err(not_found(other)),
        };
        purpose = Some(asked_for);
        response.headers_mut().insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static(subprotocol),
        );
        Ok(response)
    };

    match accept_hdr_async(stream, agree).await {
        Ok(asterisk) => match purpose {
            Some(Purpose::Control) => conversation::serve(asterisk, config, door).await,
            Some(Purpose::Media) => media::serve(asterisk, door).await,
            // A handshake that succeeded has been through `agree`.
            None => {}
        },
        Err(error) => eprintln!("asterisk-controller: handshake with Asterisk failed: {error}"),
    }
}

/// The refusal of a connection to a path the controller does not have.
fn not_found(path: &str) -> ErrorResponse {
    let mut refusal = ErrorResponse::new(Some(format!("the controller has no {path}")));
    *refusal.status_mut() = StatusCode::NOT_FOUND;
    refusal
}
