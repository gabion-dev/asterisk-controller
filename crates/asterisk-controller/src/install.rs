// crates/asterisk-controller/src/install.rs

//! Installing a telephony node on a Linux server as a systemd service.
//!
//! ```text
//! asterisk-controller install --node <name> --application <wss://…> \
//!     --public <address> [--sip-tls-port <port>] [--audio-ports <first>-<last>] \
//!     [--secret-from-stdin]
//! ```
//!
//! Run as root on the server, from the binary of the release to install. It
//! puts in place, each under a directory of its own version, this binary and
//! the Asterisk tree it pins ([`crate::tree`], the machine judged first); a
//! system user the node runs as; the node's state directory, readable by
//! that user alone, with the node's secret — read from stdin, never from the
//! command line, which every user of the machine sees — and a certificate
//! for SIP over TLS, made once and kept; and one systemd service that runs
//! the launcher ([`crate::launch`]). Run again from another release, it is the
//! update: the new version is put beside the old one and the service is
//! pointed at it and restarted.
//!
//! The service follows the launcher's rule: systemd starts the launcher
//! again when it dies, but not when it gives the node up (status 2) or
//! refuses its arguments (status 1) — the launcher's ladders are the only
//! ones; a stop signals the launcher alone, which stops Asterisk first.

use std::{
    fmt::Write as _,
    io::{BufRead as _, IsTerminal as _},
    net::IpAddr,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

use crate::{asterisk_release, config::Config, launch::GIVEN_UP, state_files, tree};

/// The name of the service, of the system user it runs as, and of its
/// directories.
const NAME: &str = "gabion-telephony";

/// The loopback port Asterisk's HTTP server listens on.
const ASTERISK_HTTP: &str = "127.0.0.1:8088";

/// The loopback port the controller listens on for Asterisk's audio
/// connections.
const LISTEN: &str = "127.0.0.1:8090";

/// The port SIP over TLS is offered on unless another is named.
const SIP_TLS_PORT: u16 = 5061;

/// The audio ports unless others are named: 500 calls' worth.
const AUDIO_PORTS: &str = "10000-10999";

/// The launcher's ladders on a telephony server: a half that keeps dying is
/// given up after five restarts in a row, three seconds apart; one that ran
/// for five minutes starts its count over.
const RESTARTS: &str = "5";
const STABLE_AFTER_MS: &str = "300000";
const RESTART_DELAY_MS: &str = "3000";

/// Where a node lives on the server.
struct Layout {
    /// Versions of the controller and of the Asterisk tree, each in a
    /// directory of its own.
    opt: PathBuf,
    /// The node's state directory.
    state: PathBuf,
    /// The directory systemd makes for the service while it runs.
    run: PathBuf,
    /// The service's unit file.
    unit: PathBuf,
}

impl Layout {
    fn system() -> Self {
        Self {
            opt: PathBuf::from("/opt").join(NAME),
            state: PathBuf::from("/var/lib").join(NAME),
            run: PathBuf::from("/run").join(NAME),
            unit: PathBuf::from("/etc/systemd/system").join(format!("{NAME}.service")),
        }
    }

    fn controller(&self) -> PathBuf {
        self.opt
            .join("controller")
            .join(env!("CARGO_PKG_VERSION"))
            .join("asterisk-controller")
    }

    fn tree(&self) -> PathBuf {
        self.opt
            .join("asterisk-server")
            .join(asterisk_release::RELEASE)
    }

    fn certificate(&self) -> PathBuf {
        self.state.join("tls").join("node.crt")
    }

    fn key(&self) -> PathBuf {
        self.state.join("tls").join("node.key")
    }
}

/// What the person installing the node says about it.
#[derive(Debug, PartialEq, Eq)]
struct Node {
    name: String,
    application: String,
    public: IpAddr,
    sip_tls_port: u16,
    audio_ports: String,
    secret_from_stdin: bool,
}

const USAGE: &str = "usage: asterisk-controller install --node <name> --application <wss://host[:port][/path]> \
     --public <address> [--sip-tls-port <port>] [--audio-ports <first>-<last>] \
     [--secret-from-stdin]";

impl Node {
    fn from_arguments(mut arguments: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut name = None;
        let mut application = None;
        let mut public = None;
        let mut sip_tls_port = SIP_TLS_PORT;
        let mut audio_ports = AUDIO_PORTS.to_owned();
        let mut secret_from_stdin = false;
        while let Some(argument) = arguments.next() {
            if argument == "--secret-from-stdin" {
                secret_from_stdin = true;
                continue;
            }
            let value = arguments
                .next()
                .ok_or_else(|| format!("{argument} needs a value\n{USAGE}"))?;
            match argument.as_str() {
                "--node" => name = Some(value),
                "--application" => application = Some(value),
                "--public" => {
                    public = Some(
                        value
                            .parse()
                            .map_err(|error| format!("--public {value}: {error}\n{USAGE}"))?,
                    );
                }
                "--sip-tls-port" => {
                    sip_tls_port = value
                        .parse()
                        .map_err(|error| format!("--sip-tls-port {value}: {error}\n{USAGE}"))?;
                }
                "--audio-ports" => audio_ports = value,
                _ => return Err(format!("unknown argument {argument}\n{USAGE}")),
            }
        }
        let missing = |name: &str| format!("{name} is missing\n{USAGE}");
        Ok(Self {
            name: name.ok_or_else(|| missing("--node"))?,
            application: application.ok_or_else(|| missing("--application"))?,
            public: public.ok_or_else(|| missing("--public"))?,
            sip_tls_port,
            audio_ports,
            secret_from_stdin,
        })
    }

    /// The controller's arguments of this node.
    fn controller_arguments(&self, layout: &Layout) -> Vec<String> {
        let path = |path: &Path| path.display().to_string();
        let unspecified = if self.public.is_ipv6() {
            "[::]"
        } else {
            "0.0.0.0"
        };
        vec![
            "--node".into(),
            self.name.clone(),
            "--listen".into(),
            LISTEN.into(),
            "--application".into(),
            self.application.clone(),
            "--asterisk-tree".into(),
            path(&layout.tree()),
            "--state".into(),
            path(&layout.state),
            "--run".into(),
            path(&layout.run),
            "--asterisk-http".into(),
            ASTERISK_HTTP.into(),
            "--audio-ports".into(),
            self.audio_ports.clone(),
            "--sip-public".into(),
            self.public.to_string(),
            "--sip-tls".into(),
            format!("{unspecified}:{}", self.sip_tls_port),
            "--sip-tls-certificate".into(),
            path(&layout.certificate()),
            "--sip-tls-key".into(),
            path(&layout.key()),
        ]
    }

    /// The service's unit file.
    fn unit(&self, layout: &Layout) -> String {
        let mut start = format!(
            "{} launch --restarts {RESTARTS} --stable-after {STABLE_AFTER_MS} \
             --restart-delay {RESTART_DELAY_MS}",
            layout.controller().display()
        );
        for argument in self.controller_arguments(layout) {
            let _ = write!(start, " {}", systemd_escaped(&argument));
        }
        format!(
            "# {unit}\n\
             # Written by asterisk-controller install; it is written again on every install.\n\
             \n\
             [Unit]\n\
             Description=Gabion telephony node {node}\n\
             Wants=network-online.target\n\
             After=network-online.target\n\
             \n\
             [Service]\n\
             Type=exec\n\
             User={NAME}\n\
             Group={NAME}\n\
             ExecStart={start}\n\
             # The node's certificate was renewed: the controller has Asterisk take it in.\n\
             ExecReload=/bin/kill -HUP $MAINPID\n\
             # The launcher keeps both halves running by its own ladders. systemd starts it\n\
             # again only when it dies: not when it gave the node up ({GIVEN_UP}) or refused\n\
             # its arguments (1) — the reason is in the journal.\n\
             Restart=on-failure\n\
             RestartPreventExitStatus=1 {GIVEN_UP}\n\
             RestartSec=5\n\
             # A stop signals the launcher alone: it stops Asterisk first, then the controller.\n\
             KillMode=mixed\n\
             TimeoutStopSec=120\n\
             RuntimeDirectory={NAME}\n\
             RuntimeDirectoryMode=0700\n\
             LimitNOFILE=65536\n\
             NoNewPrivileges=yes\n\
             ProtectSystem=strict\n\
             ProtectHome=yes\n\
             PrivateTmp=yes\n\
             ReadWritePaths={state}\n\
             \n\
             [Install]\n\
             WantedBy=multi-user.target\n",
            unit = layout.unit.display(),
            node = self.name,
            state = layout.state.display(),
        )
    }
}

/// An argument as a unit file's command line carries it: systemd reads `%`
/// as the start of a specifier and `$` as the start of a variable, and
/// doubling each keeps it as itself. Nothing the controller takes holds a
/// space or a quote.
fn systemd_escaped(argument: &str) -> String {
    argument.replace('%', "%%").replace('$', "$$")
}

/// Run `asterisk-controller install`.
pub async fn run(arguments: impl Iterator<Item = String>) -> ExitCode {
    match install(arguments).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(problem) => {
            eprintln!("asterisk-controller install: {problem}");
            ExitCode::FAILURE
        }
    }
}

