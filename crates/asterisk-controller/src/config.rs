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
    --node <name> --listen <127.0.0.1:port> --application <wss://host[:port][/path]> \
    --asterisk-tree <directory> --state <directory> --asterisk-http <127.0.0.1:port> \
    --audio-ports <first>-<last> [--sip <address:port>] [--sip-public <address>]";

/// Everything the controller is told at start.
#[derive(Clone, Debug)]
pub struct Config {
    /// Name of this node, as the application knows it. Letters, digits,
    /// `.`, `_` and `-`: it travels as the user name of HTTP Basic
    /// authentication, where a colon would end it.
    pub node: String,
    /// Where Asterisk on the same machine connects to. Loopback only: the
    /// controller refuses any other address.
    pub listen: SocketAddr,
    /// The address of the application's side of the node protocol, without
    /// a trailing slash. Conversation connections are opened to
    /// `<address>/conversation`, the service connection to
    /// `<address>/service`. `wss://`, except on the loopback address: the
    /// node's secret travels on every connection.
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
                "--node" => node = Some(node_name(&value)?),
                "--listen" => listen = Some(loopback(&name, &value)?),
                "--application" => application_url = Some(application(&value)?),
                "--asterisk-tree" => asterisk_tree = Some(PathBuf::from(value)),
                "--state" => state = Some(state_directory(&value)?),
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

/// A node name: what may travel as the user name of HTTP Basic
/// authentication and be read back as itself.
fn node_name(value: &str) -> Result<String, String> {
    let fits = (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if fits {
        Ok(value.to_owned())
    } else {
        Err(format!(
            "--node {value:?}: a node name is 1 to 128 letters, digits, '.', '_' and '-'\n{USAGE}"
        ))
    }
}

/// The address of the application: `wss://` — or `ws://` to the loopback
/// address, where nothing travels over a network. The node's secret goes
/// with every connection, and is never sent in the clear across one.
fn application(value: &str) -> Result<String, String> {
    let (secure, rest) = if let Some(rest) = value.strip_prefix("wss://") {
        (true, rest)
    } else if let Some(rest) = value.strip_prefix("ws://") {
        (false, rest)
    } else {
        return Err(format!(
            "--application {value}: must be a wss:// address\n{USAGE}"
        ));
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };
    if host.is_empty() || value.contains(['?', '#']) {
        return Err(format!(
            "--application {value}: must be wss://host[:port][/path]\n{USAGE}"
        ));
    }
    let on_loopback =
        host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    if !secure && !on_loopback {
        return Err(format!(
            "--application {value}: the node's secret goes with every connection to the \
             application; across a network that needs wss://"
        ));
    }
    Ok(value.trim_end_matches('/').to_owned())
}

/// The state directory. Asterisk plays prompts from files in it, and the
/// path of a file is written into a dialplan step, where a comma ends an
/// argument, `&` separates files, `$` and brackets begin an expression and
/// a semicolon a comment: a path with any of them would be read as
/// something else.
fn state_directory(value: &str) -> Result<PathBuf, String> {
    if value
        .chars()
        .any(|character| character.is_control() || ",&;$()[]{}\\\"'|".contains(character))
    {
        return Err(format!(
            "--state {value:?}: the path may not hold control characters or any of              , & ; $ ( ) [ ] {{ }} \\ \" ' | — Asterisk plays prompts from it, and would read              such a path as something else"
        ));
    }
    Ok(PathBuf::from(value))
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
    use super::{application, node_name, port_range, state_directory};

    #[test]
    fn a_state_directory_asterisk_would_misread_is_refused() {
        assert!(state_directory("/home/user/My Project/.gabion/telephony").is_ok());
        for path in ["/a,b", "/a&b", "/a$b", "/a;b", "/a(b)", "/a\nb"] {
            assert!(state_directory(path).is_err(), "{path:?}");
        }
    }

    #[test]
    fn the_application_is_reached_by_wss_except_on_the_loopback_address() {
        assert_eq!(
            application("wss://app.example.com/node/"),
            Ok("wss://app.example.com/node".to_owned())
        );
        assert_eq!(
            application("ws://127.0.0.1:8080/"),
            Ok("ws://127.0.0.1:8080".to_owned())
        );
        assert!(application("ws://[::1]:8080").is_ok());
        assert!(application("ws://localhost:8080").is_ok());
        assert!(application("ws://app.example.com/node").is_err());
        assert!(application("ws://10.0.0.5:8080").is_err());
        assert!(application("https://app.example.com").is_err());
        assert!(application("wss:///node").is_err());
        assert!(application("wss://app.example.com/node?x=1").is_err());
    }

    #[test]
    fn a_node_name_is_what_basic_authentication_carries_as_itself() {
        assert!(node_name("node-1.us_west").is_ok());
        assert!(node_name("node:1").is_err());
        assert!(node_name("").is_err());
        assert!(node_name("nöde").is_err());
        assert!(node_name(&"n".repeat(129)).is_err());
    }

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
