# Periscope — Implementation Plan & Roadmap

A native, GPU-accelerated Kubernetes console. k9s-class capability with real UI
affordances: live resource streams, multi-cluster, and log tailing across pods.

**Product name:** Periscope · **Binary:** `scope` · **Language:** Rust · **UI:** GPUI

---

## 0. How to use this document (read first, agent)

This is a build spec, not a suggestion. Rules that apply to every phase:

1. **Work phase by phase.** Do not start Phase N+1 until every acceptance
   criterion in Phase N passes. Each phase ends in a working, runnable binary.
2. **Verify versions before writing any `Cargo.toml`.** Do not trust version
   numbers from memory or from this document. Check crates.io / docs.rs for the
   current release of every dependency at the time you start.
3. **Pin GPUI to an exact git commit SHA.** GPUI is pre-1.0, is not published to
   crates.io, and has frequent breaking changes. Never use a floating branch.
   Upgrading the pin is its own deliberate task with its own commit.
4. **Never modify vendored or upstream GPUI code.** If GPUI cannot do something,
   work around it in our code and record the limitation in `docs/DECISIONS.md`.
5. **Every phase ends green:** `cargo fmt --check`, `cargo clippy -- -D warnings`,
   `cargo test`, and a manual smoke test against a real cluster.
6. **Read-only until Phase 5.** No code path may mutate cluster state before
   then. This is a safety invariant, not a scheduling preference.
7. **Ask before inventing scope.** If a requirement here is ambiguous, implement
   the smallest reasonable version and note the ambiguity in the PR description.
   Do not add unrequested features.

---

## 1. Product definition

### What it is
A desktop application for engineers who operate multiple Kubernetes clusters and
currently live in `kubectl` and `k9s`. It renders live cluster state at 120fps,
handles clusters with tens of thousands of objects without stalling, and can tail
logs from many pods at once in a single readable stream.

### Why GPUI
The differentiating constraint is throughput: virtualized tables over
high-churn watch streams, and log views over millions of lines. Electron-based
consoles stall at exactly this point. GPUI renders through Metal/Vulkan/DX12 with
a target of 120fps and a small memory footprint, which is the only reason to
accept a pre-1.0 framework.

### Non-goals (v1)
- Cluster provisioning, Helm release management, or CI/CD integration
- Cost analysis, security scanning, or policy enforcement
- Web or mobile clients
- A plugin system or extension API
- Windows support as a launch blocker (build it, do not gate on it)
- Accessibility parity — GPUI's a11y support is immature; document the gap,
  do not attempt to solve it in v1

### Target user
An SRE or platform engineer with 3–20 clusters, comfortable in a terminal, who
wants faster navigation and correlation than a TUI allows.

---

## 2. Architecture

### 2.1 Process layout

Single process, three logical layers, strictly separated:

```
┌────────────────────────────────────────────────────────┐
│  UI layer (GPUI)              main thread only         │
│  Views, entities, rendering, input dispatch            │
└───────────────▲────────────────────────────────────────┘
                │ channel (updates in) / commands (out)
┌───────────────┴────────────────────────────────────────┐
│  Store layer                  owns app state           │
│  Per-cluster resource caches, indexes, filters         │
└───────────────▲────────────────────────────────────────┘
                │ watch events / log lines
┌───────────────┴────────────────────────────────────────┐
│  Cluster layer (tokio)        background threads       │
│  kube-rs clients, watchers, reflectors, log streams    │
└────────────────────────────────────────────────────────┘
```

### 2.2 The GPUI ↔ tokio bridge (do this first, get it right)

GPUI has its own executors (`ForegroundExecutor` for main-thread UI mutation,
`BackgroundExecutor` for work). `kube-rs` requires a tokio runtime. Do not try to
run kube futures on GPUI's executor.

Required design:
- Spawn a **multi-threaded tokio runtime on a dedicated background thread**,
  owned by a `ClusterRuntime` struct that lives for the process lifetime.
