// crates/asterisk-controller/src/asterisk_files.rs

//! Asterisk's configuration, written by the controller.
//!
//! Every file Asterisk reads is made here, from three things: where the
//! node is and which ports are its own ([`Config`]), what the Asterisk tree
//! says about itself, and the node's settings. Nobody else writes these
//! files and nobody edits them: the configuration directory is rewritten
//! whole on every start — it is a derivative, never a place to keep
//! anything.
//!
//! The names used here are the controller's own, and the only place they
//! are agreed between the configuration and the code that later uses them.

use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use node_protocol::messages::{EntryKey, Fallback, Operator, Settings, SipTransport};
use sha2::{Digest, Sha256};

use crate::{config::Config, destination, media, prompts};

/// The Asterisk versions this controller knows how to drive. What Asterisk
/// says about its channels, what its media channel does and how its
/// configuration is read are all facts of a version; with any other the
/// controller would be guessing.
const KNOWN_ASTERISK_VERSIONS: &[&str] = &["22.11.0"];

/// The Stasis application every call of the node enters.
pub const APPLICATION: &str = "gabion";
/// The user of Asterisk's control interface the controller acts as.
pub const CONTROL_USER: &str = "gabion-controller";
/// The file of the state directory that holds that user's secret.
const SECRET_FILE: &str = "control-secret";
/// How many times, a second apart, a call that cannot enter the node's
/// application tries again before the fallback of its entry is carried
/// out. A call cannot enter while the controller is not connected to
/// Asterisk; this is how long whoever runs the node has to start the
/// controller again before callers are turned to the fallback.
const ENTRY_TRIES: u32 = 5;
/// The dialplan context calls from telephone operators arrive in. Nothing
/// in it leads back out to the network.
pub const NETWORK_CONTEXT: &str = "gabion-from-network";
/// The label of the step of an entry where its fallback begins.
pub const FALLBACK_LABEL: &str = "fallback";
const FALLBACK_STEP: &str = "(fallback)";

/// Files Asterisk looks for and the node has nothing to say in. They exist
/// so that Asterisk finds them: a missing optional file is reported as an
/// error on every start, and a log that opens with expected errors hides
/// the one that matters.
const EMPTY_FILES: &[&str] = &[
    "acl.conf",
    "ccss.conf",
    "cdr.conf",
    "cel.conf",
    "chan_websocket.conf",
    "features.conf",
    "pjproject.conf",
    "stasis.conf",
    "udptl.conf",
];

/// What an Asterisk tree says about itself in its build record.
pub struct Tree {
    /// The version of Asterisk.
    pub version: String,
    /// The modules that were built — exactly the ones to load.
    modules: Vec<String>,
}

impl Tree {
    /// Read the build record of the tree the controller is paired with.
    ///
    /// # Errors
    ///
    /// A tree without the record, or one whose record names no version or
    /// no modules, is not driven on a guess. Neither is a tree of a version
    /// the controller does not know.
    pub fn read(tree: &Path) -> Result<Self, String> {
        let path = tree.join("BUILD-INFO.txt");
        let record =
            fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        let version = record
            .lines()
            .find_map(|line| line.strip_prefix("asterisk-version:"))
            .map(str::trim)
            .ok_or_else(|| format!("{} names no Asterisk version", path.display()))?
            .to_owned();
        if !KNOWN_ASTERISK_VERSIONS.contains(&version.as_str()) {
            return Err(format!(
                "{} is Asterisk {version}; this controller drives only {}",
                tree.display(),
                KNOWN_ASTERISK_VERSIONS.join(", "),
            ));
        }
        let modules: Vec<String> = record
            .lines()
            .skip_while(|line| line.trim() != "modules:")
            .skip(1)
            .take_while(|line| line.starts_with(' '))
            .map(|line| line.trim().to_owned())
            .collect();
        if modules.is_empty() {
            return Err(format!("{} lists no modules", path.display()));
        }
        Ok(Self { version, modules })
    }
}

