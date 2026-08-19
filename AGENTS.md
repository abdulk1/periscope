# Working on Periscope

Read this first. It is the operating manual for anyone — human or agent —
changing this repository. It is short on purpose; the detail lives in the four
documents it links to.

Nothing here is specific to one assistant. `CLAUDE.md` points at this file
because Claude Code looks for that name; if your tool reads
`.github/copilot-instructions.md`, `.cursorrules` or something else, point it
here rather than copying the contents, so there is one place to keep true.

Periscope is a native, GPU-accelerated Kubernetes console written in Rust with
GPUI. The binary is `scope`. It watches clusters, streams logs, and changes
things people care about, which is why most of the rules below exist.

## The rules that are not negotiable

These come from `IMPLEMENTATION.md` and are the reason to trust this program at
all. Breaking one is not a trade-off to weigh; it is a defect.

1. **No credentials are ever written to disk.** There is no cache, no token
   store, no keychain entry. `crates/config/src/paths.rs` says so in a comment
   and there must not be a path added that changes it.
2. **No telemetry, no phone-home, no crash reporting.** The single exception is
   the update check, which is off by default, makes one request, and is
   described in `crates/config/src/updates.rs`.
3. **Tokens and cluster hostnames are redacted from log output.** The audit log
   records context *names*, never server URLs.
4. **Secrets are masked by default**, with an explicit reveal that re-fetches.
5. **Every mutation is confirmed** by a sentence naming the cluster, namespace,
   object and operation — generated from the same value that will be sent, so
   the two cannot drift.
6. **Read-only is enforced twice**: in the store, which is what the UI asks, and
   again in the cluster layer immediately before the request (ADR-0028). A bug
   in the view must not be enough to change a protected cluster.
7. **Every attempt is written to the audit log** — applied, dry-run, refused or
   failed.
8. **GPUI is pinned exactly** and the vendored copy is never modified (ADR-0001).

## The loop

Everything must be green before anything is committed:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

Against a real cluster — needed for anything touching `crates/cluster`, and
worth running for anything that changes what reaches the screen:

```sh
kind create cluster --name periscope
PERISCOPE_E2E_REQUIRE_FIXTURES=1 cargo test -p periscope-e2e -- --ignored --test-threads 1
```

See [`docs/TESTING.md`](docs/TESTING.md) for the fixtures that needs, and for
what each kind of test in this repository is for.

Then run it. A change that compiles and passes tests can still be wrong on
screen:

```sh
cargo build --release --bin scope && ./target/release/scope
```

and read `~/Library/Application Support/dev.periscope.Periscope/logs/` for
panics. On macOS you can photograph the window itself — see
[`tools/winid/README.md`](tools/winid/README.md). Do **not** reach for
`screencapture -R <rect>`: it captures whatever is on that part of the display,
which is usually somebody else's window and none of your business.

## Where things go

| Crate | Holds | May not touch |
|---|---|---|
| `crates/scope` | flags, window setup, wiring | Kubernetes directly |
| `crates/ui` | GPUI views | `kube` |
| `crates/store` | state, filtering, indexes, permissions | GPUI, `kube` |
| `crates/cluster` | clients, watches, logs, mutations | GPUI |
| `crates/bridge` | the tokio ↔ GPUI protocol | `kube` |
| `crates/config` | paths, settings, audit, logging | GPUI, `kube` |

The dependency edges are the architecture and the compiler enforces them. If a
change wants to cross one, the design is wrong — see
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md), which also says where a new
feature usually belongs.

## How to write it

[`docs/STYLE.md`](docs/STYLE.md). The short version: comments explain *why*,
never *what*; test names are sentences about behaviour; no `unwrap()` outside
tests; every error says what failed, which cluster or object, and what to try.

## Security is enforced, not remembered

Four of the eight invariants above were held by discipline alone until a review
found two of them already broken. They are now held by code, and the code is
checked on every push:

