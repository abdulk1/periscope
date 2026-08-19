# How this code is written

The point of a house style is that a reader can move through the whole codebase
at one speed. Everything here is already true of the code — this document
describes it rather than proposing it. Every example is real.

## Comments explain why, never what

The code says what it does. A comment that repeats it is noise that has to be
maintained and will eventually lie.

```rust
// Wrong — says what the next line says.
// Insert the row into the map.
self.rows.insert(row.key.clone(), row);
```

```rust
// Right — says the thing the code cannot.
// Deliberately not marked as changed: a resync replaces everything, and
// flashing every row would say "all of this just happened" when what
// happened is that the watch reconnected.
self.changed.clear();
```

Good reasons to write a comment: a decision that had alternatives; a constant
whose value is not arbitrary; a workaround for something outside this repository;
an invariant the compiler cannot express; a bug that a shape of code invites.

If the reason is big enough to need a paragraph, it is probably an ADR in
`docs/DECISIONS.md`, and the comment should point at it.

## Doc comments carry the reasoning, not the signature

```rust
/// How long a cluster nobody is looking at keeps streaming before it is let go.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
```

Public items are documented — the workspace warns on missing docs. Say what it
is *for*. `/// Gets the cluster.` on `fn cluster()` is worse than nothing.

Module headers earn their length. They are where the shape of a file is
explained: read the top of `crates/cluster/src/logs.rs` or
`crates/bridge/src/coalesce.rs` for the standard.

## Names are the documentation that cannot go stale

- Types and functions say what the thing *is*, not how it is built:
  `WritePolicy`, `Authorized`, `ResourceStream`, `sections`.
- No abbreviations that a Kubernetes operator would not use. `namespace`, not
  `ns`. `kind`, not `k`.
- Booleans read as claims: `is_problem`, `may_mutate`, `watchable`.

## Tests are sentences

A test name is a claim about behaviour, and a failing test should read like a
bug report:

```rust
#[test]
fn a_row_disappearing_under_the_cursor_moves_it_rather_than_stranding_it() { … }

#[test]
fn a_read_only_cluster_refuses_to_run_a_command() { … }

#[test]
fn versions_order_by_number_not_by_string() { … }
```

Not `test_cursor`, not `cursor_works`. If the name needs "and" twice, it is two
tests.

Where the reason a test exists is not obvious from its name, one comment says
it — usually the bug it prevents:

```rust
// The bug this prevents: "0.10.0" < "0.9.0" when compared as text.
```

Assertions carry context. `assert!(x)` tells you nothing at 3am;
`assert!(reason.contains("read-only"), "{reason}")` tells you everything.

## Errors say what failed, where, and what to do

The audience is an SRE who wants the underlying text, not a summary.

- Name the object and the cluster. The apiserver's own message for a missing pod
  is `404 Not Found`, which identifies nothing — so the target is prefixed:
  `default/api-0:8080: 404 Not Found`.
- Never swallow a cause. `crates/cluster/src/errors.rs::describe` walks the whole
  chain, because `thiserror` prints only the outermost message and for `kube`
  that is usually the least useful half.
- A refusal says what would allow it: *"prod is read-only. Remove it from
  `read-only` in settings.toml to allow changes."*
- Nothing fails silently, and nothing is a spinner without a reason.

## Failure handling

- **No `unwrap()` outside tests.** In tests, `expect("reason")`.
- A lock that could be poisoned is recovered, not panicked on, when it holds
  something that cannot be half-updated — see `guard` in
  `crates/cluster/src/handler.rs`.
- Failing to write the audit log is never fatal and never silent: it logs at
  `error` and the mutation proceeds.
- `unsafe` is forbidden workspace-wide.

## The shape of a change

- Logic that decides *what is true* belongs in `crates/store`, where it is
  testable without a window or a cluster. If a view is computing something
  interesting, it is in the wrong place.
- Types cross the bridge as plain data. Columns travel with rows, which is what
  lets a CRD render exactly like a Pod.
- Prefer making an invalid state unrepresentable over checking for it. There is
  no public constructor for `Authorized`, so a mutation that was never checked
  cannot be sent.

## Views

- No view sets a colour or a spacing by hand. The vocabulary is
  `crates/ui/src/style.rs`; if a value is missing, add it there.
- Colour means something. In a table it is reserved for row state, so anything
  coloured is something wrong or changing.
- Any new key binding goes through `periscope_config::Command` so it stays
  remappable, and through `context_of` so it does not steal keystrokes from text
  fields.

## Commit messages

Prose, in full sentences, explaining the reasoning. Not a changelog, not a list
of files. The first line is what changed, in the imperative. The body answers
"why was this worth doing" and, when something was found on the way, says so
plainly — the defects found while building a feature are the most valuable part
of the message.

Look at `git log` before writing one. A good example:

> **Give CI the fixtures the tests were quietly assuming**
>
> The first CI run failed two jobs, and both were the same mistake in different
> places: tests depending on state that only ever existed on the machine they
> were written on. […]

Say what you did *not* do, and why, when it is a choice a reader would otherwise
wonder about.

## Documentation

- `docs/LIMITATIONS.md` is updated in the same change that makes it wrong. It is
  the project's honesty and it is not optional.
- `docs/DECISIONS.md` is append-only. Supersede an ADR with a new one; never
  rewrite history.
- A deviation from `IMPLEMENTATION.md` is recorded as a deviation, in those
  words, with the reasoning — not quietly reinterpreted as compliance. See
  ADR-0033.
