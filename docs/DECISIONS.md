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

- Shader compilation moves into startup. Measured cost: the first ever run takes
  527ms to first paint, subsequent release runs 122–158ms (Metal caches the
  compiled shaders). The <500ms budget in §4 therefore holds for release builds
  from the second run onward, and the very first launch on a new machine exceeds
  it slightly. Re-measure whenever this feature changes.
- Anyone who *does* have the Metal toolchain installed can drop the feature for a
  marginally faster start. It is one line in the workspace manifest.
- CI on macOS runners does not need to download the toolchain, which keeps the
  pipeline fast.

---

## ADR-0009 — The MSRV floor is 1.89

**Date:** 2026-08-18
**Status:** Accepted
**Amends:** ADR-0002, which declared `rust-version = "1.85"`.

`kube 4.2.0` declares `rust-version = "1.89.0"`. Cargo will silently resolve a
*older* `kube` — 2.0.1 — rather than break a 1.85 floor, which would have given
us a two-major-version-old client without saying so. The floor moves to 1.89 so
the dependency we actually reviewed is the one that gets built.

The installed toolchain (1.97.1, ADR-0002) is unaffected. CI still builds on
stable; the floor is a declaration, not a pin.

---

## ADR-0010 — `kube 4.2` with `k8s-openapi` pinned to the `v1_34` API

**Date:** 2026-08-18
**Status:** Accepted
**Supersedes:** the version note in ADR-0007.

### Decision

```toml
kube = { version = "4.2", features = [
    "client", "runtime", "ws", "oidc", "oauth", "socks5", "http-proxy", "gzip",
] }
k8s-openapi = { version = "0.28", features = ["v1_34"] }
```

`kube` pins `kube-client`, `kube-core` and `kube-runtime` to its own version
internally, so one version requirement fixes all four.

### Why these features

§2.5 makes enterprise auth non-negotiable: `oidc` and `oauth` cover OIDC refresh
and GCP application-default credentials, `socks5` and `http-proxy` cover
`HTTPS_PROXY`/`NO_PROXY` estates, `gzip` matters on a 10,000-object list. Exec
credential plugins and client certificates need no feature flag. `ws` is not used
until Phase 5's exec support but is enabled now so the TLS stack is not
reconfigured later. TLS comes from `kube`'s default `rustls-tls`.

### Why one API version, and why not the newest

`k8s-openapi` permits exactly one version feature per build, and it is a
workspace-wide choice. `v1_34` is two releases behind the newest it supports;
core `v1` types are additive and deserialisation ignores unknown fields, so the
generated types work against both older and newer apiservers — the `kind` cluster
these were verified against runs 1.36. Pinning to the newest would gain nothing
and would drift the moment a cluster is older.

---

## ADR-0011 — Kubernetes objects stop at the cluster layer

**Date:** 2026-08-18
**Status:** Accepted

`k8s-openapi` types never cross the bridge. `crates/cluster` projects each `Pod`
into a `PodSnapshot` (`crates/bridge/src/resource.rs`) — namespace, name, uid,
status text, ready counts, restarts, node, creation time — and that is what the
store and the UI see.

Consequences:

- The store and the UI have no kube dependency at all, so the entire rendering
  path is testable with hand-written fixtures and no cluster.
- The projection is a pure function with its own tests, including the parts of
  `kubectl`'s `printPod` that make a crash-looping pod read `CrashLoopBackOff`
  rather than `Running`.
- The cost is that anything not projected is unavailable to the UI. Phase 2's
  YAML and describe views need the whole object, so they will have to carry the
  raw object *alongside* the projection rather than replace it. That is a
  deliberate second step, not an oversight.

---

## ADR-0012 — Coalescing needs a barrier, not just a key

**Date:** 2026-08-18
**Status:** Accepted
**Extends:** ADR-0004.

Keyed collapsing replaces a pending event *in place* to keep batch order stable.
That is correct for independent objects and wrong for a resync: a `PodsReset`
carries a complete listing, so a pod update that arrives *after* it would be
moved back to the earlier slot its key already occupied, applied first, and then
wiped by the reset. The user would see a row silently revert.