```sh
cargo test -p periscope-guardrails   # the invariants no type can express
cargo audit                          # RUSTSEC advisories against Cargo.lock
cargo deny check                     # licences, unmaintained crates, duplicates
```

`tests/guardrails` reads the source and fails the build when a shape that has
already leaked reappears — a `tracing::` call carrying a cluster URL or a token,
an audit entry written without `redact::text`, a file written without
`paths::restrict`. Each test names the incident it exists for.

Before you commit anything that touches a cluster, a credential, or a file:

- **Does it write anything down?** Log lines, audit entries and exported files
  outlive the session and get attached to bug reports. Everything persisted goes
  through `periscope_cluster::redact`; what is on screen does not, because the
  person at the keyboard already holds the credentials.
- **Does it reach the apiserver?** If it can change anything — including
  `pods/exec` and `pods/portforward`, which are `create` verbs — it passes the
  store's gate *and* the cluster layer's `WritePolicy`, and it is audited,
  including when it is refused.
- **Does it name a cluster?** Capture the `ClusterId` where the decision is
  made, not where it is executed. A mutation that resolves its cluster twice
  can be confirmed against one and sent to another.
- **Did you add a dependency?** `cargo audit` and `cargo deny check` before you
  commit, not after CI says so.

If a guardrail fails, the fix is almost never to relax it. If it is genuinely
wrong, fix the rule and say why in the commit message.

## When you finish something

1. `docs/LIMITATIONS.md` — rewrite what your change made true or false. This
   document is the project's honesty, and it is worth more than the feature.
2. `docs/DECISIONS.md` — append an ADR for anything a future reader would
   otherwise have to reverse-engineer. Append only; supersede, never rewrite.
3. Commit with a message that explains the reasoning, not a changelog. Read
   `git log` for the voice.

## Traps this project has already fallen into

Every one of these cost real time. They are here so they cost nobody else any.

- **CI cancels superseded runs.** The `kind` job takes 25+ minutes; pushing
  again inside that window kills it. It went four runs without ever reaching a
  verdict for exactly this reason. Push, then wait.
- **Fixtures that live only on your machine are not fixtures.** Three e2e tests
  depended on a CRD, two operators and a large ConfigMap that were never
  committed, and passed locally for weeks while proving nothing in CI.
- **`~/Library/…/Periscope` is macOS.** XDG lowercases it. Three tests asserted
  the capitalised form and failed the first time Linux ran them.
- **A single-letter key binding steals from every text field** unless it is
  scoped `!Input` — GPUI dispatches from the focused element upwards. See
  `context_of` in `crates/ui/src/workspace.rs`, and ADR-0035.
- **A hidden element can keep focus.** Closing the palette used to leave focus
  on its input, which silently disabled every key bound outside text fields.
- **A zero-length animation divides by zero** in GPUI and panics.
- **`serde_json::Map::remove` is a swap-remove** under `preserve_order`, so
  removing a key reorders the rest. Use `shift_remove`.
- **Golden files must never be updated to match a bug.** If a rendering changed,
  decide which one is right before regenerating.

## Working with subagents

The rules that made parallel work land cleanly here:

- **Split by file, not by feature.** Three tasks that all edit
  `crates/ui/src/workspace.rs` will conflict; run those one after another. Tasks
  in different crates can run at once in separate worktrees.
- **Give each one the house rules.** An agent that has not read `docs/STYLE.md`
  writes comments that restate the code and tests called `test_foo`.
- **Ask for the reasoning back.** "What did you decide not to build, and why"
  catches more than a diff review does — two of this project's best decisions
  are agents declining to build something and saying why.
- **Never drive the running application with synthetic keystrokes.** One agent
  did; the focus went somewhere else and it typed into the operator's session.
  Use the test harness, or `tools/winid` and a screenshot.
- **Read-only means do not author changes *and* do not undo them.** A reviewer
  ran concurrently with a writer, decided the writer's uncommitted work was a
  rogue agent, and reverted the tree four times. Say so explicitly when you ask
  for a review, and expect a writer's edits to be in the tree while it runs.
