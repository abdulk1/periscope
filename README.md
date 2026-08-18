# Periscope

A native, GPU-accelerated Kubernetes console. k9s-class capability with real UI
affordances: live resource streams, multi-cluster, and log tailing across pods.

**Binary:** `scope` · **Language:** Rust · **UI:** GPUI

> **Status: Phase 0 (skeleton).** The window opens and the tokio ↔ GPUI bridge
> works end to end. It does not connect to Kubernetes yet — that is Phase 1. See
> [`IMPLEMENTATION.md`](IMPLEMENTATION.md) for the roadmap and
> [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md) for what does not work.

## Build and run

Requires Rust stable (developed on 1.97.1; MSRV 1.85).

```sh
cargo run --bin scope             # open the window
cargo run --bin scope -- --verbose  # mirror the log to stderr
cargo run --bin scope -- --perf     # log bridge throughput per flush
```

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

Architecture decisions are recorded in [`docs/DECISIONS.md`](docs/DECISIONS.md) —
append, never rewrite.

## Security posture

No credentials are ever written to disk. No telemetry, no phone-home, no crash
reporting. The only network calls are to the clusters you configure.
