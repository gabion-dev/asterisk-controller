// crates/repository-checks/tests/file_headers.rs

//! Every file states its own path, from the repository root, at its top,
//! and an empty line sets that line apart from the file's own content.
//!
//! A file opened on its own — in a review, a search result, a pasted
//! fragment — then says where it lives. The form of the line depends on the
//! file type; the types that a comment would break, and the files that must
//! not carry one, are listed here by name. A file of a type this check does
//! not know fails it: how a new type is marked is decided once, here, not
//! left to whoever adds the first such file.

use std::{
    fs,
    path::{Path, PathBuf},
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Directories that are not part of the repository's own content.
const SKIPPED_DIRECTORIES: &[&str] = &[".git", "target"];

/// Files that carry no path line, each for its own reason.
const EXEMPT_FILES: &[(&str, &str)] = &[
    ("README.md", "the face of the repository: shown as is"),
    ("LICENSE", "a legal text, kept byte for byte"),
    ("Cargo.lock", "written by cargo, never by hand"),
];

/// How the path line of a file looks, by what the file is.
enum Marking {
    /// `<prefix><path><suffix>` on the first line, or on the second when
    /// the first is a `#!` line.
    Line {
        prefix: &'static str,
        suffix: &'static str,
    },
    /// The format has no comments; a path line would break the file.
    NoComments,
}

fn marking_of(file_name: &str) -> Option<Marking> {
    let extension = Path::new(file_name).extension().and_then(|e| e.to_str());
    match (file_name, extension) {
        (_, Some("rs" | "java")) => Some(Marking::Line {
            prefix: "// ",
            suffix: "",
        }),
        (".gitignore", _) | (_, Some("toml" | "yml" | "yaml" | "sh")) => Some(Marking::Line {
            prefix: "# ",
            suffix: "",
        }),
        (_, Some("md")) => Some(Marking::Line {
            prefix: "<!-- ",
            suffix: " -->",
        }),
        (_, Some("json")) => Some(Marking::NoComments),
        _ => None,
    }
}

fn collect_files(directory: &Path, files: &mut Vec<PathBuf>) -> TestResult {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if entry.file_type()?.is_dir() {
            if !SKIPPED_DIRECTORIES.iter().any(|skipped| name == *skipped) {
                collect_files(&path, files)?;
            }
        } else {
            files.push(path);
        }
    }
    Ok(())
}

#[test]
fn every_file_states_its_path_at_the_top() -> TestResult {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let mut files = Vec::new();
    collect_files(&root, &mut files)?;
    assert!(
        files.len() > 5,
        "the walk found almost nothing under {}",
        root.display()
    );

    let mut problems = Vec::new();
    for file in files {
        let relative = file
            .strip_prefix(&root)?
            .to_string_lossy()
            .replace('\\', "/");
        let file_name = file
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if EXEMPT_FILES.iter().any(|(exempt, _)| relative == *exempt) {
            continue;
        }
        match marking_of(file_name) {
            None => problems.push(format!(
                "{relative}: a file type this check does not know — decide in file_headers.rs \
                 how such files state their path, or why they cannot"
            )),
            Some(Marking::NoComments) => {}
            Some(Marking::Line { prefix, suffix }) => {
                let expected = format!("{prefix}{relative}{suffix}");
                let text = fs::read_to_string(&file)?;
                let mut lines = text.lines();
                let first = lines.next().unwrap_or_default();
                let stated = if first.starts_with("#!") {
                    lines.next().unwrap_or_default()
                } else {
                    first
                };
                if stated != expected {
                    problems.push(format!(
                        "{relative}: the top of the file must read `{expected}`, found `{stated}`"
                    ));
                } else if lines.next().is_some_and(|after| !after.trim().is_empty()) {
                    problems.push(format!(
                        "{relative}: the path line must be followed by an empty line"
                    ));
                }
            }
        }
    }

    assert!(problems.is_empty(), "\n{}\n", problems.join("\n"));
    Ok(())
}
