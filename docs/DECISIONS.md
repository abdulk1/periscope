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

**Update, 2026-08-18:** golden files in `crates/cluster/tests/golden/` now pin
the output for a Pod, a Deployment, two ConfigMaps, a Secret both masked and
revealed, an Ingress full of hostile annotations and a custom resource; every
rendering is also read back with a real parser and compared with the object it
came from. Writing them found three ways this was wrong, each fixed rather than
recorded: a block scalar's body was written at a fixed two-column margin, so
every multi-line value below the top level — every ConfigMap holding a config
file — produced YAML that did not parse at all; a value ending in a newline
lost it; and dropping the noisy metadata used `Map::remove`, which under
`preserve_order` fills the hole with the last key and so shuffled the
annotations of every object kubectl had ever touched. The writer is nearer 170
lines for it. The decision stands, but its reasoning was too comfortable: what
makes a hand-written writer tractable is not that the awkward cases are few, it
is that they are enumerable — and they were not enumerated until now.

---

## ADR-0021 — Log lines are merged and batched before they cross the bridge

**Date:** 2026-08-18
**Status:** Accepted

A single busy container produces more events per second than every watch in the
app put together. One `ClusterEvent` per line would fill the bounded event
channel — which is sized for watch events — and start dropping text, which is
the one thing a log view may never do.

So the cluster layer does two things before the bridge sees anything:

1. **Merges.** Every reader in a session writes into one channel, so a
   fifty-pod tail arrives already interleaved. The UI never sorts, and the
   ordering is the one `kubectl logs -f` would have produced.
2. **Batches.** Lines accumulate for 50ms or 512 lines, whichever comes first,
   and cross as one `LogBatch`. Measured against a deliberately unthrottled
   fixture: 40,000–53,000 lines/second ingested, in roughly 100 events/second
   rather than 50,000.

`LogBatch` is the one event with **no coalescing key**. Everything else in the
protocol collapses when superseded; a dropped batch is text nobody can get back,
so batches are never collapsed into one another. Backpressure is applied to the
reader instead — a slow UI slows one container's stream rather than losing it.

---

## ADR-0022 — The log buffer is a ring, and filtering never touches the stream

**Date:** 2026-08-18
**Status:** Accepted

`LogBuffer` holds at most 100,000 lines (configurable) and evicts the oldest
when it is full, counting what it dropped and showing that count. A tail left
running all afternoon therefore has a memory ceiling: measured under a 53k
lines/second firehose, resident memory settled at ~104MB and stayed flat for as
long as it was watched.

Filtering is applied over what is already held, never by re-requesting:

- The visible set is a list of sequence numbers, so eviction and filtering stay
  independent — a line that falls out of the ring falls out of the view with it.
- New lines are matched as they arrive; a full rescan happens only when the
  pattern itself changes. A 500,000-line rescan measures well inside the 100ms
  budget in release builds, for substring and regular expressions alike.
- Case-insensitive substring search has a non-allocating ASCII path, because
  lowercasing half a million lines per keystroke is exactly the kind of thing
  that makes a filter box feel broken.
- An invalid regular expression shows its error and matches nothing, rather
  than silently emptying the view.

---

## ADR-0023 — `--tail` exists so the log view can be measured

**Date:** 2026-08-18
**Status:** Accepted

`scope --tail app=web -n prod` opens the log view on those pods as soon as the
cluster connects. It is genuinely useful — it is what `stern app=web` is for —
but the reason it exists now is narrower: the environment Periscope is being
built in denies the shell accessibility permission, so nothing can click the
Logs button in a running window. Without a way to start a tail from the command
line, "tails 50 pods while staying above 60fps" could not have been measured at
all, only asserted.

It requires `--namespace`, because logs are a pod subresource: there is no
cluster-wide log endpoint to fall back on when no namespace is given. The same
constraint shapes the in-app path — tailing by label selector needs the
namespace filter set first, and says so rather than failing at the API.

---

## ADR-0024 — Panes are selections; the tables underneath are shared

**Date:** 2026-08-18
**Status:** Accepted

Viewing two clusters side by side could have been two independent stores. It is
not: `AppState` keeps one map of tables keyed by `(cluster, kind)`, and a `Pane`
holds only a selection — a cluster, a kind, its filters, and the materialised
rows it renders.

That has three consequences worth stating:

- Two panes on the same cluster and kind cost one table, not two, and one watch,
  not two. Pointing both panes at the same place is a legitimate thing to do
  (different filters on the same data) and it is nearly free.
- An event updates every pane that shows it, in one pass. `apply_batch` collects
  the affected pane indexes and rebuilds each at most once per flush, so a
  fifty-event batch still costs two rebuilds at most.