- Communication into the UI: `flume` or `tokio::sync::mpsc` channels carrying
  `ClusterEvent` messages. A GPUI background task drains the channel and applies
  updates to entities via the foreground executor.
- Communication out of the UI: a command channel carrying `ClusterCommand`
  (start watch, stop watch, fetch logs, cancel).
- **Coalesce updates.** Never apply one watch event per frame. Batch events and
  flush to the UI at a fixed cadence (start at 16ms) so a resync storm of 10,000
  objects does not produce 10,000 UI mutations.
- All channels are bounded. On overflow, drop-and-mark-stale rather than block
  the cluster layer.

Write this bridge as its own crate (`crates/bridge`) with tests that do not
require a cluster.

### 2.3 Workspace layout

```
periscope/
├── Cargo.toml                  # workspace
├── crates/
│   ├── scope/                  # binary: main, window setup, wiring
│   ├── ui/                     # GPUI views and components
│   ├── store/                  # state, indexes, filtering, selection
│   ├── cluster/                # kube-rs clients, watchers, log streams
│   ├── bridge/                 # tokio ↔ GPUI plumbing
│   └── config/                 # kubeconfig parsing, app settings, themes
├── docs/
│   ├── DECISIONS.md            # architecture decision log — append, never rewrite
│   └── LIMITATIONS.md          # known gaps, GPUI workarounds
└── tests/
    └── e2e/                    # kind-based integration tests
```

### 2.4 Key dependency choices

Verify current versions before use; these are the crates, not the versions:

| Concern | Crate | Notes |
|---|---|---|
| UI framework | `gpui` | git dep on `zed-industries/zed`, pinned SHA |
| UI components | `gpui-component` | tables, docking, charts, code editor |
| K8s client | `kube` | enable `client`, `runtime`, `ws`, `oidc`, `socks5` |
| K8s types | `k8s-openapi` | pin the API version feature explicitly |
| Async runtime | `tokio` | multi-thread flavor |
| Channels | `flume` | mpmc, sync+async |
| Errors | `anyhow` + `thiserror` | anyhow at boundaries, thiserror in libs |
| Logging | `tracing` + `tracing-subscriber` | file-based, rotating |
| Fuzzy match | `nucleo` | same matcher Zed uses |
| Serialization | `serde`, `serde_yaml`, `serde_json` | |

### 2.5 Authentication requirements (non-negotiable)

The app must work with real enterprise clusters on day one:
- **exec credential plugins** (`aws eks get-token`, `gke-gcloud-auth-plugin`,
  `kubelogin`). Handle token expiry and silent refresh; surface auth failure as a
  clear UI state, never a silent empty table.
- **OIDC** with refresh tokens.
- Client certificates, bearer tokens, and in-cluster service accounts.
- Proxy support (`HTTPS_PROXY`, `NO_PROXY`) and custom CA bundles.
- **Never write credentials to disk.** Read kubeconfig, hold tokens in memory
  only. No telemetry, no phone-home, no crash reporting that includes cluster
  identifiers.

---

## 3. Roadmap

Each phase is a shippable increment. Estimates assume one focused engineer with
agent assistance; treat them as sequencing, not commitments.

### Phase 0 — Skeleton (foundation)

**Goal:** A window that opens, a runtime that runs, and a green CI pipeline.

- Cargo workspace with the crate layout above
- GPUI window opens on macOS and Linux with a placeholder view
- `gpui-component` wired in; theme (light/dark) switchable
- `ClusterRuntime` with tokio on a background thread
- `bridge` crate with a working round-trip: UI sends a command, background task
  responds, UI renders the response
- `tracing` to a rotating log file; `--verbose` flag
- CI: fmt, clippy (deny warnings), test, build on macOS + Linux

**Acceptance:**
- `cargo run` opens a window in under 500ms on a warm start
- A test proves a message crosses tokio → GPUI and mutates a rendered entity
- CI is green on a clean checkout

---

### Phase 1 — Read a cluster (single context, single resource)

**Goal:** Connect to one cluster and render live pods.