async fn install(arguments: impl Iterator<Item = String>) -> Result<(), String> {
    let node = Node::from_arguments(arguments)?;
    let layout = Layout::system();
    // Every argument is judged before anything on the machine changes.
    Config::from_arguments(node.controller_arguments(&layout).into_iter()).map_err(|problem| {
        // The controller's refusal ends with the controller's usage; the
        // installer's is the one that applies.
        format!("{}\n{USAGE}", problem.lines().next().unwrap_or_default())
    })?;
    if !rustix::process::geteuid().is_root() {
        return Err("run it as root: it installs a system service".into());
    }
    if !Path::new("/run/systemd/system").is_dir() {
        return Err("this machine does not run systemd, which runs the node's service".into());
    }
    let secret = if node.secret_from_stdin {
        Some(secret_from_stdin()?)
    } else {
        None
    };

    say(&format!(
        "Asterisk {} into {}",
        asterisk_release::RELEASE,
        layout.tree().display()
    ));
    tree::put_in_place(&layout.tree()).await?;
    say(&format!(
        "the controller into {}",
        layout.controller().display()
    ));
    copy_this_binary(&layout.controller())?;

    let (uid, gid) = system_user()?;
    owned_directory(&layout.state, uid, gid)?;
    owned_directory(&layout.state.join("tls"), uid, gid)?;
    let secret_file = layout.state.join(crate::application::SECRET_FILE);
    match secret {
        Some(secret) => {
            owned_file(&secret_file, format!("{secret}\n").as_bytes(), uid, gid)?;
            say(&format!("the node's secret into {}", secret_file.display()));
        }
        None if secret_file.is_file() => say("the node's secret kept"),
        None => {
            return Err(format!(
                "there is no node secret in {} yet: give it with --secret-from-stdin",
                secret_file.display()
            ));
        }
    }
    if layout.certificate().is_file() && layout.key().is_file() {
        say("the node's certificate kept");
    } else {
        let (certificate, key) = certificate_for(node.public)?;
        owned_file(&layout.certificate(), certificate.as_bytes(), uid, gid)?;
        owned_file(&layout.key(), key.as_bytes(), uid, gid)?;
        say(&format!(
            "a self-signed certificate for {} into {}",
            node.public,
            layout.certificate().display()
        ));
    }

    state_files::replace(&layout.unit, node.unit(&layout).as_bytes())
        .map_err(|error| format!("{}: {error}", layout.unit.display()))?;
    std::fs::set_permissions(&layout.unit, std::fs::Permissions::from_mode(0o644))
        .map_err(|error| format!("{}: {error}", layout.unit.display()))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", NAME])?;
    systemctl(&["restart", NAME])?;
    say(&format!(
        "the service {NAME} runs; its journal: journalctl -u {NAME}"
    ));
    Ok(())
}

