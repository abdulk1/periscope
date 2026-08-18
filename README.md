# Periscope

A native, GPU-accelerated Kubernetes console. k9s-class capability with real UI
affordances: live resource streams, multi-cluster, and log tailing across pods.

**Binary:** `scope` · **Language:** Rust · **UI:** GPUI

> **Status: Phase 5 (actions), with one deliberate deviation — exec runs a
> command, it is not a terminal.** Connects to kubeconfig contexts on
> demand, discovers every kind each serves — CRDs included — streams any of them
> into a virtualised table, tails logs from one pod or from every pod matching a
> label selector, and shows two clusters side by side. Clusters you have visited
> stay warm, so switching back is instant. Fuzzy jump palette (⌘K) searches
> every warm cluster at once.
>
> It can now change things: delete, scale, restart, cordon, drain, apply edited
> YAML (with a dry run first), forward a local port onto a pod, and run a command
> in a container. Everything that changes anything shows a confirmation naming
> the cluster, passes two independent read-only gates, and is written to a local
> audit log. Exec runs a command and streams its output; the spec asked for
> terminal emulation and that is not built — see ADR-0033.
> See [`IMPLEMENTATION.md`](IMPLEMENTATION.md) for the roadmap and
> [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md) for what does not work.

## Build and run

Requires Rust stable (developed on 1.97.1; MSRV 1.89) and a kubeconfig.

```sh
cargo run --release --bin scope                        # open the window
cargo run --release --bin scope -- --kubeconfig ./kc   # use one specific file
cargo run --release --bin scope -- --verbose           # mirror the log to stderr
cargo run --release --bin scope -- --perf              # log watch throughput and flush timings
cargo run --release --bin scope -- --tail app=web -n prod  # open straight into the log view
```

It connects to the `current-context` on start; the sidebar lists every context
and every kind the cluster serves. Prefer `--release`: debug builds miss the
cold-start budget by a wide margin (`docs/LIMITATIONS.md`).

For a real application bundle on macOS:

```sh
packaging/macos/bundle.sh      # -> target/bundle/Periscope.app and Periscope.dmg
```

The bundle is **unsigned** unless `PERISCOPE_CODESIGN_IDENTITY` and
`PERISCOPE_NOTARY_PROFILE` are set, so on a machine other than the one that
built it Gatekeeper refuses to open it until you right-click and choose Open.
There is no Developer ID behind this project yet; `docs/LIMITATIONS.md` is
explicit about what that costs.

| Key | Does |
|---|---|
| `⌘K` / `ctrl-K` / `:` | Jump to a cluster, a kind, or an object by name |
| `↑` `↓` / `ctrl-P` `ctrl-N`, `enter` | Move through the jump results and open one |
| `escape` / `q` | Close the palette, then the command output, the log view, the detail pane |
| `enter` in the namespace or selector field | Re-list with that filter |
| `⌘L` / `ctrl-L` / `l` | Tail the open pod, or every pod matching the current namespace + selector |
| `⌘⇧F` | Follow the newest line, or pause where you are |
| `⌘\` | Show two clusters side by side, or go back to one |

The single-letter keys are k9s's, and they only fire when no text field has
focus — typing `l` into the namespace filter types an `l`. All of them are
remappable; see Settings below.

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

## Several clusters

Clusters connect when you first look at one, not at startup, and keep streaming
after you move away — switching back shows what is already held rather than
re-listing. `⌘\` splits the window so two clusters sit side by side; clicking a
pane points the sidebar, the filters and the palette at it. A cluster nobody has
looked at for five minutes is let go: its watches stop and its rows are freed,
while its connection is kept so returning does not mean authenticating again.

The palette searches every warm cluster, not just the one on screen, and says
which cluster a hit is on when it is not the one you are looking at.

## Logs

Open a pod and press **Logs**, or set a namespace and a label selector in the
table's filters and press `⌘L` to merge every matching pod into one stream. Each
pod keeps its own colour, new pods are attached as they appear, and a pod that
is replaced is re-attached without asking.

The buffer holds 100,000 lines and drops the oldest beyond that, saying how many
it dropped. Filtering — substring or regular expression, case-sensitive or not —
applies to what is already held, so it never restarts the stream. **Copy** puts
the visible lines on the clipboard; **Export** writes them to a file and tells
you where.

## Changing things

Open an object and the detail pane offers what its kind supports: **Scale**,
**Restart**, **Cordon**, **Drain**, **Dry run**, **Apply** and **Delete**.
Nothing happens until you confirm a sentence that names the cluster, the
namespace, the object and the operation — *"Delete deployments.apps api in
namespace payments on cluster prod?"* — and `Escape` cancels it.

A drain cordons the node and evicts its pods through the eviction API, so
PodDisruptionBudgets are respected. DaemonSet and mirror pods are skipped, and a
pod the apiserver refuses to evict is reported rather than forced.

Mark the clusters that must never change in `settings.toml`:

```toml
[access]
read-only = ["prod", "prod-eu"]

