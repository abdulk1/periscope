# Architecture decision log

Append only. Never rewrite an entry; supersede it with a new one.

---

## ADR-0001 — GPUI comes from crates.io, pinned to an exact version

**Date:** 2026-08-17
**Status:** Accepted
**Supersedes:** the instruction in `IMPLEMENTATION.md` §0.3 / §2.4 to pin GPUI to
a git commit SHA on `zed-industries/zed`.

### Context

The plan states that GPUI "is not published to crates.io" and therefore must be
pinned to an exact git SHA. That was true when the plan was written. It is no
longer true:

| Crate | crates.io | Latest release (checked 2026-08-17) |
|---|---|---|
| `gpui` | published | `0.2.2`, published 2025-10-22 |
| `gpui_macros` | published | `0.2.2` |
| `gpui-component` | published | `0.5.1`, published 2026-02-05 |
| `gpui-component-assets` | published | `0.5.1` |
| `gpui_platform` | **not published** | — (git only) |

The git route also has a concrete blocker. `gpui-component`'s own
`Cargo.toml` on `main` declares its GPUI dependency **without a `rev`**:

```toml
gpui = { version = "0.2.2", git = "https://github.com/zed-industries/zed", features = ["profiler"] }
```

Cargo treats `git = <url>` and `git = <url>, rev = <sha>` as two *different*
sources. If we pinned a SHA while `gpui-component` floated, we would link two
incompatible copies of GPUI and every type would mismatch. The usual escape
hatch — `[patch]` — is rejected by Cargo when the replacement points at the same
URL as the original ("patches must point to different sources"). Following
`gpui-component`'s floating dep instead would give us a pin that drifts whenever
`zed` `main` moves, which is precisely what §0.3 exists to prevent.

### Decision

Depend on the crates.io releases with exact `=` requirements:

```toml
gpui = "=0.2.2"
gpui-component = "=0.5.1"
gpui-component-assets = "=0.5.1"
```

`=` plus a committed `Cargo.lock` gives byte-identical builds — the same
guarantee the SHA pin was after — while avoiding both the source-unification
problem and a multi-gigabyte `zed` checkout in CI.

### Consequences

- Upgrading GPUI remains a deliberate, isolated task with its own commit, exactly
  as §0.3 requires. Only the mechanism changed.
- We are ~10 months behind GPUI `main`. Anything that exists only on `main` is
  unavailable until the next crates.io release. If a Phase 1–3 requirement turns
  out to need it, revisit this ADR; the fallback is to pin *both* `zed` and
  `gpui-component` to git SHAs and vendor `gpui-component`'s manifest with an
  added `rev`, which is a far larger commitment.
- `gpui_platform` being unpublished confirms the crates.io release is a curated
  subset, not a mirror of `main`.

---

## ADR-0002 — Rust toolchain is 1.97.1

**Date:** 2026-08-17
**Status:** Accepted

`gpui` and `gpui-component` are both edition 2024, and the workspace uses
let-chains (stable since 1.88). The machine had 1.85.1; `rustup update stable`
brought 1.97.1, which is also the version `zed` itself pins in its
`rust-toolchain.toml`. `rust-version = "1.85"` in the workspace manifest is the
declared floor; CI builds on stable.

No `rust-toolchain.toml` is committed. Pinning the toolchain file would force
every contributor onto one exact compiler for a project with no toolchain-specific
requirements; the MSRV field plus CI on stable is sufficient.

---

## ADR-0003 — tokio lives on its own thread, never on GPUI's executors

**Date:** 2026-08-17
**Status:** Accepted

`kube-rs` requires a tokio reactor; GPUI owns the main thread and runs its own
`ForegroundExecutor`/`BackgroundExecutor`. Rather than reconciling them,
`ClusterRuntime` (`crates/bridge/src/runtime.rs`) builds a multi-threaded tokio
runtime on a dedicated `std::thread` and holds it for the process lifetime. The
two worlds meet only at `flume` channels.

`ClusterRuntime` is stored as a GPUI `Global`, so GPUI's own teardown drops it,
which closes the command channel, ends the dispatch loop, gives in-flight work a
5s grace period, and joins the thread. No `Box::leak`, no orphaned runtime.

Consequence: nothing in `crates/cluster` may assume it can touch a GPUI type, and
nothing in `crates/ui` may `block_on`. The compiler enforces the first (the
`cluster` crate has no `gpui` dependency); the second is a review rule.

---

