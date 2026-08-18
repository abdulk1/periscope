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

## Phase 2 scope

The build connects to a cluster, discovers every kind it serves — CRDs included —
and streams any one of them into a table. What it does **not** do yet:

- **Logs.** No log tailing at all; that is Phase 3, and it is the differentiator.
- **Multi-cluster.** One cluster is watched at a time. The store holds rows for
  clusters you have visited, and switching back is instant, but nothing streams
  in the background and there are no side-by-side panes (Phase 4).
- **Mutations.** Still strictly read-only until Phase 5: no delete, scale,
  edit, exec or port-forward.
- **Sorting and column choice.** Rows are ordered by namespace then name, always.
  Columns per kind are what `columns.rs` says; they are not user-configurable.
- **A describe view.** The detail pane shows YAML, events and owner references.
  `kubectl describe`'s prose summary is not reimplemented.
- **CRD printer columns.** A custom resource renders with the generic
  `STATUS`/`READY` fallback rather than the `additionalPrinterColumns` its CRD
  declares. Reading those is a natural next step and is not done.
- **Namespace autocomplete.** The namespace filter is a text field. The
  namespaces seen in the current table are counted in the toolbar, but there is
  no picker.

Two behaviours worth knowing because they are deliberate:

- Switching kinds **stops** the previous watch (ADR-0019). Rows stay in the
  store; updates do not.
- The detail pane fetches the object when you open it (ADR-0018), so its YAML is
  current rather than a replay of the last watch event.

## Measured against the budgets

On an M-series Mac against a one-node `kind` cluster (Kubernetes 1.36) serving 68
kinds and seeded with 10,009 pods, release build:

| Budget (`IMPLEMENTATION.md` §3, §4) | Measured |
|---|---|
| Cold start to window < 500ms | 257–273ms |
| First render of a 10k-pod list < 3s | 459ms to list and project, 0.7ms to store and sort; ~1.6s from process start to a full table |
| External pod change visible < 1s | 9–17ms create → event, 8–15ms delete → event |
| Memory, 1 cluster / 10k pods < 300MB | 87MB RSS (78MB in Phase 1; discovery and 68 kinds account for the rest) |
| Scroll frame rate ≥ 60fps, target 120fps | 60.0fps sustained, 0 dropped frames; the display is 60Hz, so 120 could not be observed |
| Command palette < 50ms on a 10k-object cluster | 10,009 candidates scored and ranked well inside the budget; asserted by a unit test that fails over 50ms |
| Discovery of a CRD-heavy cluster | 68 kinds in ~130ms, custom resources included |

The load fixture is `cargo run --release -p periscope-e2e --bin seed-pods`.

## What the frame numbers do and do not say

`--perf` puts the window into continuous redraw and logs frame statistics every
120 frames: fps, p50/p95/max interval, element-build time, and how many frames
were late. Measured with 10,009 rows loaded:

```
frames=120 fps=60.0 p50_ms=16.66 p95_ms=17.15 max_ms=21.77
build_p50_us=198 over_budget=57 hitches=0 rows=10009
```

The 60fps floor is met with nothing to spare *because the panel is 60Hz*: every
frame is vsync-locked at 16.67ms and the app is not the limiting factor —
building the whole view costs ~200µs of that budget. `over_budget` counts
intervals over 16.67ms and hovers near half the frames purely from vsync jitter;
`hitches` (over 33ms, i.e. a frame genuinely skipped) stayed at 0.

Two honest caveats:

- **120fps is unverified.** No 120Hz display was available. What can be said is
  that the app spends ~1.2% of a 60Hz frame building its element tree.
- **This measures redraw, not scrolling.** Continuous full redraw is strictly
  more work per frame than scrolling a `uniform_list`, so it is a reasonable
  floor — but nobody has driven a scroll gesture under instrumentation.

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
| exec credential plugins, EKS-shaped (`client.authentication.k8s.io/v1beta1`) | Verified against a real apiserver with a stub plugin: the plugin runs, its token is sent, the rejection comes back with the apiserver's words |
| exec credential plugins, GKE-shaped (`client.authentication.k8s.io/v1`) | Verified — including a plugin returning a working credential, after which pods stream normally |
| A credential plugin that is not installed | Verified — the error names the missing binary |
| A credential plugin that exits non-zero | Verified — its stderr and exit status reach the UI |
| Expired credential re-fetched without user action | Verified — the client re-runs the plugin rather than reusing a dead credential |
| The real `aws` CLI as the plugin | Verified — `aws eks get-token` is run for real and the token it mints is sent (it signs an STS URL locally, so this works without an EKS cluster) |
| A real EKS or GKE cluster end to end | **Untested** |
| OIDC refresh, proxies, custom CA bundles, AKS `kubelogin` | **Untested** |

So the §2.5 requirement to test against a real EKS cluster is still **not met**:
what is proven is that Periscope drives the exec-credential protocol correctly
and reports every way it can fail, not that a real cloud handshake succeeds.
Doing better needs an EKS or GKE cluster, which costs money to run — see
`tests/e2e/tests/exec_auth.rs` for what would be reused if one existed.

## Unverified

- **Scrolling, clicking and typing in the real window.** The rendered window has
  been inspected visually with 10,009 rows and 68 kinds loaded, and the palette's
  key handling is driven through GPUI's real dispatch in tests
  (`cmd-k`, typing, arrows, `enter`, `escape`). But the environment this was
  built in does not grant the shell accessibility permission, so no scroll
  gesture, row click or filter keystroke has been sent to the running app by a
  human or a script. Those paths are covered by unit tests over the view state,
  which is not the same thing.
- **The detail pane's appearance.** Its data path is covered end to end against a
  real cluster (YAML, events, owner references), but the rendered pane — the
  syntax-highlighted editor in particular — has not been seen.
- **Light theme.** The theme toggle is wired and unit-covered; only the dark
  appearance has been looked at.
- **CI.** `.github/workflows/ci.yml` has still not run; nothing has been pushed
  to a remote. The Linux build dependency list and the new `kind` job are written
  from documentation rather than observed from a green run.
- **Linux.** Neither built nor run on Linux.