# Or invert it: nothing is writable unless named.
read-only-by-default = true
writable = ["kind-local"]
```

Those names are refused twice: once by the store, before anything is sent, and
again by the cluster layer, immediately before the request. Every attempt —
applied, dry-run, refused or failed — is appended to `audit.log` beside the
application logs.

## Settings

`settings.toml` lives in the platform's config directory beside the log
directory. It is optional — no file means defaults — but a malformed one is
refused rather than ignored, because starting with permissive defaults when
somebody has written a read-only rule is the worst thing this file could do.

```toml
theme = "system"            # system | light | dark

[access]
read-only = ["prod"]

[limits]
idle-timeout = "5m"         # how long a cluster stays warm after its pane closes
row-budget = 200000         # rows one cluster may hold before unviewed tables are freed
log-buffer = 100000         # lines a log or command buffer keeps

[columns]
# Which columns a kind shows. NAMESPACE, NAME and AGE are structural and always
# there; this chooses among the rest, in the order you write them.
pods = ["READY", "STATUS", "NODE"]
"deployments.apps" = ["READY", "UP-TO-DATE"]

[keys]
palette = ["cmd-k", "ctrl-k", ":"]
dismiss = ["escape", "q"]
logs = ["cmd-l", "ctrl-l", "l"]
follow = ["cmd-shift-f"]
split = ["cmd-\\", "ctrl-\\"]
next = ["down", "ctrl-n"]
previous = ["up", "ctrl-p"]
confirm = ["enter"]
```

Durations are written the way people write them — `30s`, `5m`, `1h` — and a bare
number is seconds. Unknown keys are ignored, so a file written by a newer version
does not stop an older one from starting. Settings are read once at startup.

A command listed under `[keys]` **replaces** its defaults rather than adding to
them, and `command = []` unbinds it entirely; anything not listed keeps its
defaults, so remapping one key does not silently drop the rest. A misspelled
command name is refused with a message naming what was expected — a keymap that
ignores a line it does not understand gives you a key that does nothing and no
way to tell that from a bug. A keystroke that is not a key is reported on screen
and skipped, and the rest of the keymap still works.

Column names are matched without regard to case. A column a kind does not have
is ignored rather than fatal — column sets differ between Kubernetes versions
and between CRDs — and if none of the names match, the kind keeps all of its
columns, because a table with nothing in it looks like a bug rather than a
setting.

## Port forwards and commands

A pod's detail pane has a port field and **Forward**: it binds a local port on
`127.0.0.1` — never `0.0.0.0` — and the forwards panel shows the address to
paste, how many connections it has served, and, if something breaks, the
apiserver's own reason. Each connection gets its own stream, so one broken
connection does not take the forward down; a port nothing is listening on is
reported rather than accepted silently. Forwards outlive the pane that started
them, which is why the panel is always reachable from the header.

Next to it is a command field and **Run**. It runs one command in the pod's
default container and streams stdout and stderr into the output pane, labelled
by stream, ending with the exit code. It is **not** a terminal: no interactive
input, no `vi`, no `top`. The command line is split on whitespace and is not a
shell — `sh -c "ls | wc -l"` is how you get one, and it runs in the container.

Running a command is treated as a change, because it is: it needs the same
confirmation, is refused on read-only clusters, and is written to the audit log
with the command line as its detail.

## Security posture

No credentials are ever written to disk. No telemetry, no phone-home, no crash
reporting. The only network calls are to the clusters you configure.

Secrets are masked: the table shows how many keys a Secret has and never a
value, and its YAML shows `<hidden, N bytes>` until you press **Reveal values**,
which re-fetches it. Closing the pane masks it again.