- Closing a pane throws away a selection, not data. The cluster stays warm until
  the idle sweep decides otherwise (ADR-0025).

---

## ADR-0025 — Clusters stay warm until nobody has looked at them for a while

**Date:** 2026-08-18
**Status:** Accepted
**Supersedes:** the one-watch-at-a-time rule in ADR-0019.

Phase 2 stopped the previous watch whenever the kind changed, which kept
bandwidth honest and made switching back a re-listing. Phase 4's acceptance
criterion is the opposite: *switching between clusters is instant, with no
re-fetch of warm clusters*. So:

- **Lazy connect.** A cluster is connected when a pane first points at it, not
  at startup. Ten contexts in kubeconfig cost ten rows in a list.
- **Warm.** Watches keep running when a pane moves elsewhere. Going back is a
  selection change and nothing else — measured at well under a millisecond
  against five warm clusters.
- **Released.** A sweep every 30 seconds stops the watches of any cluster that
  has been out of every pane for five minutes, and drops its rows with them.
  The *connection* is kept: coming back should not mean re-running an exec
  plugin and waiting on a cloud IAM round trip.

Both intervals are constants today rather than settings; Phase 6 owns the
config file, and that is where they belong.

---

## ADR-0026 — The row budget releases what nobody is looking at

**Date:** 2026-08-18
**Status:** Accepted

"Per-cluster resource budget so one huge cluster cannot starve the others" could
mean truncating a listing. It does not here: a table that silently held half a
cluster's pods would be a lie the UI could not correct.

Instead the budget (200,000 rows per cluster) is enforced by **releasing whole
tables that no pane is showing**, largest first, until the cluster is back under
it. What a pane is showing is never released — dropping what is on screen to
save memory would be absurd — so one enormous table can still exceed the budget
by itself. What the budget prevents is a cluster accumulating every kind the
user has ever visited while another cluster needs the memory.

Measured: five clusters holding 50,110 rows between them settle at 29MB
resident, flat over a 45-second soak.

---

## ADR-0027 — Split panes use resizable panels, not the docking framework

**Date:** 2026-08-18
**Status:** Accepted
**Amends:** the "split panes / docking (`gpui-component` docking)" line in
`IMPLEMENTATION.md` §3 Phase 4.

`gpui-component` ships a dock system — tab strips, drag-to-dock, panel
registration — and a much smaller `h_resizable`. Periscope uses the latter: two
panes with a draggable divider.

The dock system would buy tear-off tabs and arbitrary layouts, and cost a panel
registry, serialisable layout state, and a tab abstraction over views that
currently have no identity of their own. Two panes is what the acceptance
criterion asks for ("two clusters can be viewed side by side"), and the smaller
mechanism does it. If Phase 6 wants saved layouts or more than two panes, the
dock system is the thing to reach for then — this is a deliberate deferral, not
an oversight.

---

## ADR-0028 — Mutations pass two independent gates

**Date:** 2026-08-18
**Status:** Accepted
**Ends:** the read-only invariant declared in `IMPLEMENTATION.md` §0.6, which
held from Phase 0 to Phase 4.

Until now the app could not change a cluster because it contained no code that
could. That property is gone, so it is replaced by two checks that do not share
an implementation:

1. **The store** (`crates/store/src/permissions.rs`). `AppState::authorize`
   returns an `Authorized` value, and `Authorized` has no public constructor —
   so a caller cannot send a mutation it never asked permission for. This is the
   gate the acceptance criterion names, and the test that proves it lives beside
   it.
2. **The cluster layer** (`crates/cluster/src/mutate.rs`). `WritePolicy` is
   checked again immediately before the request goes out. It is built from the
   same settings, but it is a separate type with its own tests, because the
   point is that a bug in the first gate is not enough.

Both refuse the same way, both record the refusal, and the second one logs a
warning when it fires — a refusal there means something above it skipped a
check, which is worth knowing about.

Read-only is opt-in. Periscope's default is to have no opinion: it is exactly as
dangerous as the credentials it was handed, and pretending otherwise would train
people to ignore the setting that matters.

---

## ADR-0029 — Every attempt is written down, including the refused ones

**Date:** 2026-08-18
**Status:** Accepted

`audit.log` gets one JSON object per line for every mutation Periscope
attempts — applied, dry-run, refused or failed — written *before* the outcome
reaches the UI. It answers "what did I do to that cluster at four o'clock"
without depending on anyone's memory.

Deliberate choices:

- **Refusals are recorded.** A read-only cluster refusing a delete is exactly
  the kind of thing worth being able to prove later.
