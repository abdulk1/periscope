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
| `tests/exec_auth.rs` | Exec credential plugins shaped like EKS's and GKE's, including the real `aws` CLI |
| `tests/discovery.rs` | Discovery with CRDs, generic tables, filters, secret masking, and the detail fetch |
| `tests/logs.rs` | Tailing one pod and fifty, merging, re-attach after a restart, and the ingest-rate budget |
| `tests/multicluster.rs` | Five clusters at once, the row budget, warm switching, and one unreachable cluster |
| `tests/mutations.rs` | Delete, scale, restart, cordon, drain, apply and dry run — plus a read-only cluster refusing, and the audit log |
| `tests/forwards.rs` | Real HTTP traffic through a forward, several connections, teardown closing the port, a dead port reported, and recovery after a broken connection |
| `tests/exec.rs` | Running a command, its exit code, stdout and stderr kept apart, a command that does not exist, cancellation, and the audit log |

`tests/mutations.rs` **changes the cluster**: each test creates the deployment it
acts on and deletes it afterwards, the cordon test always uncordons, and the
drain test drains the one `kind` node and uncordons it again — the node's pods
are evicted and rescheduled, which takes a minute or so to settle.

`tests/exec.rs` runs commands inside the `webby` fixture's container. They are
all reads (`cat`, `ls`, `seq`, `id`); the one command that would write anything
is in the read-only test, which asserts it never ran.

The auth tests write a throwaway kubeconfig into the temp directory and point the
app at it with `--kubeconfig`'s programmatic equivalent, so the developer's own
kubeconfig is never touched.

## Workload fixtures

Some suites need workloads that produce something to read, or something to talk
to:

```sh
kubectl apply -f tests/e2e/fixtures/chatty.yaml     # three pods, five lines a second each
kubectl apply -f tests/e2e/fixtures/firehose.yaml   # four pods writing as fast as they can
kubectl delete -f tests/e2e/fixtures/firehose.yaml  # it burns CPU; delete it when done
kubectl apply -f tests/e2e/fixtures/webby.yaml      # one busybox pod serving a fixed string on 8080
```

`webby` is what the forward and exec suites use: it answers on a port, and its
busybox container has the handful of commands the exec tests run. Both suites
skip themselves, with instructions, when it is missing.

`chatty` scales up for the "tail fifty pods" measurement:
`kubectl scale deployment chatty --replicas=50`.

The CRD-heavy discovery test needs cert-manager and Argo CD installed; the test
says so when they are missing.

The multi-cluster suite writes its own kubeconfig with five contexts pointing at
the test cluster and one pointing at a closed port; nothing extra is needed.
`PERISCOPE_E2E_SOAK=45` keeps those five sessions streaming for 45 seconds so
resident memory can be sampled from outside the process.

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
