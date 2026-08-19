//! Golden tests for the YAML the detail pane shows.
//!
//! Every `.json` file in `tests/golden/` is one Kubernetes object; the `.yaml`
//! file beside it is exactly what the writer must produce for it. A Secret also
//! has a `.masked.yaml`, which is what the pane shows before the user asks to
//! see the values.
//!
//! These need no cluster: the fixtures are the objects an apiserver would have
//! returned, so the whole thing is `serde_json` in and text out.
//!
//! After a deliberate change to the writer, rewrite every expectation rather
//! than editing them by hand:
//!
//! ```sh
//! PERISCOPE_UPDATE_GOLDEN=1 cargo test -p periscope-cluster --test golden
//! ```
//!
//! Then read the diff. A golden file is only worth having if someone looked at
//! it — an expectation updated without being read records the bug instead of
//! catching it.

use std::path::{Path, PathBuf};

use kube::api::DynamicObject;
use periscope_cluster::detail::to_yaml;
use serde_json::Value;

/// Set to rewrite the expectations instead of comparing against them.
const UPDATE: &str = "PERISCOPE_UPDATE_GOLDEN";

/// One fixture rendered one way, and the file that says what should come out.
struct Case {
    object: DynamicObject,
    expectation: PathBuf,
    mask: bool,
}

#[test]
fn every_fixture_renders_the_yaml_beside_it() {
    for case in cases() {
        let rendered = to_yaml(&case.object, case.mask).expect("the fixture renders");
        compare(&case.expectation, &rendered);
    }
}

#[test]
fn every_rendering_reads_back_as_the_object_it_came_from() {
    // A golden file only proves the output has not changed, not that it was
    // ever YAML. Reading it back with a real parser proves both: a value that
    // lost its quoting, its newlines or its indentation either fails to parse
    // or comes back as something else.
    for case in cases() {
        // Masking replaces values on purpose, so there is nothing to compare
        // them with; the expectation file is what pins that down.
        if case.mask {
            continue;
        }

        let rendered = to_yaml(&case.object, false).expect("the fixture renders");
        let parsed = periscope_cluster::yaml::parse(&rendered).unwrap_or_else(|error| {
            panic!(
                "{} does not render valid YAML: {error}\n{rendered}",
                case.expectation.display()
            )
        });

        let original = serde_json::to_value(&case.object).expect("the fixture is a JSON value");
        assert_says_the_same(&parsed, &original, &case.expectation.display().to_string());
    }
}

/// Asserts that every value the rendering carries is the value the object had.
///
/// The writer drops noisy metadata deliberately, so the reading is a subset of
/// the object rather than a copy of it — what it must never do is change a
/// value on the way through. Anything wrongly left out is caught by the
/// expectation file instead.
fn assert_says_the_same(read: &Value, original: &Value, path: &str) {
    match (read, original) {
        (Value::Object(read), Value::Object(original)) => {
            for (key, value) in read {
                let was = original
                    .get(key)
                    .unwrap_or_else(|| panic!("{path}/{key} was never in the object"));
                assert_says_the_same(value, was, &format!("{path}/{key}"));
            }
        }
        (Value::Array(read), Value::Array(original)) => {
            assert_eq!(read.len(), original.len(), "{path} changed length");
            for (index, (value, was)) in read.iter().zip(original).enumerate() {
                assert_says_the_same(value, was, &format!("{path}/{index}"));
            }
        }
        (read, original) => assert_eq!(read, original, "{path} came back different"),
    }
}

#[test]
fn no_expectation_is_left_without_a_fixture() {
    // Deleting a fixture and leaving its expectation behind would look exactly
    // like a case that is still covered.
    let expected: Vec<PathBuf> = cases().into_iter().map(|case| case.expectation).collect();
    for entry in read_dir() {
        if entry.extension().is_some_and(|kind| kind == "yaml") {
            assert!(
                expected.contains(&entry),
                "{} has no fixture; delete it or add one",
                entry.display()
            );
        }
    }
}

/// Every fixture, in a stable order, with the ways it is rendered.
fn cases() -> Vec<Case> {
    let mut cases = Vec::new();
    for path in read_dir() {
        if path.extension().is_none_or(|kind| kind != "json") {
            continue;
        }

        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let value: Value = serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("{} is not JSON: {error}", path.display()));
        let object: DynamicObject = serde_json::from_value(value)
            .unwrap_or_else(|error| panic!("{} is not an object: {error}", path.display()));

        if is_secret(&object) {
            cases.push(Case {
                object: object.clone(),
                expectation: path.with_extension("masked.yaml"),
                mask: true,
            });
        }
        cases.push(Case {
            object,
            expectation: path.with_extension("yaml"),
            mask: false,
        });
    }

    assert!(!cases.is_empty(), "no fixtures in {}", dir().display());
    cases
}

/// Whether the detail pane would hide this object's values.
///
/// The same rule `detail::fetch` applies, so the masked expectation exists for
/// exactly the objects a user would see masked.
fn is_secret(object: &DynamicObject) -> bool {
    object
        .types
        .as_ref()
        .is_some_and(|types| types.api_version == "v1" && types.kind == "Secret")
}

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// The golden directory, sorted, so a failure names the same file every run.
fn read_dir() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir())
        .unwrap_or_else(|error| panic!("{}: {error}", dir().display()))
        .map(|entry| entry.expect("the directory can be read").path())
        .collect();
    paths.sort();
    paths
}

/// Compares a rendering with its expectation, or rewrites it when asked to.
fn compare(expectation: &Path, rendered: &str) {
    if std::env::var_os(UPDATE).is_some() {
        std::fs::write(expectation, rendered)
            .unwrap_or_else(|error| panic!("{}: {error}", expectation.display()));
        return;
    }

    let expected = std::fs::read_to_string(expectation).unwrap_or_else(|error| {
        panic!(
            "{}: {error} — run with {UPDATE}=1 to write it",
            expectation.display()
        )
    });

    // `assert_eq!` escapes both sides onto one line, which is unreadable for a
    // page of YAML; the point of a golden failure is that you can see it.
    assert!(
        rendered == expected,
        "{} is out of date.\n\n--- expected ---\n{expected}--- rendered ---\n{rendered}\
         \nRe-run with {UPDATE}=1 to accept the rendering, then read the diff.",
        expectation.display()
    );
}
