// crates/asterisk-controller/src/download.rs

//! Downloading a published file over HTTPS into a local one, with its
//! SHA-256 counted on the way.
//!
//! Only `https://` is followed, redirects included — a release is published
//! behind one. The checksum is what proves the content; the caller compares
//! it with the pinned one. A download that stops moving ends with an error
//! instead of waiting for ever.

use std::{path::Path, sync::Arc, time::Duration};

use http_body_util::{BodyExt as _, Empty};
use hyper::{
    Request, StatusCode, Uri,
    body::Bytes,
    client::conn::http1,
    header::{CONNECTION, HOST, LOCATION, USER_AGENT},
};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use sha2::{Digest, Sha256};
use tokio::{io::AsyncWriteExt as _, net::TcpStream};
use tokio_rustls::TlsConnector;

/// How many redirects are followed before the download is given up.
const REDIRECTS: usize = 5;

/// How long the download may go without a byte — or without a connection
/// or an answer — before it is given up.
const STALL_LIMIT: Duration = Duration::from_secs(60);

/// Download `url` into `into`, replacing what is there, and return the
/// SHA-256 of what was written, as lowercase hex.
///
/// # Errors
///
/// The address is not `https://`, the server cannot be reached or judged,
/// it answers otherwise than 200 after at most [`REDIRECTS`] redirects, the
/// download stalls for [`STALL_LIMIT`], or the file cannot be written; the
/// reason, in words.
pub async fn fetch(url: &str, into: &Path) -> Result<String, String> {
    let tls = Arc::new(crate::application::tls(
        "the certificate of the server it is downloaded from",
    )?);
    let mut address = url.to_owned();
    for _ in 0..=REDIRECTS {
        let response = within(get(&tls, &address)).await?;
        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|location| location.to_str().ok())
                .ok_or_else(|| format!("{address}: {status} without a location"))?;
            address = if location.starts_with('/') {
                // A location on the same server.
                let uri: Uri = address
                    .parse()
                    .map_err(|error| format!("{address}: {error}"))?;
                format!(
                    "https://{}{location}",
                    uri.authority().map_or("", |authority| authority.as_str())
                )
            } else {
                location.to_owned()
            };
            continue;
        }
        if status != StatusCode::OK {
            return Err(format!("{address}: the server answered {status}"));
        }
        return save(response.into_body(), into).await.map_err(|problem| {
            let _ = std::fs::remove_file(into);
            format!("{url}: {problem}")
        });
    }
    Err(format!("{url}: more than {REDIRECTS} redirects"))
}

/// One `GET` of an `https://` address on a connection of its own.
async fn get(
    tls: &Arc<rustls::ClientConfig>,
    address: &str,
) -> Result<hyper::Response<hyper::body::Incoming>, String> {
    let failed = |problem: &dyn std::fmt::Display| format!("{address}: {problem}");
    let uri: Uri = address.parse().map_err(|error| failed(&error))?;
    if uri.scheme_str() != Some("https") {
        return Err(failed(&"only https:// is downloaded from"));
    }
    let host = uri.host().ok_or_else(|| failed(&"no host"))?;
    let port = uri.port_u16().unwrap_or(443);
    let target = uri.path_and_query().map_or("/", |target| target.as_str());
    let stream = TcpStream::connect((host, port))
        .await
        .map_err(|error| failed(&error))?;
    let name = ServerName::try_from(host.to_owned()).map_err(|error| failed(&error))?;
    let secured = TlsConnector::from(Arc::clone(tls))
        .connect(name, stream)
        .await
        .map_err(|error| failed(&error))?;
    let (mut sender, connection) = http1::handshake(TokioIo::new(secured))
        .await
        .map_err(|error| failed(&error))?;
    // The connection is driven beside the request and ends with it.
    tokio::spawn(connection);
    let request = Request::builder()
        .method("GET")
        .uri(target)
        .header(HOST, host)
        .header(
            USER_AGENT,
            concat!("asterisk-controller/", env!("CARGO_PKG_VERSION")),
        )
        .header(CONNECTION, "close")
        .body(Empty::<Bytes>::new())
        .map_err(|error| failed(&error))?;
    sender
        .send_request(request)
        .await
        .map_err(|error| failed(&error))
}

/// Write the body into `into` and count its SHA-256.
async fn save(mut body: hyper::body::Incoming, into: &Path) -> Result<String, String> {
    let mut file = tokio::fs::File::create(into)
        .await
        .map_err(|error| format!("{}: {error}", into.display()))?;
    let mut checksum = Sha256::new();
    while let Some(frame) = within(async { Ok(body.frame().await) }).await? {
        let frame = frame.map_err(|error| error.to_string())?;
        if let Ok(data) = frame.into_data() {
            checksum.update(&data);
            file.write_all(&data)
                .await
                .map_err(|error| format!("{}: {error}", into.display()))?;
        }
    }
    file.flush()
        .await
        .map_err(|error| format!("{}: {error}", into.display()))?;
    Ok(hex(&checksum.finalize()))
}

/// Run a step of the download within [`STALL_LIMIT`].
async fn within<T>(step: impl Future<Output = Result<T, String>>) -> Result<T, String> {
    tokio::time::timeout(STALL_LIMIT, step)
        .await
        .unwrap_or_else(|_| {
            Err(format!(
                "nothing arrived for {} seconds",
                STALL_LIMIT.as_secs()
            ))
        })
}

/// Bytes as lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn only_https_is_downloaded_from() {
        let into = std::env::temp_dir().join(format!("gabion-download-{}", uuid::Uuid::new_v4()));
        let refusal = fetch("http://example.com/archive.tar.gz", &into)
            .await
            .err()
            .unwrap_or_default();
        assert_eq!(
            refusal,
            "http://example.com/archive.tar.gz: only https:// is downloaded from"
        );
        assert!(!into.exists());
    }

    #[test]
    fn bytes_are_written_as_lowercase_hex() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }
}
