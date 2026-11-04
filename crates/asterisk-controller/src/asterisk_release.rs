// crates/asterisk-controller/src/asterisk_release.rs

//! The Asterisk build this controller drives: one release of
//! gabion-dev/asterisk-server, pinned here by the checksum of each of its
//! four archives.
//!
//! This is the one place the pair is named. The checks against Asterisk run
//! on a tree put in place from here ([`crate::tree`]), and so does a telephony
//! node: what the laboratory checked is what a node runs. An archive whose
//! content differs is refused — "downloaded from the right address" proves
//! nothing about what was downloaded.
//!
//! Bumping the release means its tag, the four checksums published next to
//! its archives, the glibc symbol versions its Linux trees need (`readelf
//! -V` of every ELF file of the tree, newest per library) and the modules it
//! carries ([`crate::asterisk_files`]).
//!
//! `asterisk-controller asterisk-release` prints the pin — the release and,
//! for each archive, where it is published, its checksum and what its tree
//! needs of glibc — for whoever has to know what a node of this controller
//! runs without running one: a scanner of the archives a node downloads
//! reads it here rather than keeping a copy that would drift.

use std::process::ExitCode;

/// The release of gabion-dev/asterisk-server.
pub const RELEASE: &str = "22.11.0-r4";

/// Where the archives of every release are published.
const DOWNLOADS: &str = "https://github.com/gabion-dev/asterisk-server/releases/download";

/// One archive of the release: a tree for one platform.
pub struct Archive {
    /// The file name, as published.
    pub name: &'static str,
    /// SHA-256 of the archive, as published next to it.
    pub sha256: &'static str,
    /// The newest symbol version every glibc library must define for the
    /// tree to load. glibc defines its versions cumulatively, so a library
    /// that defines the newest one defines the older ones too. Empty on
    /// macOS, which has no glibc. Judged where there is glibc
    /// ([`crate::host`]); told with the pin everywhere.
    pub glibc: &'static [(&'static str, &'static str)],
}

impl Archive {
    /// Where the archive is downloaded from.
    pub fn url(&self) -> String {
        format!("{DOWNLOADS}/{RELEASE}/{}", self.name)
    }
}

/// The bundled `libgcc_s` needs `_dl_find_object` from `GLIBC_2.35`, which a
/// stock glibc 2.34 lacks and the RHEL 9 family's and Amazon Linux 2023's
/// glibc 2.34 carry as a backport: the symbol version, not the glibc
/// release number, is what the dynamic loader checks.
const LINUX_AMD64: Archive = Archive {
    name: "asterisk-server-linux-amd64.tar.gz",
    sha256: "13d4d99fa4ae68f8ccf685008cc14c81061a8abae7bc74caca07031ae038d509",
    glibc: &[
        ("libc.so.6", "GLIBC_2.35"),
        ("libm.so.6", "GLIBC_2.29"),
        ("ld-linux-x86-64.so.2", "GLIBC_2.3"),
    ],
};

const LINUX_ARM64: Archive = Archive {
    name: "asterisk-server-linux-arm64.tar.gz",
    sha256: "6862b85a22015cf7ea93a70087781b36db06cb32ea49a1f1ee8c25fa65b97b70",
    glibc: &[
        ("libc.so.6", "GLIBC_2.35"),
        ("libm.so.6", "GLIBC_2.29"),
        ("ld-linux-aarch64.so.1", "GLIBC_2.17"),
    ],
};

const DARWIN_ARM64: Archive = Archive {
    name: "asterisk-server-darwin-arm64.tar.gz",
    sha256: "95609c142d83598bc676572ae5e4064225d0d7746445488da961a7403aebf6c8",
    glibc: &[],
};

const DARWIN_AMD64: Archive = Archive {
    name: "asterisk-server-darwin-amd64.tar.gz",
    sha256: "bfd650e25616c78fa068cf736003294147622a8117f4abd786b17a6149e36cf7",
    glibc: &[],
};