fn say(what: &str) {
    println!("asterisk-controller install: {what}");
}

/// The node's secret, the first line of stdin.
fn secret_from_stdin() -> Result<String, String> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        eprintln!("the node's secret:");
    }
    let mut line = String::new();
    stdin
        .lock()
        .read_line(&mut line)
        .map_err(|error| format!("the node's secret could not be read: {error}"))?;
    let secret = line.trim();
    if secret.is_empty() {
        return Err("--secret-from-stdin: stdin gave no secret".into());
    }
    Ok(secret.to_owned())
}

/// Put a copy of the running binary at `into`, whole or not at all.
fn copy_this_binary(into: &Path) -> Result<(), String> {
    let this = std::env::current_exe().map_err(|error| format!("this binary: {error}"))?;
    if this == into {
        return Ok(());
    }
    let failed = |error: &dyn std::fmt::Display| format!("{}: {error}", into.display());
    let directory = into.parent().ok_or_else(|| failed(&"no directory"))?;
    std::fs::create_dir_all(directory).map_err(|error| failed(&error))?;
    let partial = directory.join(".asterisk-controller.partial");
    std::fs::copy(&this, &partial).map_err(|error| failed(&error))?;
    std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o755))
        .map_err(|error| failed(&error))?;
    std::fs::rename(&partial, into).map_err(|error| failed(&error))
}

