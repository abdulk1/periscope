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

## Phase 0 scope

The current build is the skeleton only. It does **not** connect to Kubernetes:
there is no kubeconfig parsing, no client, no watches and no resource tables. The
"Clusters" panel shows a single placeholder entry named `local` whose only real
behaviour is the health probe round trip through the bridge. `--perf` currently
logs bridge throughput per flush; frame timing is not instrumented yet.

## Testing

Bridge tests that involve the tokio thread poll with a deadline rather than
blocking on a condition variable, because GPUI's test executor uses a virtual
clock that does not advance in step with a real background thread. Tests fail on
a timeout rather than hanging, but they are wall-clock sensitive and could be
flaky on a heavily loaded machine.

There is no `kind`-based integration suite yet; it arrives with Phase 1, when
there is a cluster to integrate with.