`CoalesceKey` therefore has two extra behaviours: `is_barrier`, and `supersedes`.
A barrier key removes every pending item it supersedes and takes the end of the
batch, so nothing it invalidates can be applied after it. `EventKey::PodsReset`
is a barrier over that cluster's pod events, and over nothing else — another
cluster's pods and this cluster's connection status survive it, as do unkeyed
events that must all be delivered.

The scan is linear, and only barrier pushes pay for it; ordinary object updates
still take the hash-map path.

---

## ADR-0013 — A rejected credential ends the session; everything else retries

**Date:** 2026-08-18
**Status:** Accepted

Watch failures split in two:

- **401 and 403, or any `kube::Error::Auth`** — the watch stops, and the cluster
  goes to `AuthFailed` carrying the apiserver's text. Retrying a credential the
  cluster has already refused would hammer the API and, worse, would leave the UI
  looking merely slow. The user gets an explicit "Reconnect", which re-runs the
  exec plugin or OIDC refresh from scratch.
- **Everything else** — the cluster goes to `Degraded` with the reason and the
  watch keeps retrying under `kube`'s own backoff, returning to `Connected` when
  a listing completes again.

This is verified against a real apiserver, not a mock: `tests/e2e/tests/auth.rs`
points a junk bearer token at the `kind` cluster and asserts the state and the
message that come back.

---

## ADR-0014 — `KubeHandler` replaces `HealthHandler`, and kubeconfig is selectable

**Date:** 2026-08-18
**Status:** Accepted
**Supersedes:** ADR-0007.

Phase 0's `HealthHandler` is gone; `KubeHandler` owns one session per cluster —
a task holding a `kube::Client` and a pod watch — and answers `ListContexts`,
`Connect`, `Disconnect` and `Ping`. Sessions are independent tasks in a map, so a
cluster that is unreachable cannot stall another, and `Disconnect` aborts exactly
one of them. `Ping`/`Pong` survives as what it always was: a bridge liveness
probe that makes no API call.

`KubeHandler::with_kubeconfig(path)`, behind the `--kubeconfig` flag, reads one
specific file instead of `$KUBECONFIG`/`~/.kube/config`. It matches `kubectl`, and
it is what lets the auth-failure tests point a deliberately broken credential at
a real apiserver without touching the developer's own kubeconfig — the
alternative was mutating a process-wide environment variable from a test, which
needs `unsafe` under the 2024 edition and is forbidden workspace-wide.

---

## ADR-0015 — Frame rate is measured by forcing redraw, and only under `--perf`

**Date:** 2026-08-18
**Status:** Accepted

GPUI draws only when something invalidates the window. That makes an idle app
free — which is the point — and it makes "what is our frame rate" unanswerable by
observation: a quiet app measures as perfect and a busy one is only sampled while
it happens to be busy.

`--perf` therefore switches the window into continuous redraw (`request_animation_frame`
from `render`) and `FrameMeter` records the interval between consecutive renders,
reporting p50/p95/max, element-build time and late frames every 120 frames. Every
frame rebuilds the entire view including the visible rows, so the number is a
floor rather than a best case.

Two things this deliberately does not do:

- It does not run outside `--perf`. Continuous redraw on a laptop is a battery
  bug, not a feature.
- It does not report a single "fps" verdict. On a vsync-locked 60Hz panel the
  count of frames over the 16.67ms budget sits near half the frames however fast
  the app is, so the meter reports that raw count *and* `hitches` — intervals
  over twice the budget, where a frame was genuinely dropped. Only the second
  number means something is wrong.

---

## ADR-0016 — Every auth failure names the credential plugin

**Date:** 2026-08-18
**Status:** Accepted
**Extends:** ADR-0013.

Testing the exec-credential path found a message that broke the project's own
error rule. A kubeconfig pointing at a plugin that is not installed — by far the
most common GKE failure — produced:

```
auth error: unable to run auth exec: No such file or directory (os error 2)
```