/// The system user the node runs as, made when there is none.
fn system_user() -> Result<(u32, u32), String> {
    if let Some(ids) = user_ids()? {
        return Ok(ids);
    }
    let shell = ["/usr/sbin/nologin", "/sbin/nologin"]
        .into_iter()
        .find(|shell| Path::new(shell).is_file())
        .unwrap_or("/bin/false");
    let home = Layout::system().state;
    run_program(
        "useradd",
        &[
            "--system",
            "--user-group",
            "--no-create-home",
            "--home-dir",
            &home.display().to_string(),
            "--shell",
            shell,
            NAME,
        ],
    )?;
    say(&format!("the system user {NAME}"));
    user_ids()?.ok_or_else(|| format!("useradd made no user {NAME}"))
}

/// The user and group ids of the node's user, if there is one.
fn user_ids() -> Result<Option<(u32, u32)>, String> {
    let output = Command::new("getent")
        .args(["passwd", NAME])
        .output()
        .map_err(|error| format!("getent: {error}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    let entry = String::from_utf8_lossy(&output.stdout);
    let mut fields = entry.trim().split(':').skip(2);
    match (
        fields.next().and_then(|uid| uid.parse().ok()),
        fields.next().and_then(|gid| gid.parse().ok()),
    ) {
        (Some(uid), Some(gid)) => Ok(Some((uid, gid))),
        _ => Err(format!("getent passwd {NAME} said {entry:?}")),
    }
}

/// A directory owned by the node's user, readable by it alone.
fn owned_directory(path: &Path, uid: u32, gid: u32) -> Result<(), String> {
    let failed = |error: &dyn std::fmt::Display| format!("{}: {error}", path.display());
    std::fs::create_dir_all(path).map_err(|error| failed(&error))?;
    std::os::unix::fs::chown(path, Some(uid), Some(gid)).map_err(|error| failed(&error))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| failed(&error))
}

/// A file owned by the node's user, readable by it alone.
fn owned_file(path: &Path, content: &[u8], uid: u32, gid: u32) -> Result<(), String> {
    let failed = |error: &dyn std::fmt::Display| format!("{}: {error}", path.display());
    state_files::replace(path, content).map_err(|error| failed(&error))?;
    std::os::unix::fs::chown(path, Some(uid), Some(gid)).map_err(|error| failed(&error))
}

/// A self-signed certificate for the node's public address, and its key, as
/// PEM. The operator the node is made for, Amazon Chime SDK Voice
/// Connector, does not check the node's certificate; it is made once and
/// kept.
fn certificate_for(public: IpAddr) -> Result<(String, String), String> {
    let made = rcgen::generate_simple_self_signed(vec![public.to_string()])
        .map_err(|error| format!("the node's certificate could not be made: {error}"))?;
    Ok((made.cert.pem(), made.signing_key.serialize_pem()))
}

fn systemctl(arguments: &[&str]) -> Result<(), String> {
    run_program("systemctl", arguments)
}

fn run_program(program: &str, arguments: &[&str]) -> Result<(), String> {
    let status = Command::new(program)
        .args(arguments)
        .status()
        .map_err(|error| format!("{program}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program} {} ended with {status}",
            arguments.join(" ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node() -> Node {
        Node {
            name: "phone-1".into(),
            application: "wss://example.com/api/telephony/node".into(),
            public: "203.0.113.7".parse().unwrap_or(IpAddr::from([0, 0, 0, 0])),
            sip_tls_port: SIP_TLS_PORT,
            audio_ports: AUDIO_PORTS.into(),
            secret_from_stdin: true,
        }
    }

    #[test]
    fn the_arguments_name_the_node_and_the_rest_is_the_installers_own() -> Result<(), String> {
        let read = Node::from_arguments(
            [
                "--node",
                "phone-1",
                "--application",
                "wss://example.com/api/telephony/node",
                "--public",
                "203.0.113.7",
                "--secret-from-stdin",
            ]
            .into_iter()
            .map(String::from),
        )?;
        assert_eq!(read, node());
        let missing = Node::from_arguments(["--node", "phone-1"].into_iter().map(String::from))
            .err()
            .unwrap_or_default();
        assert!(missing.starts_with("--application is missing"), "{missing}");
        let unknown = Node::from_arguments(["--state", "/tmp"].into_iter().map(String::from))
            .err()
            .unwrap_or_default();
        assert!(unknown.starts_with("unknown argument --state"), "{unknown}");
        Ok(())
    }

    #[test]
    fn the_controller_would_accept_what_the_service_gives_it() {
        let layout = Layout::system();
        let arguments = node().controller_arguments(&layout);
        let config = Config::from_arguments(arguments.into_iter());
        assert!(config.is_ok(), "{:?}", config.err());
    }

    #[test]
    fn the_service_restarts_the_launcher_only_when_it_dies_and_stops_it_alone() {
        let unit = node().unit(&Layout::system());
        for line in [
            "Restart=on-failure",
            "RestartPreventExitStatus=1 2",
            "KillMode=mixed",
            "ExecReload=/bin/kill -HUP $MAINPID",
            "User=gabion-telephony",
            "ReadWritePaths=/var/lib/gabion-telephony",
        ] {
            assert!(unit.lines().any(|l| l == line), "{line}\n{unit}");
        }
        let start = unit
            .lines()
            .find_map(|line| line.strip_prefix("ExecStart="))
            .unwrap_or_default();
        assert!(
            start.starts_with(&format!(
                "/opt/gabion-telephony/controller/{}/asterisk-controller launch --restarts 5 ",
                env!("CARGO_PKG_VERSION")
            )),
            "{start}"
        );
        assert!(
            start.contains(&format!(
                " --asterisk-tree /opt/gabion-telephony/asterisk-server/{} ",
                asterisk_release::RELEASE
            )),
            "{start}"
        );
        assert!(start.contains(" --sip-tls 0.0.0.0:5061 "), "{start}");
        assert!(!start.contains("secret"), "{start}");
    }

    #[test]
    fn a_percent_or_a_dollar_reaches_the_controller_as_itself() {
        let mut escaped = node();
        escaped.application = "wss://example.com/a%20b$c".into();
        let unit = escaped.unit(&Layout::system());
        assert!(
            unit.contains(" --application wss://example.com/a%%20b$$c "),
            "{unit}"
        );
    }

    #[test]
    fn the_certificate_is_made_for_the_public_address() -> Result<(), String> {
        let (certificate, key) = certificate_for("203.0.113.7".parse().map_err(|_| "address")?)?;
        assert!(certificate.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(key.starts_with("-----BEGIN PRIVATE KEY-----"));
        Ok(())
    }
}
