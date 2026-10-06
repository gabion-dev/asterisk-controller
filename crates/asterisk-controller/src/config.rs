// crates/asterisk-controller/src/config.rs

//! What the controller is told when it starts.

use std::net::SocketAddr;

/// Protocol version this controller speaks.
pub const PROTOCOL_VERSION: u64 = 1;

/// Everything the controller is told at start.
#[derive(Clone, Debug)]
pub struct Config {
    /// Name of this node, as the application knows it.
    pub node: String,
    /// Where Asterisk on the same machine connects to. Loopback only: the
    /// controller refuses any other address.
    pub listen: SocketAddr,
    /// Where conversation connections to the application are opened.
    pub application_url: String,
}

impl Config {
    /// Read the configuration from the command line.
    ///
    /// # Errors
    ///
    /// A missing, unknown or malformed argument is an error with the usage:
    /// the controller never starts on a guess.
    pub fn from_arguments(arguments: impl Iterator<Item = String>) -> Result<Self, String> {
        const USAGE: &str = "usage: asterisk-controller --node <name> --listen <127.0.0.1:port> \
                             --application <ws://host:port/path>";
        let mut node = None;
        let mut listen = None;
        let mut application_url = None;

        let mut arguments = arguments;
        while let Some(name) = arguments.next() {
            let value = arguments
                .next()
                .ok_or_else(|| format!("{name} needs a value\n{USAGE}"))?;
            match name.as_str() {
                "--node" => node = Some(value),
                "--listen" => {
                    let address: SocketAddr = value
                        .parse()
                        .map_err(|error| format!("--listen {value}: {error}\n{USAGE}"))?;
                    if !address.ip().is_loopback() {
                        return Err(format!(
                            "--listen {value}: the controller accepts Asterisk on the loopback \
                             address only; nothing on a telephony node accepts commands from \
                             the network"
                        ));
                    }
                    listen = Some(address);
                }
                "--application" => {
                    if !(value.starts_with("ws://") || value.starts_with("wss://")) {
                        return Err(format!(
                            "--application {value}: must be a ws:// or wss:// address\n{USAGE}"
                        ));
                    }
                    application_url = Some(value);
                }
                other => return Err(format!("unknown argument {other}\n{USAGE}")),
            }
        }

        Ok(Self {
            node: node.ok_or_else(|| format!("--node is missing\n{USAGE}"))?,
            listen: listen.ok_or_else(|| format!("--listen is missing\n{USAGE}"))?,
            application_url: application_url
                .ok_or_else(|| format!("--application is missing\n{USAGE}"))?,
        })
    }
}
