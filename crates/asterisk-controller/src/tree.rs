// crates/asterisk-controller/src/tree.rs

//! Putting the pinned Asterisk tree in place ([`crate::asterisk_release`]).
//!
//! ```text
//! asterisk-controller tree <directory>
//! ```
//!
//! The machine is judged first ([`crate::host`]): one the tree would not load
//! on is told what it lacks, and nothing is downloaded. Then the archive of
//! this platform is downloaded, refused unless its SHA-256 is the pinned
//! one, and unpacked beside `<directory>`; only a whole tree takes the
//! directory's place, stamped with the release in [`STAMP`]. A directory
//! already stamped with the pinned release is kept as it is.
//!
//! The checks against Asterisk put their tree in place this way, and so
//! does a telephony node: one way for the laboratory and for a node.

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};

use crate::{asterisk_release, download, host};

/// The file of a tree that names the release it was put in place from.
pub const STAMP: &str = ".release";

/// What putting the tree in place came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Placed {
    /// The directory held the pinned release already.
    AlreadyThere,
    /// The pinned release was downloaded and unpacked.
    Unpacked,
}

/// Run `asterisk-controller tree <directory>`.
pub async fn run(mut arguments: impl Iterator<Item = String>) -> ExitCode {
    let (Some(directory), None) = (arguments.next(), arguments.next()) else {
        eprintln!("usage: asterisk-controller tree <directory>");
        return ExitCode::FAILURE;
    };
    let directory = PathBuf::from(directory);
    let release = asterisk_release::RELEASE;
    match put_in_place(&directory).await {
        Ok(Placed::AlreadyThere) => {
            println!("Asterisk {release} is already in {}", directory.display());
            ExitCode::SUCCESS
        }
        Ok(Placed::Unpacked) => {
            println!("Asterisk {release} is in {}", directory.display());
            ExitCode::SUCCESS
        }
        Err(problem) => {
            eprintln!("asterisk-controller tree: {problem}");
            ExitCode::FAILURE
        }
    }
}

/// Put the pinned tree of this platform in `directory`.
///
/// # Errors
///
/// The machine cannot run the tree, the archive cannot be downloaded or is
/// not the pinned one, or it cannot be unpacked; the reason, in words.
/// Whatever was in `directory` stays as it was.
pub async fn put_in_place(directory: &Path) -> Result<Placed, String> {
    let release = asterisk_release::RELEASE;
    if holds(directory, release) {
        return Ok(Placed::AlreadyThere);
    }
    let archive = asterisk_release::for_this_machine()?;
    host::can_run(archive)?;
    let download = beside(directory, "download")?;
    let checksum = download::fetch(&archive.url(), &download).await?;
    let directory_owned = directory.to_path_buf();
    let download_owned = download.clone();
    let unpacked = tokio::task::spawn_blocking(move || {
        unpack(
            &download_owned,
            &checksum,
            archive.sha256,
            release,
            &directory_owned,
        )
    })
    .await
    .map_err(|error| format!("unpacking did not finish: {error}"));
    let _ = std::fs::remove_file(&download);
    unpacked??;
    Ok(Placed::Unpacked)
}

/// Whether `directory` holds a tree stamped with `release`.
fn holds(directory: &Path, release: &str) -> bool {
    std::fs::read_to_string(directory.join(STAMP)).is_ok_and(|stamp| stamp.trim() == release)
        && directory.join("sbin").join("asterisk").is_file()
}

/// A path next to `directory`, hidden, for a step of putting it in place.
fn beside(directory: &Path, step: &str) -> Result<PathBuf, String> {
    let name = directory
        .file_name()
        .ok_or_else(|| format!("{} names no directory", directory.display()))?;
    let parent = directory.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    Ok(parent.join(format!(".{}.{step}", name.to_string_lossy())))
}