- Parse kubeconfig (`~/.kube/config`, `$KUBECONFIG`, multiple files merged)
- Context list; connect to the current context
- Auth: exec plugins + token + client cert paths working against a real EKS
  cluster and a local `kind` cluster
- `kube::runtime::watcher` + reflector `Store` for Pods in all namespaces
- Virtualized table (`uniform_list`) rendering: name, namespace, ready, status,
  restarts, age, node
- Live updates applied through the coalescing bridge
- Connection state machine surfaced in the UI: connecting, connected, degraded,
  auth-failed, disconnected — with the actual error text available

**Acceptance:**
- Connects to a cluster with **10,000+ pods**; initial list renders in under 3s
- Scrolling that list holds 60fps minimum, 120fps target
- Killing a pod externally updates the table within 1s
- Revoking credentials mid-session produces a clear auth-failed state, not a hang
- Memory under 300MB with 10k pods loaded

---

### Phase 2 — Navigate everything (all resources)

**Goal:** Any resource type, including CRDs, with real navigation.

- Resource discovery via the API discovery endpoint, including CRDs
- Generic resource table driven by column definitions per kind; sensible
  defaults for core kinds (Deployments, StatefulSets, Services, Nodes, Jobs,
  ConfigMaps, Secrets, Ingresses, PVCs, Events)
