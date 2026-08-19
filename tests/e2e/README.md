# End-to-end tests

`kind`-based integration tests, plus the fixture generator that seeds a cluster
with 10,000 pods for the Phase 1 load budget.

Every test that needs a real apiserver is `#[ignore]`d and never runs as part of
`cargo test`. The exception is `tests/harness.rs`, which tests the harness rather
than the app — what the fixtures leave on disk, and what they hand to a shell —
and needs nothing but a Unix machine.

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
| `tests/discovery.rs` | Discovery with CRDs, printer columns read from real CRDs, generic tables, filters, secret masking, and the detail fetch |
| `tests/logs.rs` | Tailing one pod and fifty, merging, re-attach after a restart, and the ingest-rate budget |
| `tests/multicluster.rs` | Five clusters at once, the row budget, warm switching, and one unreachable cluster |
| `tests/mutations.rs` | Delete, scale, restart, cordon, drain, apply and dry run — plus a read-only cluster refusing, and the audit log |
| `tests/forwards.rs` | Real HTTP traffic through a forward, several connections, teardown closing the port, a dead port reported, and recovery after a broken connection |
| `tests/exec.rs` | Running a command, its exit code, stdout and stderr kept apart, a command that does not exist, cancellation, the container it runs in, and the audit log |
| `tests/faults.rs` | The apiserver going away mid-watch: the break is reported with a reason, the rows are kept, and the watch recovers by itself when it comes back |
| `tests/harness.rs` | The harness's own promises: scratch directories and the files in them are owner-only, a `$TMPDIR` containing a quote cannot inject a command into a stub plugin, and a relative `PATH` entry is never searched for `kubectl` |

`tests/mutations.rs` **changes the cluster**: each test creates the deployment it
acts on and deletes it afterwards, the cordon test always uncordons, and the
drain test drains the one `kind` node and uncordons it again — the node's pods
are evicted and rescheduled, which takes a minute or so to settle.

`tests/exec.rs` runs commands inside the `webby` and `sidecars` fixtures'
containers. They are all reads (`cat`, `ls`, `seq`, `id`); the one command that
would write anything is in the read-only test, which asserts it never ran.

The auth tests write a throwaway kubeconfig into the temp directory and point the
app at it with `--kubeconfig`'s programmatic equivalent, so the developer's own
kubeconfig is never touched.

Those throwaway kubeconfigs, and the stub plugins the exec tests generate, are
copies of the test context's credentials — on `kind` that is the cluster's admin
certificate and key. So they are written mode 0600 into a `Scratch` directory
that this process created exclusively at mode 0700 with a random name, and the
suite resolves `kubectl` and `aws` to absolute paths rather than asking `PATH`
what to run. `tests/harness.rs` holds all of that in place.

`tests/faults.rs` does **not** stop the apiserver — that would take the cluster
down for every other test in the run. It puts a TCP proxy
(`periscope_e2e::proxy`) in front of it, points a throwaway kubeconfig at the
proxy, and cuts it: established connections are reset and new ones refused, which
is what the app's sockets would see either way. Bytes are copied verbatim, so TLS
still verifies against the cluster's own CA.

## Workload fixtures

Some suites need workloads that produce something to read, or something to talk
to:

```sh
kubectl apply -f tests/e2e/fixtures/chatty.yaml     # three pods, five lines a second each
kubectl apply -f tests/e2e/fixtures/firehose.yaml   # four pods writing as fast as they can
kubectl delete -f tests/e2e/fixtures/firehose.yaml  # it burns CPU; delete it when done
kubectl apply -f tests/e2e/fixtures/webby.yaml      # one busybox pod serving a fixed string on 8080
kubectl apply -f tests/e2e/fixtures/sidecars.yaml   # one pod, two containers and an init container
```

`webby` is what the forward and exec suites use: it answers on a port, and its
busybox container has the handful of commands the exec tests run. Both suites
skip themselves, with instructions, when it is missing.

`sidecars` is what proves the container selector: its `alpha` and `beta`
containers each write their own name into their own filesystem, so `cat
/identity` says which container the command actually ran in.

Skipping is right on a laptop — nobody wants `firehose` burning a core all day —
and wrong in CI, where a suite that skips itself is indistinguishable from one
that passes. `PERISCOPE_E2E_REQUIRE_FIXTURES=1` turns a missing fixture into a
failure that names the fixture and the command to install it; CI sets it after
applying them. Use it locally to prove a run really covered everything:

```sh
PERISCOPE_E2E_REQUIRE_FIXTURES=1 cargo test -p periscope-e2e -- --ignored --test-threads 1
```

`chatty` scales up for the "tail fifty pods" measurement:
`kubectl scale deployment chatty --replicas=50`.

The CRD tests need custom resources to look at:

```sh
kubectl apply -f tests/e2e/fixtures/widgets.yaml   # our own CRD, no operator needed
kubectl apply -f https://github.com/cert-manager/cert-manager/releases/latest/download/cert-manager.yaml
kubectl create namespace argocd
kubectl apply -n argocd -f https://raw.githubusercontent.com/argoproj/argo-cd/stable/manifests/install.yaml
```

`widgets` is ours, so the basic custom-resource path does not depend on anyone
else's release cadence; the CRD-heavy discovery test wants the other two, which
is what a real cluster looks like. All three tests skip with instructions when
they are missing, and fail under `PERISCOPE_E2E_REQUIRE_FIXTURES`.

The YAML-rendering budget needs a large object:

```sh
cargo run --release -p periscope-e2e --bin seed-pods -- --large-config-map
```

A megabyte of generated text does not belong in git, so the fixture generator
makes it.

The two printer-column tests want cert-manager and Argo CD for the same reason:
they assert that `certificates` and `applications` come out with the headings
their own CRDs declare, and those are read from the cluster, so there is nothing
to fake.

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