/// Unpack `archive`, whose SHA-256 is `checksum`, into `directory` —
/// refused unless the checksum is `pinned`. The tree is unpacked beside the
/// directory and takes its place whole, stamped with `release`.
///
/// # Errors
///
/// The checksum is not the pinned one, or the archive cannot be unpacked or
/// put in place.
fn unpack(
    archive: &Path,
    checksum: &str,
    pinned: &str,
    release: &str,
    directory: &Path,
) -> Result<(), String> {
    if checksum != pinned {
        return Err(format!(
            "{} is not the pinned Asterisk {release}: its SHA-256 is {checksum}, the pinned one \
             is {pinned}",
            archive.display()
        ));
    }
    let partial = beside(directory, "partial")?;
    let failed = |error: &dyn std::fmt::Display| format!("{}: {error}", partial.display());
    if partial.exists() {
        std::fs::remove_dir_all(&partial).map_err(|error| failed(&error))?;
    }
    std::fs::create_dir_all(&partial).map_err(|error| failed(&error))?;
    let opened =
        std::fs::File::open(archive).map_err(|error| format!("{}: {error}", archive.display()))?;
    let unpacked = tar::Archive::new(flate2::read::GzDecoder::new(opened))
        .unpack(&partial)
        .map_err(|error| format!("{} could not be unpacked: {error}", archive.display()))
        .and_then(|()| {
            std::fs::write(partial.join(STAMP), format!("{release}\n"))
                .map_err(|error| failed(&error))
        });
    if let Err(problem) = unpacked {
        let _ = std::fs::remove_dir_all(&partial);
        return Err(problem);
    }
    if directory.exists() {
        std::fs::remove_dir_all(directory)
            .map_err(|error| format!("{}: {error}", directory.display()))?;
    }
    std::fs::rename(&partial, directory)
        .map_err(|error| format!("{}: {error}", directory.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// A scratch directory and, in it, an archive holding `sbin/asterisk`.
    fn archive() -> Result<(PathBuf, PathBuf, String), Box<dyn std::error::Error>> {
        let scratch = std::env::temp_dir().join(format!("gabion-tree-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&scratch)?;
        let path = scratch.join("asterisk-server.tar.gz");
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            std::fs::File::create(&path)?,
            flate2::Compression::default(),
        ));
        let content = b"#!/bin/sh\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(u64::try_from(content.len())?);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, "sbin/asterisk", content.as_slice())?;
        builder.into_inner()?.finish()?;
        let checksum = download::hex(&Sha256::digest(std::fs::read(&path)?));
        Ok((scratch, path, checksum))
    }

    #[test]
    fn the_pinned_archive_becomes_the_directory_stamped_with_its_release() -> TestResult {
        let (scratch, archive, checksum) = archive()?;
        let directory = scratch.join("asterisk-server");
        std::fs::create_dir_all(&directory)?;
        std::fs::write(directory.join("left-over"), "")?;
        unpack(&archive, &checksum, &checksum, "1.2.3-r4", &directory)?;
        assert!(holds(&directory, "1.2.3-r4"));
        assert!(!holds(&directory, "1.2.3-r5"));
        assert!(!directory.join("left-over").exists());
        assert!(!scratch.join(".asterisk-server.partial").exists());
        std::fs::remove_dir_all(scratch)?;
        Ok(())
    }

    #[test]
    fn an_archive_that_is_not_the_pinned_one_is_refused_and_the_directory_stays() -> TestResult {
        let (scratch, archive, checksum) = archive()?;
        let directory = scratch.join("asterisk-server");
        std::fs::create_dir_all(&directory)?;
        std::fs::write(directory.join("kept"), "")?;
        let pinned = "0".repeat(64);
        let refusal = unpack(&archive, &checksum, &pinned, "1.2.3-r4", &directory)
            .err()
            .unwrap_or_default();
        assert_eq!(
            refusal,
            format!(
                "{} is not the pinned Asterisk 1.2.3-r4: its SHA-256 is {checksum}, the pinned \
                 one is {pinned}",
                archive.display()
            )
        );
        assert!(directory.join("kept").exists());
        std::fs::remove_dir_all(scratch)?;
        Ok(())
    }

    #[test]
    fn what_is_not_an_archive_leaves_nothing_half_unpacked() -> TestResult {
        let scratch = std::env::temp_dir().join(format!("gabion-tree-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&scratch)?;
        let archive = scratch.join("asterisk-server.tar.gz");
        std::fs::write(&archive, "not gzip")?;
        let checksum = download::hex(&Sha256::digest(std::fs::read(&archive)?));
        let directory = scratch.join("asterisk-server");
        assert!(unpack(&archive, &checksum, &checksum, "1.2.3-r4", &directory).is_err());
        assert!(!directory.exists());
        assert!(!scratch.join(".asterisk-server.partial").exists());
        std::fs::remove_dir_all(scratch)?;
        Ok(())
    }
}
