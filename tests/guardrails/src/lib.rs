//! Invariants enforced against the source itself.
//!
//! Most of this project's promises are enforced by types: `Authorized` has no
//! public constructor, the crate graph forbids `ui` from reaching `kube`. Two
//! are not, because they are promises about *what is written down*, and no type
//! can see the difference between a string that names a cluster and one that
//! names its endpoint.
//!
//! Those two were both broken in practice — the apiserver URL was logged on
//! every connect, and a credential plugin's stdout could reach the audit log —
//! and neither had anything watching. This crate is that something. It reads
//! the source and fails the build when a shape known to leak reappears.
//!
//! A source scan is a blunt instrument and this one is deliberately narrow: it
//! looks for the exact shapes that have already gone wrong, not for anything
//! that might. A guardrail that cries wolf gets an `#[allow]` and stops being a
//! guardrail.

use std::path::{Path, PathBuf};

/// The crates that ship. Test crates and dev tools are exempt: they run
/// deliberately, on a developer's machine, against a cluster they chose.
const SHIPPED: [&str; 6] = ["bridge", "cluster", "config", "scope", "store", "ui"];

/// Every `.rs` file in the shipped crates, with its path.
pub fn sources() -> Vec<(PathBuf, String)> {
    let root = workspace_root();
    let mut found = Vec::new();

    for crate_name in SHIPPED {
        collect(
            &root.join("crates").join(crate_name).join("src"),
            &mut found,
        );
    }

    assert!(
        found.len() > 40,
        "expected the whole workspace, found {} files — has the layout changed?",
        found.len()
    );
    found
}

/// The repository root, from this crate's manifest.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the guardrail crate lives two levels below the root")
        .to_path_buf()
}

fn collect(directory: &Path, found: &mut Vec<(PathBuf, String)>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let text = std::fs::read_to_string(&path).expect("a source file is readable");
            found.push((path, text));
        }
    }
}

/// Lines of a file, numbered from one, with the trailing `#[cfg(test)]` module
/// removed.
///
/// Tests build the very strings the rules below forbid — that is how they prove
/// the redaction works — so scanning them would make the guardrail impossible to
/// satisfy.
///
/// The cut is at the **first unindented** `#[cfg(test)]`, which is the `mod
/// tests` every file in this workspace puts last. It used to be the first
/// `#[cfg(test)]` anywhere, and that was silently wrong: a test-only accessor
/// carries the same attribute indented, in the middle of a file, and
/// `workspace.rs` grew one. The scan stopped at line 1015 of 4680 and exempted
/// three and a half thousand lines of the largest, fastest-changing file in the
/// repository — the one most worth watching — from every rule here.
pub fn production_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines()
        .enumerate()
        .take_while(|(_, line)| *line != "#[cfg(test)]")
        .map(|(at, line)| (at + 1, line))
}