/// The name, in Asterisk's configuration, of everything that belongs to one
/// operator.
///
/// An operator's identifier is the application's and may be any text; a
/// name in Asterisk's configuration may not. So the name is made from a
/// digest of the identifier: safe whatever the identifier is, and the same
/// for as long as the identifier is.
pub fn operator_name(operator: &Operator) -> String {
    let mut name = String::from("gabion-operator-");
    for byte in Sha256::digest(operator.id.as_bytes()).iter().take(6) {
        // Writing into a `String` cannot fail.
        let _ = write!(name, "{byte:02x}");
    }
    name
}

/// The secret of the control user: what the controller presents to
/// Asterisk's control interface.
///
/// It is made once for a state directory and kept there. Asterisk reads it
/// from its configuration when it starts, and a controller that is started
/// again next to a running Asterisk must present the same one. Nobody but
/// the owner of the state directory can read it.
///
/// # Errors
///
/// The state directory cannot be written or the secret cannot be read.
pub fn control_secret(state: &Path) -> Result<String, String> {
    let path = state.join(SECRET_FILE);
    let failed = |error: std::io::Error| format!("{}: {error}", path.display());
    match fs::read_to_string(&path) {
        Ok(secret) if !secret.trim().is_empty() => return Ok(secret.trim().to_owned()),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(failed(error)),
    }
    fs::create_dir_all(state).map_err(failed)?;
    let secret = uuid::Uuid::new_v4().simple().to_string();
    fs::write(&path, &secret).map_err(failed)?;
    owner_only(&path, 0o600).map_err(failed)?;
    Ok(secret)
}

/// Leave a file or directory readable by its owner alone.
fn owner_only(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

/// Write Asterisk's whole configuration and prepare the directories it
/// writes to. Returns the main file — what Asterisk is started with.
///
/// `secret` is the password of the control user ([`control_secret`]).
///
/// # Errors
///
/// A directory or file that cannot be written, or settings that need
/// something the launch did not give, is an error with the reason.
pub fn write(
    config: &Config,
    tree: &Tree,
    settings: &Settings,
    secret: &str,
) -> Result<PathBuf, String> {
    let files = files(config, tree, settings, secret)?;

    let etc = config.state.join("etc");
    let failed = |path: &Path, error: std::io::Error| format!("{}: {error}", path.display());
    // Nothing of an earlier configuration survives. Only this directory is
    // emptied: the controller may be started next to an Asterisk that is
    // running, and everything else here is that Asterisk's.
    match fs::remove_dir_all(&etc) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(failed(&etc, error)),
    }
    // Asterisk looks for keys in a `keys` directory inside its key directory.
    for directory in ["etc", "db", "keys/keys", "spool", "run", "log"] {
        let directory = config.state.join(directory);
        fs::create_dir_all(&directory).map_err(|error| failed(&directory, error))?;
    }
    // The configuration carries the secret of the control user and the
    // passwords of the operators.
    owner_only(&etc, 0o700).map_err(|error| failed(&etc, error))?;
    for (name, content) in files {
        let path = etc.join(name);
        fs::write(&path, content).map_err(|error| failed(&path, error))?;
    }
    Ok(etc.join("asterisk.conf"))
}

/// The files that follow from the node's settings, and the module of
/// Asterisk that reads each. These are what changes when the settings do,
/// and Asterisk takes them in without dropping a call when the module is
/// reloaded. Everything else follows from the launch alone.
pub const FROM_SETTINGS: &[(&str, &str)] = &[
    ("extensions.conf", "pbx_config.so"),
    ("pjsip.conf", "res_pjsip.so"),
];

/// The content of the files that follow from settings ([`FROM_SETTINGS`]),
/// in the same order.
///
/// # Errors
///
/// Settings that need something the launch did not give.
pub fn from_settings(config: &Config, settings: &Settings) -> Result<Vec<String>, String> {
    Ok(vec![
        dialplan(config, settings),
        operators(config, settings)?,
    ])
}