- **Context names, not server URLs.** The audit log is the one file that could
  leak what the rest of the app is careful never to write down, so it records
  the name the user chose and nothing about how to reach the cluster. A test
  asserts a line contains no URL and no token.
- **A failed write is logged, never fatal.** Losing the ability to record an
  action is not a reason to refuse to perform one — but it is a reason to say so
  loudly in the app log.
- **Read back with `filter_map`.** A line truncated by a crash is skipped rather
  than making the whole history unreadable.

---

## ADR-0030 — The confirmation is generated from the request

**Date:** 2026-08-18
**Status:** Accepted

"Every mutation requires explicit confirmation showing cluster name, object,
namespace, and the exact operation" could have been a dialog assembled in the
view. It is not: `Mutation::confirmation(cluster)` builds the sentence from the
same value that will be sent, so the two cannot drift. A test asserts that every
variant's sentence names the cluster.

The sentence leads with the operation and ends with the cluster — *"Delete
deployments.apps api in namespace payments on cluster prod?"* — because the
mistake this exists to prevent is doing the right thing to the wrong cluster,
and the cluster is the last thing read before the pointer moves to the button.

Nothing is pre-selected in the dialog, destructive confirms are the only red
control on screen, and `Escape` cancels the mutation before it closes anything
else that happens to be open.

## ADR-0031 — A forward is a listener plus a stream per connection

**Date:** 2026-08-18
**Status:** Accepted

Port forwarding could have opened one stream to the apiserver and multiplexed
every local connection through it. It does not: the listener stays bound for the
life of the forward, and each accepted connection opens its own port-forward
stream, copies bytes both ways, and closes.

That is what makes a forward survive a hiccup. A stream that breaks takes down
one connection; the next one gets a fresh stream, and the address the user
copied keeps working. It also means a forward has three honest states rather
than two — `Listening`, `Degraded` (bound, but the last connection failed, with
the reason kept verbatim), and `Failed` (not bound at all) — so a forward that
has stopped working never looks identical to one that is merely idle.

Forwards bind `127.0.0.1` only. Binding `0.0.0.0` would put a cluster-internal
service on whatever network the laptop is attached to, which is not something a
debugging tool should do without being asked very explicitly.

The apiserver reports per-port problems — "port not open", "pod not running" —
on a side channel rather than by failing the request, so the forward takes that
error and prefers it over the copy error it caused. Its own words are useless on
their own (`404 Not Found` names nothing), so the target is prefixed:
`default/api-0:8080: 404 Not Found`.

## ADR-0032 — Drain is cordon plus eviction, and it says what it skipped

**Date:** 2026-08-18
**Status:** Accepted

`Mutation::Drain` cordons the node, lists the pods on it with a
`spec.nodeName` field selector, and evicts them through the eviction API — which
respects PodDisruptionBudgets, where a delete would not.

It skips what `kubectl drain` skips by default: DaemonSet pods, which would come
straight back, and mirror pods, which cannot be evicted at all. A pod the
apiserver refuses to evict does not fail the drain; the refusal is collected and
reported alongside everything that worked, because "drained, except these three"
is the truth and "failed" is not.

There is no `--force` and no deletion fallback. A drain that cannot evict
something says so and leaves it running, which is recoverable; deleting a pod
with no controller behind it is not.

## ADR-0033 — Exec runs a command; it is not a terminal

**Date:** 2026-08-18
**Status:** Accepted — and a deliberate deviation from the spec

`IMPLEMENTATION.md` §Phase 5 asks for "exec into a container (terminal emulation
inside the app)". This does not do that. It implements *run a command and stream
its output*: no pseudo-terminal, no VT parser, no cursor addressing, no stdin,
no resize protocol.

That is a deviation, not a reading of an ambiguity, and it is recorded here as
one. The reasoning: terminal emulation is a component on the scale of the log
view, not a detail of this phase — a VT/ANSI parser, a character grid renderer
with its own virtualisation, a `terminal_size` channel wired to layout, and
stdin plumbed through the websocket. Half of it is worse than none: a box that
accepts `top` and then renders escape sequences is a bug report, not a feature.

What is built covers what people actually reach for during an incident: `ls`,
`cat /etc/config`, `env`, `ps`, `nslookup`. It does not cover interactive `sh`,
`vi` or `top`. `docs/LIMITATIONS.md` says so in those words, and the protocol is
already shaped so a terminal can be added later without changing it: the target
carries a container name, and the transport is the same exec subresource a PTY
would use.

Consequences of drawing the line there:

