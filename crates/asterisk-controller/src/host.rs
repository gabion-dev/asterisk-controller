// crates/asterisk-controller/src/host.rs

//! Whether this machine can run the pinned Asterisk tree — judged before
//! the tree is put in place, so a machine that cannot is told what it lacks
//! instead of meeting an error of the dynamic loader at the first start.
//!
//! A Linux tree carries every library but the C library, and that one is
//! compatible only upward. What the dynamic loader checks is that every
//! glibc library a file of the tree depends on defines the symbol version
//! the file asks for, so that is what is judged here — not the glibc release
//! number, which disagrees with it both ways (the RHEL 9 family's glibc 2.34
//! defines `GLIBC_2.35` as a backport, a stock 2.34 does not). The libraries
//! judged are the ones this very process loaded: the controller links the
//! same glibc libraries the tree does, so they are the machine's own.
//!
//! A macOS tree needs Darwin 22 (macOS 13) or later.

use crate::asterisk_release::Archive;

/// Refuse a machine the tree of `archive` would not load on, saying what
/// it lacks.
///
/// # Errors
///
/// The machine lacks what the tree needs, or what it has cannot be read.
pub fn can_run(archive: &Archive) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        glibc_defines(archive)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = archive;
        darwin_is_recent()
    }
}

#[cfg(target_os = "macos")]
fn darwin_is_recent() -> Result<(), String> {
    const OLDEST: u32 = 22;
    let release = rustix::system::uname()
        .release()
        .to_string_lossy()
        .into_owned();
    let major = release
        .split('.')
        .next()
        .and_then(|major| major.parse::<u32>().ok())
        .ok_or_else(|| format!("the Darwin release {release:?} could not be read"))?;
    if major < OLDEST {
        return Err(format!(
            "this is Darwin {release}; the Asterisk tree needs Darwin {OLDEST} (macOS 13) or later"
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn glibc_defines(archive: &Archive) -> Result<(), String> {
    use std::path::PathBuf;

    let maps = std::fs::read_to_string("/proc/self/maps")
        .map_err(|error| format!("/proc/self/maps: {error}"))?;
    let loaded: Vec<PathBuf> = maps
        .lines()
        .filter_map(|line| line.split_whitespace().nth(5))
        .filter(|path| path.starts_with('/'))
        .map(PathBuf::from)
        .collect();
    let libc_directory = loaded
        .iter()
        .find(|path| path.file_name().is_some_and(|name| name == "libc.so.6"))
        .and_then(|path| path.parent().map(std::path::Path::to_path_buf));
    let mut lacking = Vec::new();
    for &(library, version) in archive.glibc {
        // A library this process did not load is looked for next to the C
        // library it did.
        let path = loaded
            .iter()
            .find(|path| path.file_name().is_some_and(|name| name == library))
            .cloned()
            .or_else(|| {
                libc_directory
                    .as_ref()
                    .map(|directory| directory.join(library))
            })
            .ok_or_else(|| {
                "this process loaded no libc.so.6: the machine's C library is not glibc".to_owned()
            })?;
        let elf = std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        let defined = defined_versions(&elf).map_err(|problem| {
            format!(
                "the symbol versions of {} could not be read: {problem}",
                path.display()
            )
        })?;
        if !defined.iter().any(|name| name == version) {
            lacking.push(format!(
                "{version} in {library} ({})",
                defined
                    .iter()
                    .filter(|name| name.starts_with("GLIBC_2"))
                    .max_by_key(|name| glibc_order(name))
                    .map_or_else(
                        || "it defines no GLIBC_2 version".to_owned(),
                        |newest| format!("its newest is {newest}")
                    )
            ));
        }
    }
    if lacking.is_empty() {
        return Ok(());
    }
    Err(format!(
        "this machine's glibc is too old for the Asterisk tree: it lacks {}",
        lacking.join(", ")
    ))
}

/// Orders `GLIBC_2.<minor>[.<patch>]` by its numbers.
#[cfg(target_os = "linux")]
fn glibc_order(name: &str) -> Vec<u32> {
    name.trim_start_matches("GLIBC_")
        .split('.')
        .filter_map(|part| part.parse().ok())
        .collect()
}

/// The symbol versions an ELF file defines: the names of its version
/// definition section (`SHT_GNU_verdef`). Little-endian 64-bit only — the
/// two Linux platforms of the node.
///
/// # Errors
///
/// The file is not a little-endian 64-bit ELF file, or its sections do not
/// hold together.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn defined_versions(elf: &[u8]) -> Result<Vec<String>, String> {
    const SHT_GNU_VERDEF: u32 = 0x6fff_fffd;
    if elf.get(..4) != Some(b"\x7fELF".as_slice()) {
        return Err("not an ELF file".into());
    }
    if elf.get(4) != Some(&2) || elf.get(5) != Some(&1) {
        return Err("not a little-endian 64-bit ELF file".into());
    }
    let section_table = to_usize(read_u64(elf, 0x28)?)?;
    let entry_size = usize::from(read_u16(elf, 0x3a)?);
    let sections = usize::from(read_u16(elf, 0x3c)?);
    let section = |index: usize| -> Result<(u32, usize, usize, usize), String> {
        let at = index
            .checked_mul(entry_size)
            .and_then(|offset| offset.checked_add(section_table))
            .ok_or("a section header lies beyond the file")?;
        Ok((
            read_u32(elf, at.saturating_add(0x04))?,
            to_usize(read_u64(elf, at.saturating_add(0x18))?)?,
            to_usize(read_u64(elf, at.saturating_add(0x20))?)?,
            to_usize(u64::from(read_u32(elf, at.saturating_add(0x28))?))?,
        ))
    };
    for index in 0..sections {
        let (kind, offset, size, link) = section(index)?;
        if kind != SHT_GNU_VERDEF {
            continue;
        }
        let (_, strings, strings_size, _) = section(link)?;
        let names = elf
            .get(strings..strings.saturating_add(strings_size))
            .ok_or("the string table lies beyond the file")?;
        let definitions = elf
            .get(offset..offset.saturating_add(size))
            .ok_or("the version definitions lie beyond the file")?;
        return version_names(definitions, names);
    }
    Ok(Vec::new())
}

/// Walk the chain of version definitions (`Elf64_Verdef`, each followed by
/// its `Elf64_Verdaux` entries) and collect the first name of each — the
/// version it defines; the others name the versions it succeeds.
fn version_names(definitions: &[u8], names: &[u8]) -> Result<Vec<String>, String> {
    let mut found = Vec::new();
    let mut at = 0usize;
    loop {
        let auxiliary = to_usize(u64::from(read_u32(definitions, at.saturating_add(12))?))?;
        let next = to_usize(u64::from(read_u32(definitions, at.saturating_add(16))?))?;
        let name_at = to_usize(u64::from(read_u32(
            definitions,
            at.saturating_add(auxiliary),
        )?))?;
        let name = names
            .get(name_at..)
            .and_then(|rest| rest.split(|&byte| byte == 0).next())
            .ok_or("a version name lies beyond the string table")?;
        found.push(String::from_utf8_lossy(name).into_owned());
        if next == 0 {
            return Ok(found);
        }
        at = at
            .checked_add(next)
            .ok_or("the version definitions do not end")?;
    }
}

fn read_u16(bytes: &[u8], at: usize) -> Result<u16, String> {
    bytes
        .get(at..at.saturating_add(2))
        .and_then(|field| field.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| format!("a field at {at} lies beyond the file"))
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32, String> {
    bytes
        .get(at..at.saturating_add(4))
        .and_then(|field| field.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| format!("a field at {at} lies beyond the file"))
}

fn read_u64(bytes: &[u8], at: usize) -> Result<u64, String> {
    bytes
        .get(at..at.saturating_add(8))
        .and_then(|field| field.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| format!("a field at {at} lies beyond the file"))
}

fn to_usize(value: u64) -> Result<usize, String> {
    usize::try_from(value).map_err(|_| format!("{value} does not fit an address"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_not_an_elf_file_is_refused() {
        assert_eq!(
            defined_versions(b"#!/bin/sh\n"),
            Err("not an ELF file".to_owned())
        );
        assert!(defined_versions(b"\x7fELF\x01\x01").is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_c_library_this_process_loaded_defines_the_versions_the_controller_itself_needs()
    -> Result<(), String> {
        let maps = std::fs::read_to_string("/proc/self/maps").map_err(|e| e.to_string())?;
        let libc = maps
            .lines()
            .filter_map(|line| line.split_whitespace().nth(5))
            .find(|path| path.ends_with("/libc.so.6"))
            .ok_or("no libc.so.6 is loaded")?;
        let defined = defined_versions(&std::fs::read(libc).map_err(|e| e.to_string())?)?;
        // The controller is built on glibc 2.34 and needs its versions.
        for version in ["GLIBC_2.2.5", "GLIBC_2.17", "GLIBC_2.34"] {
            if cfg!(target_arch = "aarch64") && version == "GLIBC_2.2.5" {
                continue;
            }
            assert!(defined.iter().any(|name| name == version), "{version}");
        }
        assert!(!defined.iter().any(|name| name == "GLIBC_9.99"));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_version_the_machine_does_not_define_is_named_with_the_newest_it_has() {
        let archive = Archive {
            name: "probe",
            sha256: "",
            glibc: &[("libc.so.6", "GLIBC_9.99")],
        };
        let refusal = glibc_defines(&archive).err().unwrap_or_default();
        assert!(
            refusal.starts_with(
                "this machine's glibc is too old for the Asterisk tree: it lacks GLIBC_9.99 in \
                 libc.so.6 (its newest is GLIBC_2."
            ),
            "{refusal}"
        );
    }
}
