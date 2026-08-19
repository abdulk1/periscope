# How Periscope is put together

One process, six crates, and two rules that the compiler enforces: nothing below
the view may touch GPUI, and nothing above the cluster layer may touch
Kubernetes. Everything else follows from those.

```
                    main thread                          its own thread
 ┌────────────────────────────────────────┐        ┌──────────────────────┐
 │  scope        flags, window, wiring    │        │  tokio runtime       │
 │  ui           GPUI views               │        │                      │
 │  store        what is true             │        │  cluster             │
 └────────────────────────────────────────┘        │  kube clients,       │
                    ▲          │                   │  watches, logs,      │
        ClusterEvent│          │ClusterCommand     │  mutations, exec     │
                    │          ▼                   └──────────────────────┘
                 ┌──────────────────────────────────────┐
                 │  bridge   bounded, coalesced channels│
                 └──────────────────────────────────────┘
```

| Crate | Owns | Cannot depend on |
|---|---|---|
| `scope` | flags, window options, wiring it all together | `kube` |
| `ui` | GPUI views, key bindings, the visual vocabulary | `kube` |
| `store` | state, filtering, sorting, permissions, buffers | GPUI, `kube` |
| `cluster` | clients, discovery, watches, logs, mutations, exec | GPUI |
| `bridge` | the command/event protocol and the plumbing | `kube` |
| `config` | paths, settings, state, audit log, logging | GPUI, `kube` |

`store`, `cluster` and `config` having no GPUI dependency is what makes their
logic testable without a window, and it is why most of this project's tests need
neither a display nor a cluster.

## The bridge (ADR-0003, ADR-0004, ADR-0005)

`kube` needs a tokio reactor; GPUI owns the main thread and runs its own
executors. Rather than reconcile them, `ClusterRuntime` builds a multi-threaded
tokio runtime on a dedicated `std::thread` and holds it for the process
lifetime. The two worlds meet only at `flume` channels.

Three properties matter:

- **Events are coalesced by key.** A 10,000-object resync must not become 10,000
  UI mutations, and 10,000 events for the *same* object must collapse to one.
  `ClusterEvent::coalesce_key` returns `None` for anything that must all be
  delivered — a `Pong` answers a specific ping.
- **Overflow behaves differently in each direction.** Events drop and mark the
  cluster stale, because blocking would apply backpressure all the way up a
  watch stream. Commands never drop silently: a lost command is a button that
  did nothing, so `send` returns `CommandError::Backpressure` and the UI says so.
- **Time is injected.** The coalescer takes `now` as a parameter, so the batching
  is tested without sleeping.

## What each layer is for

**`cluster` turns Kubernetes into data.** Objects arrive as `DynamicObject`, so
a Pod and a CRD nobody has heard of take exactly the same path; what differs is
the projector in `columns.rs` that turns them into rows. Columns travel to the
UI *as data*, which is the reason a custom resource renders like anything else
(and, since printer columns, renders the columns its author chose).

**`store` decides what is true.** The view asks it questions and never infers
state from what happens to be in a table — that separation is what makes "the
table is empty" and "the token expired" impossible to confuse. Filtering,
sorting, the keyboard cursor, the log ring buffer, permissions and the sidebar's
grouping all live here, and all are tested without a window.

**`ui` renders and sends.** It holds an `AppState` and asks it questions; it
never talks to Kubernetes and never decides what is true. Tables and log views
are virtualised through `uniform_list`, so a cluster with tens of thousands of
objects costs the same per frame as one with twenty.

## Where a new feature goes

Ask what kind of thing it is.

- *Reading something new from a cluster* → a `ClusterCommand` and a
  `ClusterEvent` in `bridge`, a handler arm in `cluster/src/handler.rs`, a fold
  in `AppState::apply`, and a view. Follow logs or exec end to end as a model.
- *A new decision about what is shown* → `store`. If a view is computing
  something interesting, move it.
- *Anything that changes a cluster* → `cluster/src/mutate.rs` or `exec.rs`, and
  it must pass the write policy and be audited. Nothing else in the cluster
  layer may write.
- *A new setting* → `config/src/settings.rs`. User-authored configuration goes
  in `settings.toml`; remembered UI state goes in `state.toml` beside the audit
  log (ADR-0038), because serialising over a file somebody hand-writes destroys
  their comments.
- *Something visual* → `ui/src/style.rs` first, then the view.

## Two gates, one sentence

Mutations pass the store's `authorize` and then the cluster layer's
`WritePolicy` (ADR-0028). The duplication is deliberate: the store's check is
what the UI asks, and the cluster layer's is what actually stands between a
protected cluster and a request. `Authorized` and `AuthorizedExec` have no
public constructor, so an unchecked mutation cannot be sent — a compile-time
property rather than a convention.

The confirmation sentence is generated from the same value that will be sent
(`Mutation::confirmation`, `ExecTarget::confirmation`), so the dialog and the
request cannot disagree.

## Threading rules

- Nothing in `crates/cluster` may assume it can touch a GPUI type. The compiler
  enforces this.
- Nothing in `crates/ui` may block. There is no `block_on` in the view layer;
  this one is a review rule, not a compiler rule.
- Mutexes in the cluster layer are held long enough to insert or take a handle,
  never across an await.

## Reference

- Decisions and their reasoning: [`DECISIONS.md`](DECISIONS.md), 39 ADRs.
- What does not work: [`LIMITATIONS.md`](LIMITATIONS.md).
- How it is tested: [`TESTING.md`](TESTING.md).
- How it is written: [`STYLE.md`](STYLE.md).