True, and useless: it never says which binary. `kube` does not carry the command
into that error, so Periscope does. The `exec` command for the context is read
out of kubeconfig when the client is built and travels with the connection, and
`errors::attribute_plugin` appends it to any auth failure that does not already
mention it, on both paths that can raise one (client construction and the watch
stream). The message becomes:

```
auth error: unable to run auth exec: No such file or directory (os error 2)
  (credential plugin: `gke-gcloud-auth-plugin`)
```

---

## ADR-0017 — Every kind travels the same path, as projected rows plus columns

**Date:** 2026-08-18
**Status:** Accepted
**Extends:** ADR-0011.

Phase 2 has to render kinds nobody has compiled support for. Rather than a table
of types, the cluster layer watches everything as `DynamicObject` and projects
each object into a `ResourceRow`: a key, a state, and a vector of string cells.
The **column definitions travel with the rows** in the `ResourceReset` event.

Consequences:

- The UI has no knowledge of Kubernetes kinds at all. It renders whatever
  columns arrive, which is exactly why an Argo CD `Application` needs no code.
- Adding good columns for a kind is one function in `crates/cluster/src/columns.rs`
  with a unit test, and touches nothing else.
- Unknown kinds fall back to `STATUS` and `READY`, read from `status.phase` and a
  `Ready` condition. Those are conventions, not guarantees, so they render empty
  rather than wrong when a CRD does something else.
- A test asserts every built-in projector emits exactly as many cells as it
  declares columns; a mismatch would silently shift a whole table sideways.

---

## ADR-0018 — Full objects are fetched on demand, never cached

**Date:** 2026-08-18
**Status:** Accepted

The detail view needs the whole object; the tables need thirty bytes of it. If
the store kept both, memory would scale with the largest ConfigMap in the
cluster rather than with the number of rows — and a 10,000-object listing has to
stay under 300MB.

So `FetchObject` issues a `get` when the user opens something, and the result is
held only while that pane is open. The watch stream additionally clears
`managedFields` as objects arrive, which is most of the bytes in a typical
object and is never rendered.

The trade is one round trip per open — single-digit milliseconds against a local
cluster — in exchange for memory that does not depend on what is stored in the
cluster. It also means the YAML on screen is what the apiserver says *now*,
rather than what a watch event said some time ago.

---

## ADR-0019 — One watch at a time, replaced rather than accumulated

**Date:** 2026-08-18
**Status:** Accepted

Watching every kind the user has looked at would be the fastest way back to a
previous table and the fastest way to a cluster-wide bandwidth problem: a
watch per kind, per cluster, held forever.

The UI therefore keeps exactly one watch per cluster: switching kinds sends
`StopWatch` for the old one and `Watch` for the new. Rows already in the store
stay there — switching back is instant and shows the last known state — but
nothing is being streamed for a table nobody is looking at. A namespace or
selector change is the same mechanism: the watch is replaced, because those
filters are applied by the apiserver.

Phase 4 revisits this when several clusters are live at once and a per-cluster
budget has to decide what stays warm.

---

## ADR-0020 — The YAML view is written here, not by a serialiser

**Date:** 2026-08-18
**Status:** Accepted

`serde_yaml` is unmaintained, and every YAML crate needs post-processing to
match what `kubectl -o yaml` prints — sequence items at the same indent as their
key, block scalars for certificates, and quoting that keeps `"1.20"` a string.
Kubernetes objects are JSON-shaped, so the general YAML problem never arises:
maps, arrays, strings, numbers, booleans, null.

`crates/cluster/src/detail.rs` therefore contains a small writer, about eighty
lines, with tests for the cases that actually bite: values that would round-trip
as another type, multi-line strings, empty collections, and the exact
indentation `kubectl` uses. It also drops `managedFields`, `resourceVersion`,
`generation` and the `last-applied-configuration` annotation, which are noise a
reader has to scroll past.

If a case turns up that this cannot render, the fallback is a maintained crate
plus a post-processing pass — but that is a worse trade than it looks.
