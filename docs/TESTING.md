# How Periscope is tested

Six kinds of test, each answering a different question. Knowing which one you
need is most of the work.

| Kind | Answers | Needs |
|---|---|---|
| Unit | is this logic right | nothing |
| Golden | does this render exactly as agreed | nothing |
| UI | does this behave when keys are pressed | a headless GPUI window |
| End-to-end | does this work against a real apiserver | a `kind` cluster |
| Fault injection | what happens when it breaks | a cluster and a proxy |
| Budget | is it still fast enough | a cluster, and honesty |

## Unit

The bulk of it, and the reason `store`, `cluster` and `config` have no GPUI
dependency: their logic is testable without a window or a cluster.

```sh
cargo test --workspace
```

Live beside the code in `#[cfg(test)] mod tests`. Fixtures are JSON literals of
real Kubernetes objects — see `crates/cluster/src/columns.rs` for the pattern.

## Golden

The YAML the detail pane shows is pinned to files, because it is long, exact,
and easy to break without noticing.

```sh
cargo test -p periscope-cluster --test golden
PERISCOPE_UPDATE_GOLDEN=1 cargo test -p periscope-cluster --test golden   # regenerate
```

Fixtures are `crates/cluster/tests/golden/*.json` with a `.yaml` beside each.
Add one whenever a rendering rule is subtle: block scalars, quoting that must
survive (`"true"`, `"0755"`), annotations containing colons and newlines,
masked and revealed Secrets.

**Never regenerate a golden to make a test pass.** If the output changed, decide
which version is correct first. Three real bugs in the YAML writer were found by
goldens; all three were fixed in the writer, not baked into the file.

## UI

Real GPUI windows, driven by real key dispatch — not by calling handlers
directly, which is how two keybinding collisions were caught.

```rust
let (harness, rx) = workspace(cx);
harness.keys(cx, "cmd-k");
assert!(harness.read(cx, |workspace| workspace.palette_open()));
```

They live at the bottom of `crates/ui/src/workspace.rs`. They can also read
painted layout back — `ScrollHandle::bounds_for_item` is how line wrapping is
asserted — but they cannot tell you whether something *looks* right. For that,
`tools/winid` and a screenshot.

## End-to-end

Everything here talks to a real apiserver, so it is `#[ignore]`d and opted into.

```sh
kind create cluster --name periscope
PERISCOPE_E2E_REQUIRE_FIXTURES=1 cargo test -p periscope-e2e -- --ignored --test-threads 1
```

`--test-threads 1` keeps several watch streams from racing for one small
cluster. `PERISCOPE_E2E_CONTEXT` points the suite at a different context.

### Fixtures, and the rule about them

```sh
kubectl apply -f tests/e2e/fixtures/chatty.yaml     # log producers
kubectl apply -f tests/e2e/fixtures/webby.yaml      # serves a port; runs commands
kubectl apply -f tests/e2e/fixtures/sidecars.yaml   # two containers, for exec
kubectl apply -f tests/e2e/fixtures/widgets.yaml    # a CRD of our own
kubectl apply -f tests/e2e/fixtures/firehose.yaml   # unthrottled; delete when done
cargo run --release -p periscope-e2e --bin seed-pods -- --count 10000
cargo run --release -p periscope-e2e --bin seed-pods -- --large-config-map
```

A test that needs a fixture **skips** with instructions when it is missing, and
**fails** when `PERISCOPE_E2E_REQUIRE_FIXTURES` is set. CI sets it. Skipping is
right on a laptop, where nobody wants the firehose burning a core all day; it is
wrong in CI, where a suite that skips itself is indistinguishable from one that
passes. Fifteen tests were silently absent from every pull request before that
existed, and three more depended on cluster state that was never committed at
all.

So: **if a test needs something in the cluster, that something lives in this
repository.** `periscope_e2e::require` and `periscope_e2e::fixture` are how a
test states the dependency.

`seed-pods` is the only thing in the repository that writes to a cluster, apart
from the tests that create what they destroy. It prints the cluster URL before
it writes anything.

## Fault injection

`IMPLEMENTATION.md` §4 lists these as tests, not edge cases: kill the apiserver
mid-watch, expire a token mid-session, saturate the log stream. All three exist.

The apiserver is not really killed — that would take the cluster down for every
other test in the run. `periscope_e2e::proxy` interposes a TCP proxy and cuts
it: established connections reset, new ones refused, bytes otherwise copied
verbatim so TLS still verifies against the cluster's own CA. It found a real
defect the first time it ran (ADR-0034).

## Budgets

`IMPLEMENTATION.md` §4 sets numbers; the tests enforce them and
`docs/LIMITATIONS.md` records what was measured.

Two things to know before adding one:

- **Debug builds are several times slower.** A budget asserted without a debug
  allowance fails on a clean checkout, which happened to two tests here and made
  `cargo test --workspace` unreliable. Use `cfg!(debug_assertions)` and say why.
- **Say what you did not measure.** The frame numbers measure redraw, not
  scrolling; the five-cluster memory figure is five contexts against one
  apiserver. Both are written down as such.

## What a green suite still does not prove

`docs/LIMITATIONS.md` has an "Unverified" section listing exactly this: mouse
interaction, anything that only exists mid-gesture, EKS, Windows at runtime.
Keep it current. A test suite that is trusted beyond its reach is worse than a
small one.