* Output is emitted as `LogLine`s, so it lands in the same bounded, filterable
  ring buffer the log view already has. A command that prints a gigabyte cannot
  take the app down.
* stdout and stderr are read concurrently and tagged, because "was that stdout
  or stderr" is usually the next question.
* The command line is split on whitespace and is **not** a shell. A pipe is
  passed to the program as an argument. Anyone who wants a shell asks for one:
  `sh -c "ls | wc -l"` runs a shell *in the container*, which is honest about
  what is happening.
* A non-zero exit is a result, not a Periscope failure, and it is shown as
  `exited 2`. A command that never started — no such executable, no such
  container — is a `Failed`, not an exit with an unknown code. The distinction
  is load-bearing: the first version reported both as `Exited { code: None }`,
  which made "executable file not found" render as if the command had finished
  fine. The e2e test against a real cluster is what caught it.

Exec goes through the same two gates as a mutation (ADR-0028) and is written to
the same audit log (ADR-0029), with the command line as the detail. `kubectl
exec` needs `create` on `pods/exec`, and `rm -rf /data` is a perfectly ordinary
command: a cluster marked read-only refuses to run one, in the store and again
in the cluster layer. The confirmation names the cluster, the namespace, the pod
and the exact command, and every command is treated as destructive, because
Periscope cannot tell `ls` from `rm -rf` before it runs it.

The audit line is written *before* the command runs, not after: a command that
hangs, or one Periscope is killed in the middle of, still has to leave a trace.

## ADR-0034 — A degraded watch asks the apiserver whether it is back

**Date:** 2026-08-18
**Status:** Accepted

Recovery from a broken watch used to be inferred from the stream: after a
failure, the next `InitDone` — the end of a fresh listing — cleared the degraded
state and reported `Connected`.

That inference is wrong, and the fault-injection test is what proved it.
`kube`'s watcher resumes an interrupted watch from the last resource version it
saw rather than re-listing, so a brief outage produces **no** `InitDone`. On a
namespace where nothing happens to be changing it produces no events at all.
The watch was healthy again within a second or two; the UI went on showing
*"Degraded — pods: tls handshake eof"* indefinitely, with rows that were in fact
live. A false alarm that never clears is worse than no indicator, because it
teaches people to ignore the one that matters.

Two changes:

* Any successful event now clears the degraded state, not only `InitDone`.
  Recovery is reported before the event is translated, because most watch events
  produce nothing for the UI and waiting for one that does is the same bug in
  smaller form.
* While degraded — and only while degraded — the stream is raced against a
  liveness probe every three seconds: one `list` with `limit=1` against the same
  kind and namespace. When it succeeds, the cluster is reported healthy.

The probe costs nothing on a healthy cluster because it does not run, it asks
about the same objects the watch covers rather than something incidental, and it
does not do the reconnecting — `kube`'s backoff still owns that. It only answers
the question the status bar is claiming to answer.

The residual dishonesty is small and worth naming: the probe can succeed while
the watch is still failing for a reason specific to watching, in which case the
next watch error flips the state straight back to degraded. Reporting healthy a
few seconds early is recoverable; reporting broken forever is not.

## ADR-0035 — Typable keys are bound outside text fields only

**Date:** 2026-08-18
**Status:** Accepted

Phase 6 asks for "keybindings modeled on k9s/vim defaults so the target user is
immediately productive; fully remappable". k9s's defaults are single letters —
`:` for the jump prompt, `l` for logs, `q` to go back — and single letters are
also characters people type.

GPUI dispatches an action from the focused element upwards, so a binding on the
root fires even while a text input has focus. Bound naively, `l` tails logs
instead of typing an `l` into the namespace filter. That is not a hypothetical:
the first version did exactly that, and the test that types into a filter field
is what caught it.

So the context depends on the *keystroke*, not on the command. A keystroke with
no `ctrl`/`cmd`/`alt`/`fn` modifier whose key is a single character is bound with
the predicate `!Input && !NumberInput && !SearchPanel`; everything else — `cmd-l`,
`escape`, `enter` — binds unrestricted, because a chord is unambiguous. Shift
still counts as typable: `:` is shift-semicolon and is very much a character.

Two consequences worth knowing:

* The palette's own navigation is never a letter. Inside the palette a text field
  always has focus, so `j`/`k` would be swallowed or, worse, both typed and acted
  on. It uses the arrows and the readline pair `ctrl-n`/`ctrl-p`, and a test
  asserts no palette binding is a single character.
