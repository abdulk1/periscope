# Periscope

A native, GPU-accelerated Kubernetes console. k9s-class capability with real UI
affordances: live resource streams, multi-cluster, and log tailing across pods.

**Binary:** `scope` · **Language:** Rust · **UI:** GPUI

> **Status: Phase 5 (actions), partially.** Connects to kubeconfig contexts on
> demand, discovers every kind each serves — CRDs included — streams any of them
> into a virtualised table, tails logs from one pod or from every pod matching a
> label selector, and shows two clusters side by side. Clusters you have visited
> stay warm, so switching back is instant. Fuzzy jump palette (⌘K) searches
> every warm cluster at once. Read-only until Phase 5. See
> [`IMPLEMENTATION.md`](IMPLEMENTATION.md) for the roadmap and
> [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md) for what does not work.

## Build and run

Requires Rust stable (developed on 1.97.1; MSRV 1.89) and a kubeconfig.

```sh
cargo run --release --bin scope                        # open the window
cargo run --release --bin scope -- --kubeconfig ./kc   # use one specific file
cargo run --release --bin scope -- --verbose           # mirror the log to stderr
cargo run --release --bin scope -- --perf              # log watch throughput and flush timings
cargo run --release --bin scope -- --tail app=web -n prod  # open straight into the log view
```

It connects to the `current-context` on start; the sidebar lists every context
and every kind the cluster serves. Prefer `--release`: debug builds miss the
cold-start budget by a wide margin (`docs/LIMITATIONS.md`).

| Key | Does |
|---|---|
| `⌘K` / `ctrl-K` | Jump to a cluster, a kind, or an object by name |
| `↑` `↓` `enter` | Move through the jump results and open one |
| `escape` | Close the palette, then the detail pane |
| `enter` in the namespace or selector field | Re-list with that filter |
| `⌘L` / `ctrl-L` | Tail the open pod, or every pod matching the current namespace + selector |
| `⌘⇧F` | Follow the newest line, or pause where you are |
| `⌘\` | Show two clusters side by side, or go back to one |

Logs are written to a daily-rotating file under the platform's application data
directory; the path is printed in the log's first line and shown by `--verbose`.

## Layout

```
crates/
├── scope/     binary: flags, window setup, wiring
├── ui/        GPUI views and components          (main thread only)
├── store/     state, indexes, filtering          (no GPUI, no kube)
├── cluster/   kube clients, watchers, logs       (tokio only)
├── bridge/    tokio <-> GPUI plumbing
└── config/    paths, settings, themes, logging   (no GPUI, no kube)
```

The dependency edges are the architecture: `store`, `cluster` and `config` cannot
reach GPUI, and `ui` cannot reach Kubernetes. Everything crossing between them
goes through `bridge` as a bounded, coalesced message stream.

## Development

Every change must leave these green:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
```

The end-to-end suite needs a real cluster and is skipped unless asked for:

```sh
kind create cluster --name periscope
cargo test -p periscope-e2e -- --ignored --test-threads 1
cargo run --release -p periscope-e2e --bin seed-pods -- --count 10000  # load fixture
```

Architecture decisions are recorded in [`docs/DECISIONS.md`](docs/DECISIONS.md) —
append, never rewrite.

## Several clusters

Clusters connect when you first look at one, not at startup, and keep streaming
after you move away — switching back shows what is already held rather than
re-listing. `⌘\` splits the window so two clusters sit side by side; clicking a
pane points the sidebar, the filters and the palette at it. A cluster nobody has
looked at for five minutes is let go: its watches stop and its rows are freed,
while its connection is kept so returning does not mean authenticating again.

The palette searches every warm cluster, not just the one on screen, and says
which cluster a hit is on when it is not the one you are looking at.

## Logs

Open a pod and press **Logs**, or set a namespace and a label selector in the
table's filters and press `⌘L` to merge every matching pod into one stream. Each
pod keeps its own colour, new pods are attached as they appear, and a pod that
is replaced is re-attached without asking.

The buffer holds 100,000 lines and drops the oldest beyond that, saying how many
it dropped. Filtering — substring or regular expression, case-sensitive or not —
applies to what is already held, so it never restarts the stream. **Copy** puts
the visible lines on the clipboard; **Export** writes them to a file and tells
you where.

## Changing things

Open an object and the detail pane offers what its kind supports: **Scale**,
**Restart**, **Cordon**, **Dry run**, **Apply** and **Delete**. Nothing happens
until you confirm a sentence that names the cluster, the namespace, the object
and the operation — *"Delete deployments.apps api in namespace payments on
cluster prod?"* — and `Escape` cancels it.

Mark the clusters that must never change in `settings.toml`:

```toml
[access]
read-only = ["prod", "prod-eu"]

# Or invert it: nothing is writable unless named.
read-only-by-default = true
writable = ["kind-local"]
```

Those names are refused twice: once by the store, before anything is sent, and
again by the cluster layer, immediately before the request. Every attempt —
applied, dry-run, refused or failed — is appended to `audit.log` beside the
application logs.

## Security posture

No credentials are ever written to disk. No telemetry, no phone-home, no crash
reporting. The only network calls are to the clusters you configure.

Secrets are masked: the table shows how many keys a Secret has and never a
value, and its YAML shows `<hidden, N bytes>` until you press **Reveal values**,
which re-fetches it. Closing the pane masks it again.