/// The current content of a file of the configuration; empty when there is
/// none.
///
/// # Errors
///
/// The file exists and cannot be read.
pub fn current(config: &Config, name: &str) -> Result<String, String> {
    let path = config.state.join("etc").join(name);
    match fs::read_to_string(&path) {
        Ok(content) => Ok(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Replace one file of the configuration, whole or not at all: Asterisk
/// may read it at any moment, and never finds it half written.
///
/// # Errors
///
/// The file cannot be written.
pub fn replace(config: &Config, name: &str, content: &str) -> Result<(), String> {
    let etc = config.state.join("etc");
    let path = etc.join(name);
    let written = etc.join(format!(".{name}.new"));
    let failed = |error: std::io::Error| format!("{}: {error}", path.display());
    fs::write(&written, content).map_err(failed)?;
    fs::rename(&written, &path).map_err(failed)
}

/// Every configuration file, by name.
fn files(
    config: &Config,
    tree: &Tree,
    settings: &Settings,
    secret: &str,
) -> Result<Vec<(&'static str, String)>, String> {
    let mut files = vec![
        ("asterisk.conf", directories(config)),
        ("modules.conf", modules(tree)),
        // Everything goes to the console: whoever started Asterisk owns its
        // output; Asterisk keeps no log file of its own.
        (
            "logger.conf",
            "[general]\n[logfiles]\nconsole => notice,warning,error\n".to_owned(),
        ),
        (
            "http.conf",
            format!(
                "[general]\nenabled = yes\nbindaddr = {}\nbindport = {}\n",
                config.asterisk_http.ip(),
                config.asterisk_http.port(),
            ),
        ),
        (
            "rtp.conf",
            format!(
                "[general]\nrtpstart = {}\nrtpend = {}\n",
                config.audio_ports.0, config.audio_ports.1,
            ),
        ),
        // The management interface is not used; said explicitly, so that its
        // absence is a setting and not a missing file.
        ("manager.conf", "[general]\nenabled = no\n".to_owned()),
        // Tones a caller hears. One country is required for Asterisk to have any.
        (
            "indications.conf",
            "[general]\ncountry = us\n\n[us]\ndescription = United States / North America\n\
             ringcadence = 2000,4000\ndial = 350+440\nbusy = 480+620/500,0/500\n\
             ring = 440+480/2000,0/4000\ncongestion = 480+620/250,0/250\n"
                .to_owned(),
        ),
        ("ari.conf", control_interface(secret)),
        ("websocket_client.conf", connections(config)),
        ("extensions.conf", dialplan(config, settings)),
        ("pjsip.conf", operators(config, settings)?),
    ];
    files.extend(EMPTY_FILES.iter().map(|name| (*name, String::new())));
    Ok(files)
}

/// No path is compiled into an Asterisk tree: every directory is named here.
/// The tree is only read; everything Asterisk writes goes under the state
/// directory.
fn directories(config: &Config) -> String {
    let tree = &config.asterisk_tree;
    let state = &config.state;
    let lib = tree.join("var/lib/asterisk");
    format!(
        "[directories]\nastetcdir => {}\nastmoddir => {}\nastvarlibdir => {lib}\n\
         astdatadir => {lib}\nastagidir => {}\nastsbindir => {}\nastdbdir => {}\n\
         astkeydir => {}\nastspooldir => {}\nastrundir => {}\nastlogdir => {}\n",
        state.join("etc").display(),
        tree.join("lib/asterisk/modules").display(),
        lib.join("agi-bin").display(),
        tree.join("sbin").display(),
        state.join("db").display(),
        state.join("keys").display(),
        state.join("spool").display(),
        state.join("run").display(),
        state.join("log").display(),
        lib = lib.display(),
    )
}

/// Exactly the modules the build put into the tree, each one required: a
/// module that fails to load stops Asterisk instead of leaving it running
/// without a part.
fn modules(tree: &Tree) -> String {
    let mut text = String::from("[modules]\nautoload = no\n");
    for module in &tree.modules {
        let _ = writeln!(text, "require = {module}");
    }
    text
}

/// The control interface: its one user, whom the controller acts as.
fn control_interface(secret: &str) -> String {
    format!(
        "[general]\nenabled = yes\n\n\
         [{CONTROL_USER}]\ntype = user\nread_only = no\npassword = {}\n",
        value(secret),
    )
}

/// Asterisk's connections to the controller: one for each media channel,
/// opened when the channel is created. For control it is the controller
/// that connects to Asterisk.
fn connections(config: &Config) -> String {
    format!(
        "[{media}]\ntype = websocket_client\nuri = ws://{listen}{path}\nprotocols = {protocol}\n\
         connection_type = per_call_config\nconnection_timeout = 500\n\
         reconnect_interval = 500\nreconnect_attempts = 4\ntls_enabled = no\n",
        listen = config.listen,
        media = media::CONNECTION,
        path = media::PATH,
        protocol = media::SUBPROTOCOL,
    )
}

/// The dialplan: how a call from the telephone network enters, and what
/// becomes of it when it cannot.
///
/// A number that is an entry leads to the node's application. A call cannot
/// enter while the controller is not connected: Asterisk then says so at
/// once, and the call tries again a second later, [`ENTRY_TRIES`] times in
/// all. After that Asterisk carries out the fallback of the entry by
/// itself — the one thing the node does for a caller with no controller.
///
/// The only rule here that dials is a fallback's transfer, to the one
/// number the settings give it, on the line they give it. Nothing a caller
/// sends chooses where a call goes.
fn dialplan(config: &Config, settings: &Settings) -> String {
    let mut text = format!("[{NETWORK_CONTEXT}]\n");
    for entry in &settings.entries {
        match &entry.key {
            EntryKey::DialedNumber(number) => {
                let number = number.as_str();
                let mut steps = vec![
                    format!("Stasis({APPLICATION},dialed_number,{number})"),
                    r#"GotoIf($["${STASISSTATUS}" = "SUCCESS"]?done)"#.to_owned(),
                    "Set(GABION_TRIES=$[${GABION_TRIES}+1])".to_owned(),
                    format!("GotoIf($[${{GABION_TRIES}} >= {ENTRY_TRIES}]?fallback)"),
                    "Wait(1)".to_owned(),
                    "Goto(enter)".to_owned(),
                    "NoOp(the fallback of the entry)".to_owned(),
                ];
                steps.extend(fallback(config, settings, &entry.fallback));
                steps.push("Hangup()".to_owned());

                let _ = writeln!(text, "exten = {number},1,Set(GABION_TRIES=0)");
                let last = steps.len() - 1;
                for (position, step) in steps.iter().enumerate() {
                    // The steps the others jump to are the ones with names.
                    let name = match position {
                        0 => "(enter)",
                        6 => FALLBACK_STEP,
                        _ if position == last => "(done)",
                        _ => "",
                    };
                    let _ = writeln!(text, " same = n{name},{step}");
                }
            }
            // These enter through the controller itself, not the dialplan.
            EntryKey::UserEndpoints | EntryKey::WebPasses => {}
        }
    }
    text
}

/// The dialplan steps that carry out a fallback — by Asterisk alone when
/// the controller is away, and for the controller when the application is:
/// it sends a caller here rather than play the message itself, so how a
/// message is played is said in one place.
fn fallback(config: &Config, settings: &Settings, fallback: &Fallback) -> Vec<String> {
    match fallback {
        Fallback::Transfer {
            number,
            line,
            answer_limit_ms,
        } => match destination::judge(settings, line, number) {
            // The caller is connected to the number, which is shown the
            // line's own. Asterisk counts the wait in whole seconds.
            Ok(route) => vec![
                format!("Set(CALLERID(num)={})", route.line.number.as_str()),
                format!(
                    "Dial(PJSIP/{}@{},{})",
                    number.as_str(),
                    operator_name(route.operator),
                    answer_limit_ms.div_ceil(1000).max(1),
                ),
            ],
            // Checked settings have no such transfer; one that slipped
            // through is not dialled.
            Err(_) => Vec::new(),
        },
        // The prompt is played from its file; playing answers the caller.
        Fallback::Message { prompt } => settings
            .prompts
            .iter()
            .find(|named| named.id == *prompt)
            .map(|named| {
                vec![format!(
                    "Playback({})",
                    prompts::playable(&config.state, named).display()
                )]
            })
            // Checked settings name only prompts they have.
            .unwrap_or_default(),
    }
}

/// The telephone operators: where each one's trunk is, what the node
/// presents to it, and where its calls are accepted from.
fn operators(config: &Config, settings: &Settings) -> Result<String, String> {
    if settings.operators.is_empty() {
        // Without operators Asterisk listens for none.
        return Ok(String::new());
    }
    let sip = config.sip.ok_or(
        "the settings name a telephone operator, and the controller was not told where \
         Asterisk listens for operators (--sip)",
    )?;

    // A request is matched to an operator by the address it came from and by
    // nothing it says about itself; one that matches no operator is refused.
    let mut text = String::from(
        "[global]\ntype = global\nendpoint_identifier_order = ip\nuser_agent = Gabion node\n\n",
    );
    for transport in [SipTransport::Udp, SipTransport::Tcp] {
        if settings
            .operators
            .iter()
            .any(|operator| operator.transport == transport)
        {
            let _ = write!(
                text,
                "[{}]\ntype = transport\nprotocol = {transport}\nbind = {sip}\n",
                transport_name(transport),
            );
            if let Some(public) = config.sip_public {
                let _ = write!(
                    text,
                    "external_signaling_address = {public}\nexternal_media_address = {public}\n",
                );
            }
            text.push('\n');
        }
    }

    let mut names = std::collections::HashSet::new();
    for operator in &settings.operators {
        let name = operator_name(operator);
        if !names.insert(name.clone()) {
            return Err(format!(
                "operator {:?} cannot be told apart from another operator in Asterisk's \
                 configuration; give it another identifier",
                operator.id.as_str()
            ));
        }
        operator_sections(&mut text, &name, operator);
    }
    Ok(text)
}

fn transport_name(transport: SipTransport) -> String {
    format!("gabion-transport-{transport}")
}

fn operator_sections(text: &mut String, name: &str, operator: &Operator) {
    let host = operator.host.as_str();
    // An IPv6 address is written in brackets inside an address with a port.
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let _ = write!(
        text,
        "[{name}]\ntype = endpoint\ntransport = {}\ncontext = {NETWORK_CONTEXT}\n\
         disallow = all\nallow = ulaw,alaw\naors = {name}\ndirect_media = no\n\
         rtp_symmetric = yes\nforce_rport = yes\n",
        transport_name(operator.transport),
    );
    if operator.credentials.is_some() {
        let _ = writeln!(text, "outbound_auth = {name}");
    }
    let _ = write!(
        text,
        "\n[{name}]\ntype = aor\ncontact = sip:{host}:{}",
        operator.port
    );
    if operator.transport == SipTransport::Tcp {
        text.push_str("\\;transport=tcp");
    }
    text.push_str("\n\n");
    if let Some(credentials) = &operator.credentials {
        let _ = write!(
            text,
            "[{name}]\ntype = auth\nauth_type = userpass\nusername = {}\npassword = {}\n\n",
            value(credentials.username.as_str()),
            value(credentials.password.as_str()),
        );
    }
    // An operator with no source networks is one the node only calls out
    // through: nothing is accepted from it, so nothing identifies it.
    if !operator.source_networks.is_empty() {
        let networks: Vec<&str> = operator
            .source_networks
            .iter()
            .map(|network| network.as_str())
            .collect();
        let _ = write!(
            text,
            "[{name}]\ntype = identify\nendpoint = {name}\nmatch = {}\n\n",
            networks.join(","),
        );
    }
}

/// A free-form value as it is written into Asterisk's configuration. A
/// semicolon would begin a comment, so it is written escaped. Everything
/// else that the format cannot carry was refused when the settings were
/// checked ([`crate::settings::check`]).
fn value(text: &str) -> String {
    text.replace(';', "\\;")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use node_protocol::messages::Settings;
    use serde_json::json;

    use super::{Tree, files, operator_name};
    use crate::config::Config;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn config(sip: Option<&str>) -> Result<Config, Box<dyn std::error::Error>> {
        Ok(Config {
            node: "node".into(),
            listen: "127.0.0.1:4000".parse()?,
            application_url: "ws://127.0.0.1:4001/".into(),
            asterisk_tree: PathBuf::from("/tree"),
            state: PathBuf::from("/state"),
            asterisk_http: "127.0.0.1:4002".parse()?,
            audio_ports: (4100, 4199),
            sip: sip.map(str::parse).transpose()?,
            sip_public: None,
        })
    }

    fn tree() -> Tree {
        Tree {
            version: "22.11.0".into(),
            modules: vec!["res_ari.so".into(), "chan_pjsip.so".into()],
        }
    }

    fn file(files: &[(&'static str, String)], name: &str) -> String {
        files
            .iter()
            .find(|(found, _)| *found == name)
            .map(|(_, content)| content.clone())
            .unwrap_or_default()
    }

    fn settings() -> Result<Settings, Box<dyn std::error::Error>> {
        Ok(node_protocol::decode_value(json!({
            "operators": [{
                "id": "an operator; [named] oddly", "host": "2001:db8::1", "port": 5060,
                "transport": "tcp",
                "credentials": { "username": "node", "password": "a;b" },
                "source_networks": ["192.0.2.0/24", "198.51.100.7/32"],
                "countries": ["US"]
            }],
            "lines": [],
            "entries": [
                { "key": { "type": "dialed_number", "number": "+19715870050" },
                  "fallback": { "type": "message", "prompt": "closed" } },
                { "key": { "type": "web_passes" },
                  "fallback": { "type": "message", "prompt": "closed" } }
            ],
            "prompts": [{ "id": "closed", "sha256": "0".repeat(64) }]
        }))?)
    }

    /// What `Tree::read` says of a tree with this build record.
    fn read_tree_with(record: &str) -> Result<Result<Tree, String>, Box<dyn std::error::Error>> {
        let directory = std::env::temp_dir().join(format!("gabion-tree-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory)?;
        std::fs::write(directory.join("BUILD-INFO.txt"), record)?;
        let read = Tree::read(&directory);
        std::fs::remove_dir_all(&directory)?;
        Ok(read)
    }

    #[test]
    fn only_an_asterisk_the_controller_knows_is_driven() -> TestResult {
        let known = read_tree_with("asterisk-version: 22.11.0\nmodules:\n  res_ari.so\n")??;
        assert_eq!(known.modules, ["res_ari.so"]);

        for (record, expected) in [
            (
                "asterisk-version: 23.0.0\nmodules:\n  res_ari.so\n",
                "drives only 22.11.0",
            ),
            ("modules:\n  res_ari.so\n", "names no Asterisk version"),
            ("asterisk-version: 22.11.0\nmodules:\n", "lists no modules"),
        ] {
            let problem = read_tree_with(record)?.err().unwrap_or_default();
            assert!(problem.contains(expected), "{record:?}: {problem:?}");
        }
        Ok(())
    }

    #[test]
    fn a_node_without_settings_listens_for_no_operator_and_lets_no_call_in() -> TestResult {
        let nothing: Settings = node_protocol::decode_value(
            json!({ "operators": [], "lines": [], "entries": [], "prompts": [] }),
        )?;
        let files = files(&config(None)?, &tree(), &nothing, "secret")?;
        assert_eq!(file(&files, "pjsip.conf"), "");
        assert_eq!(file(&files, "extensions.conf"), "[gabion-from-network]\n");
        assert_eq!(
            file(&files, "modules.conf"),
            "[modules]\nautoload = no\nrequire = res_ari.so\nrequire = chan_pjsip.so\n"
        );
        Ok(())
    }

    #[test]
    fn an_operator_is_known_by_where_its_calls_come_from_and_by_nothing_it_says() -> TestResult {
        let files = files(
            &config(Some("203.0.113.5:5060"))?,
            &tree(),
            &settings()?,
            "secret",
        )?;
        let operators = file(&files, "pjsip.conf");
        assert!(operators.contains("endpoint_identifier_order = ip\n"));
        assert!(operators.contains("match = 192.0.2.0/24,198.51.100.7/32\n"));
        assert!(operators.contains("bind = 203.0.113.5:5060\n"));
        assert!(operators.contains("contact = sip:[2001:db8::1]:5060\\;transport=tcp\n"));
        // A semicolon in a value would otherwise begin a comment.
        assert!(operators.contains("password = a\\;b\n"));
        // The operator's own identifier appears nowhere: it is not a name
        // Asterisk's configuration could carry.
        assert!(!operators.contains("oddly"));
        Ok(())
    }

    #[test]
    fn an_entry_tries_the_application_and_falls_back_when_it_cannot_enter() -> TestResult {
        let playback = format!(" same = n,Playback(/state/prompts/{})", "0".repeat(64));
        let files = files(
            &config(Some("203.0.113.5:5060"))?,
            &tree(),
            &settings()?,
            "secret",
        )?;
        assert_eq!(
            file(&files, "extensions.conf"),
            [
                "[gabion-from-network]",
                "exten = +19715870050,1,Set(GABION_TRIES=0)",
                " same = n(enter),Stasis(gabion,dialed_number,+19715870050)",
                r#" same = n,GotoIf($["${STASISSTATUS}" = "SUCCESS"]?done)"#,
                " same = n,Set(GABION_TRIES=$[${GABION_TRIES}+1])",
                " same = n,GotoIf($[${GABION_TRIES} >= 5]?fallback)",
                " same = n,Wait(1)",
                " same = n,Goto(enter)",
                " same = n(fallback),NoOp(the fallback of the entry)",
                playback.as_str(),
                " same = n(done),Hangup()",
                "",
            ]
            .join("\n")
        );
        Ok(())
    }

    #[test]
    fn a_fallback_that_transfers_dials_its_one_number_showing_the_lines() -> TestResult {
        let settings: Settings = node_protocol::decode_value(json!({
            "operators": [{
                "id": "carrier", "host": "192.0.2.1", "port": 5060, "transport": "udp",
                "source_networks": [], "countries": ["US"]
            }],
            "lines": [{
                "id": "main", "number": "+19715870050", "operator": "carrier",
                "allowed_countries": ["US"], "concurrent_outbound_limit": 1
            }],
            "entries": [{
                "key": { "type": "dialed_number", "number": "+19715870050" },
                "fallback": { "type": "transfer", "number": "+15035550100", "line": "main",
                              "answer_limit_ms": 20500 }
            }],
            "prompts": []
        }))?;
        let operator = settings.operators.first().ok_or("no operator")?;
        let files = files(
            &config(Some("203.0.113.5:5060"))?,
            &tree(),
            &settings,
            "secret",
        )?;
        let dialplan = file(&files, "extensions.conf");
        let expected = format!(
            " same = n(fallback),NoOp(the fallback of the entry)\n \
             same = n,Set(CALLERID(num)=+19715870050)\n \
             same = n,Dial(PJSIP/+15035550100@{},21)\n \
             same = n(done),Hangup()\n",
            operator_name(operator)
        );
        assert!(dialplan.ends_with(&expected), "{dialplan}");
        Ok(())
    }

    #[test]
    fn settings_with_an_operator_need_a_place_to_listen_for_it() -> TestResult {
        let problem = files(&config(None)?, &tree(), &settings()?, "secret")
            .err()
            .unwrap_or_default();
        assert!(problem.contains("--sip"), "{problem:?}");
        Ok(())
    }

    #[test]
    fn an_operators_name_is_the_same_for_as_long_as_its_identifier_is() -> TestResult {
        let settings = settings()?;
        let operator = settings.operators.first().ok_or("no operator")?;
        assert_eq!(operator_name(operator), operator_name(&operator.clone()));
        assert!(
            operator_name(operator)
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        );
        Ok(())
    }
}
