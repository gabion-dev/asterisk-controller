// crates/asterisk-controller/src/state_files.rs

//! How the controller writes into the state directory.
//!
//! A file is written whole or not at all, and once written it stays: it is
//! written under a temporary name next to it, flushed to the disk, renamed
//! over the old one, and the rename is flushed by syncing the directory.
//! Without the two flushes a power loss could undo a rename the controller
//! has already acted on, or leave the name pointing at an empty file: a
//! confirmed report would come back, settings the application was told are
//! applied would be gone, a secret Asterisk has read would be empty. The
//! journal of reports is appended to and flushed line by line.
//!
//! Everything written here is readable by its owner alone: the control
//! secret is a password, settings carry the operators' passwords, reports
//! carry callers' numbers. Asterisk reads what it needs as the same user —
//! it already must, since its configuration directory is closed to others.

use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Write as _},
    os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _},
    path::Path,
};

/// The mode of every file the controller writes.
const OWNER_ONLY: u32 = 0o600;

/// Replace a file whole or not at all, and keep it.
///
/// # Errors
///
/// The file or its directory cannot be written or flushed.
pub fn replace(path: &Path, content: &[u8]) -> io::Result<()> {
    let (directory, name) = place(path)?;
    let written = directory.join(format!(".{name}.new"));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(OWNER_ONLY)
        .open(&written)?;
    // The mode of `open` applies to a file it creates; one left over by a
    // write that died keeps the mode it had.
    file.set_permissions(fs::Permissions::from_mode(OWNER_ONLY))?;
    file.write_all(content)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&written, path)?;
    File::open(directory)?.sync_all()
}

/// Append a line to a file, creating it, and keep it.
///
/// # Errors
///
/// The file or its directory cannot be written or flushed.
pub fn append_line(path: &Path, line: &str) -> io::Result<()> {
    let (directory, _) = place(path)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(OWNER_ONLY)
        .open(path)?;
    writeln!(file, "{line}")?;
    file.sync_data()?;
    // The file may have just been created: its name is kept only when the
    // directory is flushed.
    File::open(directory)?.sync_all()
}

/// Whether anyone but the owner may read, write or run the file.
pub fn open_to_others(metadata: &Metadata) -> bool {
    metadata.permissions().mode() & 0o077 != 0
}

/// The directory of a file and the file's name.
fn place(path: &Path) -> io::Result<(&Path, String)> {
    match (path.parent(), path.file_name()) {
        (Some(directory), Some(name)) => Ok((directory, name.to_string_lossy().into_owned())),
        _ => Err(io::Error::other(format!(
            "{} is not a file in a directory",
            path.display()
        ))),
    }
}
