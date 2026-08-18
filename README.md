# Periscope

A native, GPU-accelerated Kubernetes console. k9s-class capability with real UI
affordances: live resource streams, multi-cluster, and log tailing across pods.

**Binary:** `scope` · **Language:** Rust · **UI:** GPUI

> **Status: Phase 2 (navigate everything).** Connects to a kubeconfig context,
> discovers every kind the cluster serves — CRDs included — and streams any of
> them into a virtualised table with the columns `kubectl` prints. Fuzzy jump
> palette (⌘K), namespace and label-selector filters, and a detail pane with
> YAML, events and owner-reference navigation. Read-only. Logs are Phase 3 and
> multi-cluster is Phase 4. See [`IMPLEMENTATION.md`](IMPLEMENTATION.md) for the
> roadmap and [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md) for what does not
> work.

## Build and run

Requires Rust stable (developed on 1.97.1; MSRV 1.89) and a kubeconfig.

```sh
cargo run --release --bin scope                        # open the window
cargo run --release --bin scope -- --kubeconfig ./kc   # use one specific file
cargo run --release --bin scope -- --verbose           # mirror the log to stderr
cargo run --release --bin scope -- --perf              # log watch throughput and flush timings
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

## Security posture

No credentials are ever written to disk. No telemetry, no phone-home, no crash
reporting. The only network calls are to the clusters you configure.

Secrets are masked: the table shows how many keys a Secret has and never a
value, and its YAML shows `<hidden, N bytes>` until you press **Reveal values**,
which re-fetches it. Closing the pane masks it again.