- Namespace filter; label selector filter
- Fuzzy command palette (`nucleo`): jump to any resource kind or object
- Detail view: full YAML with syntax highlighting (use `gpui-component`'s editor),
  describe-style summary, related events
- Owner-reference navigation: Deployment → ReplicaSet → Pods, and back
- Secrets masked by default with an explicit reveal action

**Acceptance:**
- A CRD-heavy cluster (Argo CD, cert-manager installed) lists all custom
  resources without special-casing
- Command palette returns results in under 50ms on a 10k-object cluster
- YAML view of a large ConfigMap opens without a visible frame drop

---

### Phase 3 — Logs (the differentiator)

**Goal:** Tail logs across many pods in one stream, and make it fast.

- Stream logs for a single container via `kube` log follow
- **Multi-pod tailing:** select a Deployment/label selector and merge streams
  from all matching pods, with per-pod colour coding and a source column
- Automatic re-attach when a pod restarts or is replaced
- Virtualized log view with a **bounded ring buffer** (configurable, default
  100k lines/stream); dropped-lines indicator when the cap is hit
- Live filter (substring and regex), highlight, and case toggle — applied
  without restarting the stream
- Follow/pause, jump to timestamp, copy selection, export visible buffer to file
- `--previous` container logs; init and sidecar container selection

**Acceptance:**
- Tails **50 pods simultaneously** while staying above 60fps
- Ingests 10,000 lines/second without unbounded memory growth
- Filtering a 500k-line buffer updates in under 100ms
- Pod restart during tail reconnects automatically within 2s

---

### Phase 4 — Multi-cluster

**Goal:** Many clusters at once, without losing your place.

- Connect to N contexts concurrently, each with its own client and watchers
- Cluster switcher; per-cluster connection health indicator
- Lazy connect: do not open watches for a cluster until it is viewed, then keep
  it warm with a configurable idle timeout
- Split panes / docking (`gpui-component` docking) so two clusters can be viewed
  side by side
- Cross-cluster search: find a resource name across every connected cluster
- Per-cluster resource budget so one huge cluster cannot starve the others

**Acceptance:**
- 5 clusters connected simultaneously stay under 800MB total
- Switching between clusters is instant (no re-fetch of warm clusters)
- One unreachable cluster degrades only its own pane; others are unaffected

---

### Phase 5 — Actions (first mutations)

**Goal:** The operations people actually need, with guardrails.

- Exec into a container (terminal emulation inside the app)
- Port-forward with a visible list of active forwards and one-click teardown
- Delete, scale, restart (rollout), cordon/drain, edit-and-apply
- **Every mutation requires explicit confirmation** showing cluster name, object,
  namespace, and the exact operation. No blind double-click destruction.
- Optional read-only mode per context, set in config — enforced in the store
  layer, not just hidden in the UI
- Action audit log written locally (what, where, when)
- Dry-run preview for edits (server-side dry run where supported)

**Acceptance:**
- Read-only contexts reject mutations at the store layer, proven by a test
- Every destructive action is reachable only through a confirmation that names
  the target cluster
- Port-forwards survive brief network interruption or die loudly

---

### Phase 6 — Ship it

**Goal:** Something a stranger can install.

- macOS: signed and notarized `.app`, Homebrew cask
- Linux: AppImage plus `.deb`/`.rpm`
- Windows: build and basic verification (not a launch blocker)
- Auto-update check (opt-in, no silent updates)
- Config file (TOML): themes, keybindings, default columns, idle timeouts
- Keybindings modeled on k9s/vim defaults so the target user is immediately
  productive; fully remappable
- README with screenshots, a 60-second demo GIF, and honest limitations
- `docs/LIMITATIONS.md` published as-is

**Acceptance:**
- A clean machine can install and connect to a cluster in under 2 minutes
- No crash in a 4-hour session with active watches and log tailing

---

## 4. Cross-cutting requirements

### Performance budgets (enforce, do not aspire)
| Metric | Budget |
|---|---|
| Cold start to window | < 500ms |
| First render of 10k-pod list | < 3s |
| Scroll frame rate | ≥ 60fps, target 120fps |
| Filter/search latency | < 100ms |
| Memory, 1 cluster / 10k pods | < 300MB |
| Memory, 5 clusters | < 800MB |

Add a `--perf` flag that logs frame times and watch-event throughput. Regressions
against these budgets fail the phase.

### Testing strategy
- **Unit:** store logic, filtering, indexing, coalescing — no cluster needed
- **Integration:** `kind` clusters spun up in CI; seed with a fixture generator
  that can create 10k pods for load tests
- **Golden:** YAML rendering and describe output snapshots
- **Manual smoke:** a checklist per phase, run against a real EKS cluster
- **Fault injection:** kill the API server mid-watch, expire a token mid-session,
  saturate the log stream. These are tests, not edge cases.

### Error handling philosophy
Every failure the user can see must state what failed, which cluster, and what to
try. No empty tables that silently mean "auth expired." No spinners without a
timeout. Surface the underlying API error text — this audience wants it.

### Security posture
- No credentials on disk, ever
- No network calls except to configured clusters and (opt-in) the update check
- Redact tokens and cluster hostnames from all log output
- Secrets masked by default

---

## 5. Risk register

| Risk | Mitigation |
|---|---|
| GPUI breaking changes | Pin exact SHA; upgrades are isolated, deliberate tasks |
| GPUI missing a UI primitive | Prefer `gpui-component`; document workarounds in `docs/LIMITATIONS.md` |
| Watch storms overwhelming the UI | Coalescing bridge with bounded channels, built in Phase 0 |
| Auth plugin variety (EKS/GKE/AKS/OIDC) | Test matrix in Phase 1; do not defer to later |
| Memory growth on long sessions | Bounded ring buffers; a soak test in the phase gate |
| Scope creep into a Helm/CI platform | Non-goals list in §1 is binding |
| Accessibility gap | Documented, not solved, in v1 |

---

## 6. First task for the agent

Start Phase 0. Concretely:

1. Create the workspace and crate layout in §2.3.
2. Look up the current GPUI repository state and pin an exact commit SHA;
   record the SHA and the date in `docs/DECISIONS.md`.
3. Get a GPUI window rendering "Periscope" with `gpui-component` theming.
4. Implement `ClusterRuntime` and the `bridge` round-trip described in §2.2,
   with a passing test.
5. Set up CI as described in Phase 0.
6. Report back with: the pinned SHA, the versions of every dependency you chose,
   anything in this plan that turned out to be wrong, and the cold-start time you
   measured.

Do not begin Phase 1 until all Phase 0 acceptance criteria pass.
