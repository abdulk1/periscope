# Known limitations

Honest, current, and published as-is. Updated at the end of every phase.

## Accessibility

GPUI has no mature accessibility layer. Periscope does not expose an
accessibility tree, does not support screen readers, and its focus handling is
whatever `gpui-component` provides. This is a **documented gap, not a solved
problem**, and it is out of scope for v1 by explicit decision. Anyone who depends
on a screen reader should use `kubectl` or `k9s` instead.

## Platform support

| Platform | Status |
|---|---|
| macOS (Apple silicon) | Developed and smoke-tested here |
| macOS (Intel) | Should work; untested |
| Linux (X11/Wayland) | Builds in CI; not smoke-tested against a display |
| Windows | Not built or tested yet (not a launch blocker) |

## GPUI version

Periscope pins `gpui 0.2.2` and `gpui-component 0.5.1` from crates.io, which are
roughly ten months behind GPUI `main` (see `docs/DECISIONS.md` ADR-0001). Features
that exist only on `main` are unavailable. `gpui_platform`, which `main` requires,
is not published at all.

## Cold start in debug builds

Measured on an M-series Mac, process start to first paint:

| Build | Cold start |
|---|---|
| Release, warm (n=14) | 122–158ms, median ~145ms |
| Release, first ever run | 527ms |
| Debug | ~1170ms |

The <500ms budget in `IMPLEMENTATION.md` §4 is met comfortably by release builds
and **missed by debug builds**, which are what `cargo run` produces. The budget is
therefore tracked against `--release`. Some of the debug cost is the runtime
shader compilation from ADR-0008; the rest is unoptimised GPUI layout code that
the `[profile.dev.package]` overrides only partly cover.

## Phase 1 scope

The build connects to a cluster and streams **pods, and only pods**. There is no
resource discovery, no CRDs, no other built-in kinds, no detail view, no YAML, no
logs, no namespace or label filtering and no command palette; those are Phases 2
and 3. The pod table has no sorting controls, no selection, and no keyboard
navigation — rows are ordered by namespace then name, always.

Pods are watched cluster-wide (`Api::all`). A credential that may only list pods
in one namespace will fail with a `403` and be reported as an auth failure; the
namespaced fallback is not implemented.

The store holds a table per cluster and switching between them is instant, but
the UI only auto-connects the cluster you are looking at, and clusters are
connected one at a time by clicking them. Concurrent multi-cluster watching, warm
idle clusters and side-by-side panes are Phase 4.

Disconnecting empties the table on purpose: after the watch stops we no longer
know what is running, and rows that look live but are not are worse than none.

## Measured against the Phase 1 budgets

On an M-series Mac against a one-node `kind` cluster (Kubernetes 1.36) seeded
with 10,009 pods, release build:

| Budget (`IMPLEMENTATION.md` §3, §4) | Measured |
|---|---|
| Cold start to window < 500ms | 225–273ms (release) |
| First render of a 10k-pod list < 3s | 1.6s from process start to 10,009 rows; 406ms of that is list + projection, 0.9ms is store insert and sort |
| External pod change visible < 1s | 17ms create → event, 15ms delete → event |
| Memory, 1 cluster / 10k pods < 300MB | 78MB RSS |
| Scroll frame rate ≥ 60fps | **not measured** — see below |

The load fixture is `cargo run --release -p periscope-e2e --bin seed-pods`.

## Frame rate is not instrumented

`--perf` logs watch throughput, coalescing, drops and how long each flush takes
to apply (851µs for a 10,009-row swap). It does **not** log frame times, so the
60fps floor and 120fps target in §4 are unverified. `uniform_list` builds only
the visible rows, and scrolling a 10k-row table looks smooth by eye, but no
number backs that up yet.

## Testing

Bridge tests that involve the tokio thread poll with a deadline rather than
blocking on a condition variable, because GPUI's test executor uses a virtual
clock that does not advance in step with a real background thread. Tests fail on
a timeout rather than hanging, but they are wall-clock sensitive and could be
flaky on a heavily loaded machine.

The `kind`-based suite lives in `tests/e2e` and is `#[ignore]`d, because it needs
a real cluster. It is not run by `cargo test` and has never run in CI; every
end-to-end result quoted here was produced locally with
`cargo test -p periscope-e2e -- --ignored --test-threads 1`.

## Authentication coverage

`kube` implements exec credential plugins, OIDC refresh, client certificates,
bearer tokens, in-cluster service accounts, proxies and custom CA bundles, and
Periscope enables all of those features. What has actually been *exercised*:

| Mechanism | Status |
|---|---|
| Client certificates (`kind`) | Verified |
| Bearer token rejected by the apiserver | Verified — surfaces as auth-failed with the apiserver's own text |
| Missing / malformed / empty kubeconfig | Verified |
| exec plugins (EKS, GKE, AKS), OIDC refresh, proxies, custom CAs | **Untested** — no such cluster was available |

The §2.5 requirement to test against a real EKS cluster is therefore **not met**.
The code path is `kube`'s and the error classification is covered by unit tests,
but nobody has watched `aws eks get-token` expire mid-session in this app.

## Unverified

- **Scroll and click at scale.** The rendered window has been inspected visually
  with 10,009 rows loaded (columns, colours, truncation, the context picker), but
  scrolling and context switching were not driven through the real UI — those
  paths are covered only by unit tests over the store and view state.
- **Light theme.** The theme toggle is wired and unit-covered; only the dark
  appearance has been looked at.
- **CI.** `.github/workflows/ci.yml` has still not run; nothing has been pushed
  to a remote. The Linux build dependency list and the new `kind` job are written
  from documentation rather than observed from a green run.
- **Linux.** Neither built nor run on Linux.
