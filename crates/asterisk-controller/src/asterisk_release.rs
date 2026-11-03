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

/// The release of gabion-dev/asterisk-server.
pub const RELEASE: &str = "22.11.0-r3";

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
    /// macOS, which has no glibc.
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
    sha256: "28145d8df5909528580bc04abff350af770f2e4086bae0084d978352ad463a21",
    glibc: &[
        ("libc.so.6", "GLIBC_2.35"),
        ("libm.so.6", "GLIBC_2.29"),
        ("ld-linux-x86-64.so.2", "GLIBC_2.3"),
    ],
};

const LINUX_ARM64: Archive = Archive {
    name: "asterisk-server-linux-arm64.tar.gz",
    sha256: "f43c5a4eb868d9ec64c3cc0ee15e44615c13923b0af050aaf3f6d3e2da2b5ba4",
    glibc: &[
        ("libc.so.6", "GLIBC_2.35"),
        ("libm.so.6", "GLIBC_2.29"),
        ("ld-linux-aarch64.so.1", "GLIBC_2.17"),
    ],
};

const DARWIN_ARM64: Archive = Archive {
    name: "asterisk-server-darwin-arm64.tar.gz",
    sha256: "dad28302be12f39f687e509f45a4de7fa3396d0514524cc681d2e3392633aca4",
    glibc: &[],
};

const DARWIN_AMD64: Archive = Archive {
    name: "asterisk-server-darwin-amd64.tar.gz",
    sha256: "56791a5a2f1c04ad5eabcb94d9e8e5055885101454ca3d2c7a141520b731e676",
    glibc: &[],
};

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
        for archive in [&LINUX_AMD64, &LINUX_ARM64, &DARWIN_ARM64, &DARWIN_AMD64] {
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
