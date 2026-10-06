// crates/asterisk-controller/src/config.rs

//! What the controller is told when it starts.
//!
//! These are facts of the machine and of the launch — where things are and
//! which ports are this node's — known to whoever starts the controller and
//! Asterisk. What the node does with calls is not here: that is its
//! settings ([`crate::settings`]), which come from the application.

use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};

/// Protocol version this controller speaks.
pub const PROTOCOL_VERSION: u64 = 1;

const USAGE: &str = "usage: asterisk-controller \
    --node <name> --listen <127.0.0.1:port> --application <ws://host:port/path> \
    --asterisk-tree <directory> --state <directory> --asterisk-http <127.0.0.1:port> \
    --audio-ports <first>-<last> [--sip <address:port>] [--sip-public <address>]";

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
    /// The Asterisk tree this controller is paired with. Only read.
    pub asterisk_tree: PathBuf,
    /// Where everything that belongs to this node alone lives: the
    /// configuration the controller writes for Asterisk, the directories
    /// Asterisk writes to, and the node's stored settings.
    pub state: PathBuf,
    /// Where Asterisk's own HTTP server listens. Loopback only: it carries
    /// call control, and nothing on a telephony node accepts commands from
    /// the network.
    pub asterisk_http: SocketAddr,
    /// The UDP ports call audio may use, first and last.
    pub audio_ports: (u16, u16),
    /// Where Asterisk listens for telephone operators. Needed only when the
    /// settings name an operator; without it Asterisk listens for none.
    pub sip: Option<SocketAddr>,
    /// The address operators reach this node at, when it differs from the
    /// one Asterisk listens on — a machine behind address translation.
    pub sip_public: Option<IpAddr>,
}

impl Config {
    /// Read the configuration from the command line.
    ///
    /// # Errors
    ///
    /// A missing, unknown or malformed argument is an error with the usage:
    /// the controller never starts on a guess.
    pub fn from_arguments(arguments: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut node = None;
        let mut listen = None;
        let mut application_url = None;
        let mut asterisk_tree = None;
        let mut state = None;
        let mut asterisk_http = None;
        let mut audio_ports = None;
        let mut sip = None;
        let mut sip_public = None;

        let mut arguments = arguments;
        while let Some(name) = arguments.next() {
            let value = arguments
                .next()
                .ok_or_else(|| format!("{name} needs a value\n{USAGE}"))?;
            match name.as_str() {
                "--node" => node = Some(value),
                "--listen" => listen = Some(loopback(&name, &value)?),
                "--application" => {
                    if !(value.starts_with("ws://") || value.starts_with("wss://")) {
                        return Err(format!(
                            "--application {value}: must be a ws:// or wss:// address\n{USAGE}"
                        ));
                    }
                    application_url = Some(value);
                }
                "--asterisk-tree" => asterisk_tree = Some(PathBuf::from(value)),
                "--state" => state = Some(PathBuf::from(value)),
                "--asterisk-http" => asterisk_http = Some(loopback(&name, &value)?),
                "--audio-ports" => audio_ports = Some(port_range(&value)?),
                "--sip" => {
                    sip = Some(
                        value
                            .parse()
                            .map_err(|error| format!("--sip {value}: {error}\n{USAGE}"))?,
                    );
                }
                "--sip-public" => {
                    sip_public = Some(
                        value
                            .parse()
                            .map_err(|error| format!("--sip-public {value}: {error}\n{USAGE}"))?,
                    );
                }
                other => return Err(format!("unknown argument {other}\n{USAGE}")),
            }
        }

        let missing = |name: &str| format!("{name} is missing\n{USAGE}");
        Ok(Self {
            node: node.ok_or_else(|| missing("--node"))?,
            listen: listen.ok_or_else(|| missing("--listen"))?,
            application_url: application_url.ok_or_else(|| missing("--application"))?,
            asterisk_tree: asterisk_tree.ok_or_else(|| missing("--asterisk-tree"))?,
            state: state.ok_or_else(|| missing("--state"))?,
            asterisk_http: asterisk_http.ok_or_else(|| missing("--asterisk-http"))?,
            audio_ports: audio_ports.ok_or_else(|| missing("--audio-ports"))?,
            sip,
            sip_public,
        })
    }
}

/// An address that must be on the loopback interface.
fn loopback(name: &str, value: &str) -> Result<SocketAddr, String> {
    let address: SocketAddr = value
        .parse()
        .map_err(|error| format!("{name} {value}: {error}\n{USAGE}"))?;
    if address.ip().is_loopback() {
        Ok(address)
    } else {
        Err(format!(
            "{name} {value}: the controller and Asterisk talk to each other on the loopback \
             address only; nothing on a telephony node accepts commands from the network"
        ))
    }
}

/// A range of ports written as `first-last`.
fn port_range(value: &str) -> Result<(u16, u16), String> {
    let malformed = || format!("--audio-ports {value}: must be <first>-<last>\n{USAGE}");
    let (first, last) = value.split_once('-').ok_or_else(malformed)?;
    let first: u16 = first.parse().map_err(|_| malformed())?;
    let last: u16 = last.parse().map_err(|_| malformed())?;
    if first == 0 || first > last {
        return Err(malformed());
    }
    // Asterisk gives a call an even port for its audio and the odd one after
    // it for the reports about that audio. Given an odd first port it moves
    // the range up by one and says so on every start. Given an even last port
    // it can hand that port out, and the reports of that call then go to the
    // port after it — one that is not this node's.
    if !first.is_multiple_of(2) {
        return Err(format!(
            "--audio-ports {value}: the first port must be even; Asterisk uses ports in pairs \
             that begin on an even one"
        ));
    }
    if last.is_multiple_of(2) {
        return Err(format!(
            "--audio-ports {value}: the last port must be odd; Asterisk uses ports in pairs \
             that end on an odd one, and would take the port after an even last one"
        ));
    }
    Ok((first, last))
}

#[cfg(test)]
mod tests {
    use super::port_range;

    #[test]
    fn a_range_of_whole_pairs_is_accepted() {
        assert_eq!(port_range("4100-4199"), Ok((4100, 4199)));
        assert_eq!(port_range("4100-4101"), Ok((4100, 4101)));
    }

    #[test]
    fn an_odd_first_port_is_refused() {
        assert!(port_range("4101-4199").is_err());
    }

    #[test]
    fn an_even_last_port_is_refused() {
        assert!(port_range("4100-4198").is_err());
        assert!(port_range("4100-4100").is_err());
    }

    #[test]
    fn what_is_not_a_range_is_refused() {
        assert!(port_range("4100").is_err());
        assert!(port_range("4199-4100").is_err());
        assert!(port_range("0-4199").is_err());
        assert!(port_range("first-last").is_err());
    }
}
