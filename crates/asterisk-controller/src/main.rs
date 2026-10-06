// crates/asterisk-controller/src/main.rs

//! The Asterisk controller of a Gabion telephony node.
//!
//! Runs next to Asterisk on the same machine and controls it on behalf of
//! the application. Asterisk opens a control connection to the controller
//! for every call; for every call the controller opens a conversation
//! connection to the application and translates between Asterisk's language
//! and the Gabion node protocol.
//!
//! Nothing reaches the controller from the network: it listens on the
//! loopback address only, and every connection to the application is opened
//! from here.

mod ari;
mod config;
mod conversation;

use std::{process::ExitCode, sync::Arc};

use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        handshake::server::{ErrorResponse, Request, Response},
        http::{HeaderValue, header::SEC_WEBSOCKET_PROTOCOL},
    },
};

use crate::config::Config;

/// The WebSocket subprotocol Asterisk speaks on a control connection.
const ASTERISK_SUBPROTOCOL: &str = "ari";

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

    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _peer)) => {
                    tokio::spawn(accept_call(stream, Arc::clone(&config)));
                }
                Err(error) => eprintln!("asterisk-controller: accept failed: {error}"),
            },
            _ = tokio::signal::ctrl_c() => return ExitCode::SUCCESS,
        }
    }
}

/// Complete the WebSocket handshake of one control connection and serve it.
async fn accept_call(stream: TcpStream, config: Arc<Config>) {
    #[expect(
        clippy::result_large_err,
        clippy::unnecessary_wraps,
        reason = "the callback's signature is the WebSocket library's"
    )]
    fn agree_on_subprotocol(
        _: &Request,
        mut response: Response,
    ) -> Result<Response, ErrorResponse> {
        response.headers_mut().insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static(ASTERISK_SUBPROTOCOL),
        );
        Ok(response)
    }

    match accept_hdr_async(stream, agree_on_subprotocol).await {
        Ok(asterisk) => conversation::serve(asterisk, config).await,
        Err(error) => eprintln!("asterisk-controller: handshake with Asterisk failed: {error}"),
    }
}
