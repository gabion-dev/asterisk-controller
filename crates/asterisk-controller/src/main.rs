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

mod application;
mod ari;
mod asterisk;
mod asterisk_files;
mod config;
mod conversation;
mod destination;
mod media;
mod node;
mod playout;
mod prompts;
mod reports;
mod service;
mod settings;
mod state_files;

use std::{
    process::ExitCode,
    sync::{Arc, RwLock},
};
use tokio::{
    net::{TcpListener, TcpStream},
    signal::unix::{SignalKind, signal},
};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        handshake::server::{ErrorResponse, Request, Response},
        http::{
            HeaderValue, StatusCode,
            header::{AUTHORIZATION, SEC_WEBSOCKET_PROTOCOL},
        },
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
    let (applied, secret, asterisk_version) = match prepare_asterisk(&config) {
        Ok(prepared) => prepared,
        Err(problem) => {
            eprintln!("asterisk-controller: {problem}");
            return ExitCode::FAILURE;
        }
    };

    // A node that cannot prove itself to the application, or judge who
    // answers for it, does not start.
    let application = match application::Application::new(&config) {
        Ok(application) => application,
        Err(problem) => {
            eprintln!("asterisk-controller: {problem}");
            return ExitCode::FAILURE;
        }
    };

    // Whoever runs the node stops the controller with SIGTERM, or with
    // SIGINT from a terminal; either is a stop asked for, and ends it with
    // success.
    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(terminate) => terminate,
        Err(error) => {
            eprintln!("asterisk-controller: cannot listen for SIGTERM: {error}");
            return ExitCode::FAILURE;
        }
    };

    // Whoever renews the node's certificate for operators says so with
    // SIGHUP: Asterisk is then given it, with no call dropped.
    let mut hangup = match signal(SignalKind::hangup()) {
        Ok(hangup) => hangup,
        Err(error) => {
            eprintln!("asterisk-controller: cannot listen for SIGHUP: {error}");
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
    let media_authorization: Arc<str> = media::authorization(&secret).into();
    let reports = reports::Journal::of(&config.state);
    let node = Arc::new(Node {
        config,
        application,
        asterisk_version,
        run: uuid::Uuid::new_v4()
            .simple()
            .to_string()
            .chars()
            .take(12)
            .collect(),
        applied: RwLock::new(Arc::new(applied)),
        reports,
        door: media::Door::default(),
        lines: node::Lines::default(),
        replaced: tokio::sync::Notify::new(),
    });
    tokio::spawn(asterisk::run(Arc::clone(&node), Arc::clone(&asterisk)));
    tokio::spawn(service::run(Arc::clone(&node), Arc::clone(&asterisk)));

    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _peer)) => {
                    tokio::spawn(accept_media(
                        stream,
                        node.door.clone(),
                        Arc::clone(&media_authorization),
                    ));
                }
                Err(error) => eprintln!("asterisk-controller: accept failed: {error}"),
            },
            _ = tokio::signal::ctrl_c() => return ExitCode::SUCCESS,
            _ = terminate.recv() => return ExitCode::SUCCESS,
            _ = hangup.recv() => {
                tokio::spawn(certificate_renewed(Arc::clone(&node), Arc::clone(&asterisk)));
            }
            () = node.replaced.notified() => return ExitCode::FAILURE,
        }
    }
}

/// Read the node's settings and write Asterisk's configuration from them.
/// Returns the settings, the secret of the control user and the version of
/// Asterisk.
fn prepare_asterisk(config: &Config) -> Result<(settings::Applied, String, String), String> {
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
    for prompt in prompts::missing(&config.state, &applied.settings) {
        eprintln!(
            "asterisk-controller: prompt {:?} is not here; it is fetched when the application \
             welcomes the node, and until then a message fallback with it turns callers away",
            prompt.id.as_str()
        );
    }
    eprintln!(
        "asterisk-controller: Asterisk {} is to be started with {}",
        tree.version,
        main_file.display()
    );
    Ok((applied, secret, tree.version))
}

/// The node's certificate for operators may have been renewed: Asterisk's
/// configuration follows it, and Asterisk takes the new one in.
async fn certificate_renewed(node: Arc<Node>, asterisk: Arc<asterisk::Asterisk>) {
    match service::files_follow(&node, &asterisk).await {
        Ok(()) => eprintln!(
            "asterisk-controller: SIGHUP — Asterisk's configuration follows the files it is \
             made from"
        ),
        Err(problem) => eprintln!(
            "asterisk-controller: SIGHUP — Asterisk's configuration could not follow the files \
             it is made from: {problem}"
        ),
    }
}

/// Complete the WebSocket handshake of a media connection of Asterisk and
/// serve it. Only Asterisk is let in: the connection must present the
/// control secret Asterisk has from its configuration.
async fn accept_media(stream: TcpStream, door: media::Door, authorization: Arc<str>) {
    #[expect(
        clippy::result_large_err,
        reason = "the callback's signature is the WebSocket library's"
    )]
    let agree = |request: &Request, mut response: Response| {
        if request.uri().path() != media::PATH {
            return Err(not_found(request.uri().path()));
        }
        let presented = request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        if presented != Some(&*authorization) {
            return Err(unauthorized());
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

/// The refusal of a media connection that is not Asterisk's.
fn unauthorized() -> ErrorResponse {
    let mut refusal = ErrorResponse::new(Some(
        "a media connection is Asterisk's, with the node's control secret".to_owned(),
    ));
    *refusal.status_mut() = StatusCode::UNAUTHORIZED;
    refusal
}

/// The refusal of a connection to a path the controller does not have.
fn not_found(path: &str) -> ErrorResponse {
    let mut refusal = ErrorResponse::new(Some(format!("the controller has no {path}")));
    *refusal.status_mut() = StatusCode::NOT_FOUND;
    refusal
}