## ADR-0004 — Coalescing is keyed, and time is injected

**Date:** 2026-08-17
**Status:** Accepted

The spec requires batching so a 10k-object resync does not produce 10k UI
mutations. Batching alone is not enough: 10k events for the *same* object should
also collapse. `Coalescer` (`crates/bridge/src/coalesce.rs`) therefore keys
pending items — a newer event replaces an older one with the same key *in place*,
preserving batch order — and `ClusterEvent::coalesce_key()` returns `None` for
events that must all be delivered (a `Pong` answers a specific ping; collapsing
pongs would lose replies).

`Coalescer` takes `now: Instant` as a parameter rather than reading a clock. The
deadline is set on the first push of a batch and is deliberately *not* extended by
later pushes, so a continuous event stream cannot starve the UI forever. Both
properties are covered by tests that never sleep.

---

## ADR-0005 — Channels are bounded, and the two directions overflow differently

**Date:** 2026-08-17
**Status:** Accepted

The spec says all channels are bounded and overflow should "drop-and-mark-stale
rather than block the cluster layer". That is right for events but wrong for
commands, so the two directions differ:

- **Events (cluster → UI)** drop on overflow and bump a counter. The pump reads
  the counter each flush and reports it as `FlushStats::dropped`; the store marks
  affected clusters stale. Blocking here would apply backpressure all the way up
  the Kubernetes watch stream.
- **Commands (UI → cluster)** are never dropped silently. A lost command is a
  button that did nothing. `CommandSender::send` returns
  `CommandError::Backpressure`, and the UI renders it.

---

## ADR-0006 — Crate boundaries are enforced by dependency edges

**Date:** 2026-08-17
**Status:** Accepted

| Crate | May depend on GPUI | May depend on kube |
|---|---|---|
| `scope` | yes | no (wires only) |
| `periscope-ui` | yes | no |
| `periscope-bridge` | yes | no |
| `periscope-store` | **no** | no |
| `periscope-cluster` | **no** | yes (Phase 1) |
| `periscope-config` | **no** | no |

`store`, `cluster` and `config` having no GPUI dependency is what makes their
logic testable without a window, and is why the Phase 0 test suite runs headless
in CI. `bridge` depends on GPUI only for `spawn_event_pump`; everything else in
it is plain data.

---

## ADR-0007 — Phase 0's cluster layer is a health handler, not a kube client

**Date:** 2026-08-17
**Status:** Accepted

Phase 0's acceptance criteria require a proven round trip, not a cluster
connection. `crates/cluster` therefore ships `HealthHandler` — an async
`CommandHandler` that answers `Ping` with `Pong` — and does **not** yet depend on
`kube`. This keeps the Phase 0 build fast and keeps the read-only invariant
trivially true.

Versions verified on 2026-08-17 for Phase 1, when they will be added:
`kube 4.2.0`, `k8s-openapi 0.28.0`.

---

## ADR-0008 — Metal shaders are compiled at runtime, not at build time

**Date:** 2026-08-17
**Status:** Accepted

### Context

On macOS, `gpui`'s build script invokes `xcrun metal` to compile
`shaders.metal` into a `.metallib` at build time. On this machine that fails:

```
cargo::error=metal shader compilation failed:
error: cannot execute tool 'metal' due to missing Metal Toolchain;
use: xcodebuild -downloadComponent MetalToolchain
```

Xcode 26 ships the Metal compiler as a separately-downloaded component. It is a
multi-gigabyte download, and `xcodebuild` on this machine is itself degraded
(`DVTDownloads.framework` is missing, so plug-in loading fails and it asks for
`xcodebuild -runFirstLaunch`).

### Decision

Enable `gpui`'s `runtime_shaders` feature. The build script then stitches the
generated header into the shader source and ships it as text; Metal compiles it
at process start. No build-time Metal toolchain is required.

This is the same configuration `gpui-component` uses upstream for its own
workspace (`gpui_platform = { …, features = […, "runtime_shaders"] }`), so it is
a supported path rather than a workaround we invented.

### Consequences

- Shader compilation moves into startup. This is a direct cost against the
  <500ms cold-start budget in §4 and must be re-measured whenever the budget is
  checked; the measured figure is in the Phase 0 report.
- Anyone who *does* have the Metal toolchain installed can drop the feature for a
  marginally faster start. It is one line in the workspace manifest.
- CI on macOS runners does not need to download the toolchain, which keeps the
  pipeline fast.
