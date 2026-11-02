// crates/asterisk-controller/tests/common/mod.rs

//! The ports the checks against Asterisk give out.
//!
//! A port the system offers as free (a bind to port 0) is an ephemeral one,
//! from the range the system also takes the local ports of outgoing
//! connections from. Checked free and let go, such a port can be taken by
//! any connection on the machine before the process it was meant for binds
//! it — the controller then refused to start with "Address already in use".
//! So ports are taken from a band the system never hands out on its own,
//! below its ephemeral range, and the test refuses to run on a machine
//! whose ephemeral range reaches into that band.
//!
//! None is given out twice and none inside a range of audio ports: a port
//! inside the node's audio ports would receive its RTCP. Every port is
//! checked over UDP too — SIP and audio are UDP.

use std::{error::Error, ops::RangeInclusive};

use tokio::net::{TcpListener, UdpSocket};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// Where ports are taken from: below the ephemeral range of Linux (32768
/// and up by default) and of macOS (49152 and up by default).
const BAND: RangeInclusive<u16> = 20_000..=32_767;

/// The ports the test gives out.
pub struct Ports {
    taken: Vec<RangeInclusive<u16>>,
    /// Where the next search begins: apart for every test process, so two
    /// checks of the machine running at once rarely ask for the same port.
    next: u16,
}

impl Ports {
    /// A fresh account, on a machine whose ephemeral ports stay out of
    /// [`BAND`].
    pub fn new() -> TestResult<Self> {
        let ephemeral = ephemeral_ports()?;
        if ephemeral.start() <= BAND.end() && BAND.start() <= ephemeral.end() {
            return Err(format!(
                "the system hands out ports {}-{} on its own, inside {}-{} the test takes its \
                 ports from",
                ephemeral.start(),
                ephemeral.end(),
                BAND.start(),
                BAND.end()
            )
            .into());
        }
        let size = u32::from(BAND.end() - BAND.start()) + 1;
        let offset = std::process::id().wrapping_mul(7919) % size;
        Ok(Self {
            taken: Vec::new(),
            next: BAND.start() + u16::try_from(offset)?,
        })
    }

    fn is_taken(&self, port: u16) -> bool {
        self.taken.iter().any(|range| range.contains(&port))
    }

    /// Whether nothing holds the port over TCP or UDP.
    async fn is_free(port: u16) -> bool {
        TcpListener::bind(("127.0.0.1", port)).await.is_ok()
            && UdpSocket::bind(("127.0.0.1", port)).await.is_ok()
    }

    /// The candidates in order, once round the band from where the last
    /// search stopped.
    fn candidates(&self) -> impl Iterator<Item = u16> + use<> {
        let from = self.next;
        (from..=*BAND.end()).chain(*BAND.start()..from)
    }

    /// One port, free over TCP and UDP and not given out before.
    pub async fn one(&mut self) -> TestResult<u16> {
        for port in self.candidates() {
            if !self.is_taken(port) && Self::is_free(port).await {
                self.taken.push(port..=port);
                self.next = port
                    .checked_add(1)
                    .filter(|next| BAND.contains(next))
                    .unwrap_or(*BAND.start());
                return Ok(port);
            }
        }
        Err(format!("no free port in {}-{}", BAND.start(), BAND.end()).into())
    }

    /// The first of `count` consecutive ports, beginning on an even one
    /// (audio goes in pairs), all free and none given out before.
    pub async fn range(&mut self, count: u16) -> TestResult<u16> {
        'candidates: for first in self.candidates().filter(|port| port % 2 == 0) {
            let Some(last) = first
                .checked_add(count - 1)
                .filter(|last| BAND.contains(last))
            else {
                continue;
            };
            for port in first..=last {
                if self.is_taken(port) || !Self::is_free(port).await {
                    continue 'candidates;
                }
            }
            self.taken.push(first..=last);
            self.next = last
                .checked_add(1)
                .filter(|next| BAND.contains(next))
                .unwrap_or(*BAND.start());
            return Ok(first);
        }
        Err(format!(
            "no {count} free ports in a row in {}-{}",
            BAND.start(),
            BAND.end()
        )
        .into())
    }
}

/// The range the system takes ports of outgoing connections and of binds
/// to port 0 from.
fn ephemeral_ports() -> TestResult<RangeInclusive<u16>> {
    let text = if cfg!(target_os = "linux") {
        std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")?
    } else {
        let output = std::process::Command::new("sysctl")
            .args([
                "-n",
                "net.inet.ip.portrange.first",
                "net.inet.ip.portrange.last",
            ])
            .output()?;
        if !output.status.success() {
            return Err(format!(
                "sysctl could not say the ephemeral ports: {}",
                output.status
            )
            .into());
        }
        String::from_utf8(output.stdout)?
    };
    let mut bounds = text.split_whitespace().map(str::parse::<u16>);
    match (bounds.next(), bounds.next()) {
        (Some(Ok(first)), Some(Ok(last))) => Ok(first..=last),
        _ => Err(format!("the ephemeral ports could not be read from {text:?}").into()),
    }
}
