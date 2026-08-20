//! The two ways a secret has actually escaped, and one that would be next.
//!
//! Each test names the incident it exists for. When one fails, the fix is
//! almost never to relax the test.

use periscope_guardrails::{production_lines, sources};

/// Things that must never be interpolated into a `tracing::` call.
///
/// The macro takes `%value` or `field = %value`, so the check is for the
/// identifier appearing inside a `tracing::` invocation at all.
const NEVER_LOGGED: [(&str, &str); 4] = [
    (
        "cluster_url",
        "the apiserver endpoint — on EKS it carries the account id and the region",
    ),
    ("config.cluster_url", "same, by another name"),
    (".token", "a bearer token"),
    ("kubeconfig", "a whole kubeconfig, which holds credentials"),
];

#[test]
fn nothing_that_identifies_a_cluster_is_logged() {
    // The incident: `building client … server=https://…` was written on every
    // connect at the default verbosity, 35 times in one day on the author's
    // machine, into a file people attach to bug reports.
    let mut offences = Vec::new();

    for (path, text) in sources() {
        let mut inside = false;
        for (number, line) in production_lines(&text) {
            if line.contains("tracing::") {
                inside = true;
            }

            if inside {
                for (needle, why) in NEVER_LOGGED {
                    if line.contains(needle) {
                        offences.push(format!("{}:{number}: {needle} — {why}", path.display()));
                    }
                }
            }

            // A `tracing!` call ends at the line holding its closing paren.
            if inside && line.trim_end().ends_with(");") {
                inside = false;
            }
        }
    }

    assert!(
        offences.is_empty(),
        "a log call carries something that must not be written down:\n{}\n\n\
         If this is genuinely safe, it still should not be logged — put the \
         redacted projection through `periscope_cluster::redact` instead.",
        offences.join("\n")
    );
}

#[test]
fn the_audit_log_is_only_ever_written_through_the_redactor() {
    // The incident: `describe()` walks a whole error chain, and `kube`'s
    // `AuthExecRun` Debug-prints a failed credential plugin's entire stdout —
    // which is where an ExecCredential token lives. That reached `AuditEntry`.
    let mut offences = Vec::new();

    for (path, text) in sources() {
        let lines: Vec<_> = production_lines(&text).collect();

        for (index, (number, line)) in lines.iter().enumerate() {
            if !line.trim_start().starts_with(".outcome(") {
                continue;
            }

            // The call is often wrapped across several lines, so the statement
            // is what is checked: from `.outcome(` to the semicolon that ends
            // it. A line-at-a-time rule would push people into writing it on
            // one long line to satisfy the scanner, which helps nobody.
            let mut statement = String::new();
            for (_, text) in &lines[index..] {
                statement.push(' ');
                statement.push_str(text);
                if text.trim_end().ends_with(';') {
                    break;
                }
            }

            if !statement.contains("redact::text") {
                offences.push(format!("{}:{number}: {}", path.display(), statement.trim()));
            }
        }
    }

    assert!(
        offences.is_empty(),
        "an audit entry's reason is written without redaction:\n{}\n\n\
         Wrap it in `crate::redact::text`. What is on screen keeps the \
         apiserver's exact words; what outlives the session does not.",
        offences.join("\n")
    );
}

#[test]
fn everything_that_persists_is_restricted_to_its_owner() {
    // Not an incident yet. The audit log, `state.toml` and exported log buffers
    // were all world-readable, which only mattered because of the two above —
    // so this is the rule that keeps the next leak from being readable by every
    // account on the machine.
    let mut offences = Vec::new();

    /// How far after a write `restrict` may appear. Every real call site does it
    /// on the next line or the one after; the slack is for a `?` split across
    /// lines or an intervening `flush`.
    const WINDOW: usize = 6;

    for (path, text) in sources() {
        // `paths::restrict` is the one place allowed to talk about modes.
        if path.ends_with("paths.rs") {
            continue;
        }

        let lines: Vec<_> = production_lines(&text).collect();
        for (index, (number, line)) in lines.iter().enumerate() {
            if !(line.contains("fs::write(") || line.contains(".append(true)")) {
                continue;
            }

            // Checked against the following few lines rather than the whole
            // file. A file-wide `contains("restrict")` meant one restricted
            // write anywhere exempted every other write in the same file —
            // which is the shape of exemption that grows quietly.
            let restricted = lines[index..]
                .iter()
                .take(WINDOW)
                .any(|(_, near)| near.contains("restrict"));
            if !restricted {
                offences.push(format!("{}:{number}: {}", path.display(), line.trim()));
            }
        }
    }

    assert!(
        offences.is_empty(),
        "a file is written without being restricted to its owner:\n{}\n\n\
         Call `periscope_config::paths::restrict` after writing it.",
        offences.join("\n")
    );
}

#[test]
fn the_scan_does_not_stop_at_the_first_test_only_item() {
    // The split used to cut at the first `#[cfg(test)]` anywhere in a file.
    // `workspace.rs` carries that attribute, indented, on two test-only
    // accessors — so the scan stopped at line 1015 of 4680 and quietly exempted
    // three and a half thousand lines of the largest and fastest-changing file
    // in the repository. Nothing was leaking; nothing was watching either.
    //
    // The canary is the export write at ~1898: a real `fs::write` of cluster
    // log text, sitting well past the old cut, and exactly the kind of line
    // these rules exist to police.
    let (_, text) = sources()
        .into_iter()
        .find(|(path, _)| path.ends_with("workspace.rs"))
        .expect("the workspace view is part of the shipped source");

    let write = production_lines(&text).find(|(_, line)| line.contains("fs::write("));
    let (number, _) = write.expect(
        "the export write is not inside the scanned region; the split found a \
         `#[cfg(test)]` that is not the trailing test module",
    );
    assert!(
        number > 1015,
        "expected it past the old cut, found line {number}"
    );
}

#[test]
fn the_guardrails_can_actually_fail() {
    // A test that cannot fail is decoration. These scan real files, so the
    // check is that the scanner sees something to scan and that the rules match
    // what they claim to.
    let files = sources();
    assert!(files.iter().any(|(path, _)| path.ends_with("redact.rs")));

    let sample = "tracing::info!(\n    server = %config.cluster_url,\n);\n";
    let caught = production_lines(sample).any(|(_, line)| line.contains("cluster_url"));
    assert!(caught, "the scanner would not catch the original incident");
}