* Closing the palette now returns focus to the root. It used to leave focus on
  the palette's input, which is no longer on screen — harmless when every binding
  was a chord, and fatal once any binding is scoped to "not in a text field":
  every typable key stopped working for the rest of the session. Found by the
  same batch of tests, fixed with the focus call that should always have been
  there.

Remapping replaces a command's defaults rather than adding to them, an empty list
unbinds, and a command that is not mentioned keeps its defaults. A misspelled
command name is an error naming what was expected, which is a deliberate
exception to this file's usual "ignore what you do not understand" rule: an
ignored keymap line gives you a key that does nothing, and no way to tell that
apart from a bug in the app. A keystroke that is not a key is skipped and
reported on screen — `KeyBinding::new` panics on a malformed one, and a typo in
a config file must not take the app down before it has a window to complain in.

## ADR-0036 — A custom resource prints the columns its CRD declared

**Date:** 2026-08-18
**Status:** Accepted

Every kind but the fifteen this project writes projectors for used to render the
same two columns — `STATUS` from `status.phase`, `READY` from a `Ready`
condition — which for most CRDs means two empty cells. `kubectl get certificates`
prints READY, SECRET and AGE, and it does so without knowing anything about
cert-manager: a CustomResourceDefinition declares `additionalPrinterColumns`,
each with a heading, an OpenAPI type, a JSONPath and a priority, and the
apiserver renders them. Periscope now reads the same declarations, so a custom
resource looks the same in both.

### Where the columns come from

Discovery already asks the apiserver what it serves. It now also lists
CustomResourceDefinitions and, for each custom kind, reads the printer columns
declared for the exact version being watched. The list is paged: a CRD carries
its whole OpenAPI schema, four fields of which are wanted, and a cluster with a
service mesh and two operators has hundreds of them.

Reading CRDs cannot fail the listing. `customresourcedefinitions` is a
cluster-scoped resource that plenty of RBAC setups withhold, and a cluster that
serves custom resources it will not describe is ordinary; the failure is logged
with its reason intact and every kind lists on the generic columns, exactly as
before. The same is true one column at a time: a JSONPath this cannot parse
costs that column and nothing else.

### Why `jsonpath-rust` rather than a path walker

The forms CRDs really write are `.spec.secretName`, `.status.conditions[0].type`
and — cert-manager uses it on five kinds —
`.status.conditions[?(@.type=="Ready")].status`. The first two are an afternoon;
the filter is a parser, and a hand-rolled parser for an expression language is
the kind of thing that is subtly wrong for a year. `jsonpath-rust` is already in
the dependency tree because `kube-client` uses it, so depending on it directly
costs nothing to build, and it accepts every form above once the root is named:
`additionalPrinterColumns` declares paths relative to the object (`.spec.foo`)
and RFC 9535 wants `$.spec.foo`, which is the whole translation.

Expressions are parsed once, when discovery reads the CRD, and the parsed query
is what each row is evaluated against. A ten-thousand-row listing evaluates
every column on every object; re-parsing the expression per cell would put a
`pest` parse in that loop.

The one dialect difference left is `kubectl`'s backslash escaping for key names
containing dots — `.metadata.labels.app\.kubernetes\.io/name`. RFC 9535 spells
that with brackets and `jsonpath-rust` rejects the backslashes, so such a column
is dropped with a warning naming it. No CRD in the test cluster writes one.

### Matching what `kubectl` prints

The point is a table the target user can read without translating, so the
formatting rules are `kubectl`'s, not new ones:

* a path that finds nothing, and a value whose JSON type is not the declared
  one, both render `<none>` — a blank cell and an empty string mean different
  things;
* a `date` renders as an age (`5m`, `2d`) through the same formatter as the AGE
  column, which moved into `periscope-bridge` so the cluster layer and the view
  cannot drift apart on it. A timestamp in the future is `<invalid>`;
* headings are upper-cased, so a CRD's `Sync Status` reads `SYNC STATUS`
  alongside `READY` and `RESTARTS`.

A column with a priority above zero is hidden, as `kubectl` hides one until
asked for `-o wide`. There is no wide listing yet; `printer::is_visible` is the
single test a `-o wide` toggle would relax, and rebuilding the table is what it
would re-run.

Two departures, both deliberate. A CRD's own `Age` column — cert-manager
declares one on every kind — is dropped, because the table already appends AGE
to every kind from the same field and two identical columns look like a bug. And
a row's colour still comes from the conventional `status.phase` and `Ready`
heuristic: printer columns say what to *print*, never how healthy an object is.

### What did not change

Columns still travel with the rows as data. The UI learned nothing: it renders a
CRD's declared columns through the same path it renders a Pod's, and the
`[columns]` setting narrows them by name like any other kind's.