/// Every archive of the release.
const ARCHIVES: [&Archive; 4] = [&LINUX_AMD64, &LINUX_ARM64, &DARWIN_ARM64, &DARWIN_AMD64];

/// The pin, as `asterisk-controller asterisk-release` prints it.
fn pin() -> serde_json::Value {
    let archives: Vec<serde_json::Value> = ARCHIVES
        .iter()
        .map(|archive| {
            let glibc: serde_json::Map<String, serde_json::Value> = archive
                .glibc
                .iter()
                .map(|&(library, version)| (library.to_owned(), version.into()))
                .collect();
            serde_json::json!({
                "name": archive.name,
                "url": archive.url(),
                "sha256": archive.sha256,
                "glibc": glibc,
            })
        })
        .collect();
    serde_json::json!({ "release": RELEASE, "archives": archives })
}

/// Run `asterisk-controller asterisk-release`: print the pin.
pub fn run(mut arguments: impl Iterator<Item = String>) -> ExitCode {
    if arguments.next().is_some() {
        eprintln!("usage: asterisk-controller asterisk-release");
        return ExitCode::FAILURE;
    }
    match serde_json::to_string_pretty(&pin()) {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("asterisk-controller asterisk-release: {error}");
            ExitCode::FAILURE
        }
    }
}

/// The archive of the platform this controller is built for — the one it
/// runs on.
///
/// # Errors
///
/// There is no Asterisk build for this platform.
pub fn for_this_machine() -> Result<&'static Archive, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok(&LINUX_AMD64),
        ("linux", "aarch64") => Ok(&LINUX_ARM64),
        ("macos", "aarch64") => Ok(&DARWIN_ARM64),
        ("macos", "x86_64") => Ok(&DARWIN_AMD64),
        (os, arch) => Err(format!("there is no Asterisk build for {os} {arch}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_archive_is_pinned_by_a_whole_checksum_and_linux_ones_by_their_glibc_needs() {
        for archive in ARCHIVES {
            assert_eq!(archive.sha256.len(), 64, "{}", archive.name);
            assert!(
                archive.sha256.chars().all(|c| c.is_ascii_hexdigit()),
                "{}",
                archive.name
            );
            assert_eq!(
                archive.glibc.is_empty(),
                archive.name.contains("darwin"),
                "{}",
                archive.name
            );
        }
    }

    #[test]
    fn the_printed_pin_is_the_pin_of_every_archive() {
        let field = |value: &serde_json::Value, key: &str| value.get(key).cloned();
        let pin = pin();
        assert_eq!(field(&pin, "release"), Some(RELEASE.into()));
        let printed = field(&pin, "archives")
            .and_then(|archives| archives.as_array().cloned())
            .unwrap_or_default();
        assert_eq!(printed.len(), ARCHIVES.len());
        for (archive, printed) in ARCHIVES.iter().zip(&printed) {
            assert_eq!(field(printed, "name"), Some(archive.name.into()));
            assert_eq!(field(printed, "url"), Some(archive.url().into()));
            assert_eq!(field(printed, "sha256"), Some(archive.sha256.into()));
            let glibc = field(printed, "glibc")
                .and_then(|glibc| glibc.as_object().cloned())
                .unwrap_or_default();
            assert_eq!(glibc.len(), archive.glibc.len(), "{}", archive.name);
            for &(library, version) in archive.glibc {
                assert_eq!(
                    glibc.get(library),
                    Some(&version.into()),
                    "{}",
                    archive.name
                );
            }
        }
    }

    #[test]
    fn this_machine_has_an_archive_and_its_address_names_the_release() -> Result<(), String> {
        let archive = for_this_machine()?;
        assert_eq!(
            archive.url(),
            format!(
                "https://github.com/gabion-dev/asterisk-server/releases/download/{RELEASE}/{}",
                archive.name
            )
        );
        Ok(())
    }
}
