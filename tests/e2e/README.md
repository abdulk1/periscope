# End-to-end tests

`kind`-based integration tests, plus the fixture generator that seeds a cluster
with 10,000 pods for the Phase 1 load budget.

Every test here needs a real apiserver, so all of them are `#[ignore]`d and never
run as part of `cargo test`.

```sh
kind create cluster --name periscope
cargo test -p periscope-e2e -- --ignored --test-threads 1
```

The context defaults to `kind-periscope`; `PERISCOPE_E2E_CONTEXT` points the
suite anywhere else. `--test-threads 1` keeps several watch streams from racing
for the same one-node cluster.

## What is covered

| File | Covers |
|---|---|
| `tests/kind.rs` | Listing pods, live create/delete latency, disconnect, and the 10k-pod list budget |
| `tests/auth.rs` | A credential the apiserver rejects, a missing kubeconfig, and a context that does not exist |

The auth tests write a throwaway kubeconfig into the temp directory and point the
app at it with `--kubeconfig`'s programmatic equivalent, so the developer's own
kubeconfig is never touched.

## Load fixture

```sh
cargo run --release -p periscope-e2e --bin seed-pods -- --count 10000
cargo run --release -p periscope-e2e --bin seed-pods -- --delete
```

`seed-pods` is the only thing in this repository that writes to a cluster. It
creates a `periscope-load` namespace full of pods carrying a node selector that
matches nothing: the apiserver stores them and streams them to every watcher, but
they are never scheduled, so 10,000 of them fit on a one-node `kind` cluster and
nothing is pulled or run. `--delete` removes the namespace and everything in it.

It prints the cluster URL it is about to write to before it writes anything.
