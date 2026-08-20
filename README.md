# Periscope

A native, GPU-accelerated Kubernetes console. k9s-class capability with real UI
affordances: live resource streams, multi-cluster, and log tailing across pods.

**Binary:** `scope` · **Language:** Rust · **UI:** GPUI

It opens on your `current-context`, lists every kind the cluster serves — custom
resources included, with the columns their own definitions declare — and streams
any of them into a table that updates as the cluster does. Two clusters fit side
by side. Logs merge across every pod matching a selector. Anything that changes a
cluster asks first, in a sentence naming the cluster, the object and the
operation.

It requires no account, phones nothing home, and writes no credential to disk.

[What works today](#status) · [Run it](#run-it) · [The keyboard](#the-keyboard) ·
[Settings](#settings) · [Security posture](#security-posture) ·
[Working on it](#working-on-it)

## Status

**There is no release to download yet.** You build it from source, and on macOS
the bundle is unsigned. That is the honest headline; everything below is what
you get once you have.

Phases 0 through 5 — the console itself — are built and tested. Phase 6,
packaging and distribution, is what remains.

**Built:** browsing every kind a cluster serves, custom resources included, with
the columns their own definitions declare; a virtualised table that streams;
clusters that stay warm so switching back is instant; a fuzzy palette (⌘K) that
searches every warm cluster at once; merged log tailing across a selector; two
clusters side by side; delete, scale, restart, cordon, drain, apply-with-dry-run,
port-forwarding and running a command in a container — each behind a confirmation
naming the cluster, two independent read-only gates, and a local audit log. A
`settings.toml` covering theme, access, limits, columns and a fully remappable
k9s-style keymap. An opt-in update check. A macOS `.app` and `.dmg`, Debian and
RPM packages. Windows builds and passes its tests in CI, though nobody has
opened the window there.

**Not built:** code signing and notarization — the scripts are written and there
is no Developer ID to run them with — a Homebrew cask, an AppImage, screenshots,
and a describe-style summary. Exec runs a command and streams its output; it is
not a terminal, and ADR-0033 argues that half a terminal is worse than none.

**Not proven:** a real EKS or GKE handshake, a session longer than 90 minutes,
and the app running on a Linux desktop. Each is listed in
[`docs/LIMITATIONS.md`](docs/LIMITATIONS.md), which is where this project keeps
its honesty and is worth reading before you rely on anything here.

[`IMPLEMENTATION.md`](IMPLEMENTATION.md) has the roadmap.

## Run it

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

Its own logs go to a daily-rotating file under the platform's application data
directory; the path is printed in the log's first line and shown by `--verbose`.
If it cannot start at all — a malformed `settings.toml`, most likely — it says
so in a window rather than dying silently, unless you ran it from a terminal, in
which case the error is on stderr where you are already looking.

## The keyboard

| Key | Does |
|---|---|
| `j` `k` / `↑` `↓` | Move through the table |
| `g` / `G`, `home` / `end` | Jump to the first or last row |
| `enter` | Open the row you are on |
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

## Finding things

The sidebar groups every kind the cluster serves the way Rancher and the EKS
console do — workloads, networking, configuration, storage, access control,
cluster — with each custom resource under the API group that installed it, so
cert-manager's `certificates` sit under `CERT-MANAGER.IO`. Only workloads is
open at first; a section opens by itself when it holds whatever you have
selected, and the filter box above them narrows across all of them at once.

**RECENT** sits above the rest and holds the last six kinds you opened on the
cluster you are looking at, most recent first. It needs no curating — opening a
kind is the only thing that puts one there — and it is per cluster, because half
of what one cluster serves does not exist on the next. Which sections you left
open, and which kind each cluster was on, are remembered: reopening Periscope
puts a cluster back on the kind you left it on, or on pods if that kind is gone.
Which *cluster* you start on is still kubeconfig's current context, not the last
one you clicked — `kubectl config use-context` is a statement about where you
are, and this does not argue with it.

A custom resource shows the columns its own CustomResourceDefinition declares,
which are the ones `kubectl` prints for it: `certificates` come out as READY and
SECRET, Argo CD's `applications` as SYNC STATUS and HEALTH STATUS. Columns the
CRD marked as only worth showing in a wide listing are hidden, dates render as
ages, and a CRD that declares nothing falls back to STATUS and READY.

Click a column heading to sort by it, again to reverse, and a third time to put
the natural order back — numbers sort as numbers, so RESTARTS does not put 10
before 2. The natural order is namespace then name, which is what the table
already holds and therefore the one order that costs nothing; NAME sorts by
name, with the namespace only as the tie-break, because two objects in different
namespaces routinely share a name.

Moving through a table is `j`/`k` or the arrows, `ctrl-d`/`ctrl-u` for half a
screen, `g`/`G` for the ends, and `enter` to open what you are on. Half a screen
is half of what actually fits, so it keeps a few rows of context rather than
replacing everything you were reading. Finding an object by name is `⌘K`, which
searches every warm cluster; there is deliberately no second, weaker
type-to-jump inside the table. The **All namespaces** button beside the
namespace field lists the namespaces the loaded rows are in, so narrowing to one
does not mean knowing its name first; the field is still there for a namespace
nothing has been loaded from yet. The cursor is a highlight; the object the
detail pane is showing keeps a stripe down its left edge, so it stays findable
after you have moved on.

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

The time field jumps to a point in the buffer. It takes `14:32`, `14:32:10`,
`-5m` (also `s`, `h`, `d`) and an RFC3339 stamp pasted from anywhere; Enter
scrolls to the first line at or after it and pauses following. **Everything on
screen is UTC**, the timestamp column included.

**By time** orders the lines by their timestamps instead of by the order they
were read, which is what un-blocks a merged tail whose pods each delivered a
backlog before the live streams interleaved. Lines the apiserver gave no
timestamp for go last, in arrival order, and the pane says how many there are.

**Wrap** shows long lines whole. Wrapping and virtualisation cannot both be had,
so wrapped mode shows a window of 500 lines — the newest, or 500 from wherever
you last jumped — and says which lines those are. Use the filter or the time
field to move the window; scrolling will not take you through 100,000 wrapped
lines, by design.

## Changing things

The detail pane is tabbed — **YAML**, **Events**, **Related** — with counts on
the tabs that have something in them, so a misbehaving object's event list gets
the whole pane rather than 180 pixels at the bottom.

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
application logs. Port-forwarding and running a command are refused by the same
two gates, because both are `create` verbs: a tunnel to a production database is
a change to production.

On a read-only cluster the buttons are still **there**, disabled, each saying
why: *"`prod` is read-only. Change `[access]` in `settings.toml` to allow
writes."* They used to disappear, which was worse — an absent button is
indistinguishable from a button this program does not have, so the rule
protecting you became invisible at exactly the moment it applied. Disabling the
control is a courtesy to the reader and nothing more; the two gates are
untouched, and the keyboard and the palette still reach them (ADR-0043).

A kind you are not allowed to list says so too. A `403` is about the request,
not the credential, so it stops that one kind and leaves the rest of the cluster
streaming — and the table reads *"Not allowed to list secrets here"* rather than
"No secrets here.", which is a reassuring sentence and, for that user, a false
one (ADR-0040).

## Port forwards and commands

A pod's detail pane has a port field and **Forward**: it binds a local port on
`127.0.0.1` — never `0.0.0.0` — and the forwards panel shows the address to
paste, how many connections it has served, and, if something breaks, the
apiserver's own reason. Each connection gets its own stream, so one broken
connection does not take the forward down; a port nothing is listening on is
reported rather than accepted silently. Forwards outlive the pane that started
them, which is why the panel is always reachable from the header.

Next to it is a command field and **Run**. It runs one command in the pod and
streams stdout and stderr into the output pane, labelled by stream, ending with
the exit code. It is **not** a terminal: no interactive input, no `vi`, no
`top`. The command line is split on whitespace and is not a shell — `sh -c "ls |
wc -l"` is how you get one, and it runs in the container.

A pod with more than one container also gets a container button, listing its
containers and its init containers with **Default container** — the one the
apiserver would pick, which is what `kubectl exec` uses without `-c` — at the
top. The names come from the object the pane already fetched, so opening the
list costs nothing. Whichever is chosen is named in the confirmation, because
the same command in a sidecar and in the app are different acts.

The output pane has the log view's filter box, its `.*` and `Aa` toggles, and
its **Copy** and **Export** buttons, over the same bounded buffer: copying and
exporting take what the filter left on screen, in the order it is on screen.

Running a command is treated as a change, because it is: it needs the same
confirmation, is refused on read-only clusters, and is written to the audit log
with the command line as its detail.

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

[updates]
check = false               # the only non-cluster network call, off unless asked for
endpoint = "https://api.github.com/repos/abdulk1/periscope/releases/latest"

[keys]
palette = ["cmd-k", "ctrl-k", ":"]
dismiss = ["escape", "q"]
logs = ["cmd-l", "ctrl-l", "l"]
follow = ["cmd-shift-f"]
split = ["cmd-\\", "ctrl-\\"]
next = ["down", "ctrl-n"]
previous = ["up", "ctrl-p"]
confirm = ["enter"]
row-down = ["j", "down"]
row-up = ["k", "up"]
row-top = ["g", "home"]
row-bottom = ["shift-g", "end"]
half-page-down = ["ctrl-d"]
half-page-up = ["ctrl-u"]
open-row = ["enter"]
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

### `state.toml`

`settings.toml` is yours; `state.toml` is Periscope's. It lives in the data
directory beside `audit.log` — never in the config directory, because a program
that rewrites a hand-edited config file loses the comments in it — and it holds
what the sidebar looked like when you last closed the window:

```toml
# Written by Periscope. Settings live in settings.toml; this is
# remembered session state and is safe to delete.

[sections]
CLUSTER = true
WORKLOADS = false

[[recent]]
context = "kind-periscope"
kind = "deployments.apps"
```

Sections list only the headings you clicked; everything else follows its own
default. `recent` is the last thirty-two kinds opened, newest first, and each
cluster's sidebar reads its own entries out of it. Context names and kind names
are all that is in there — no server addresses, no namespaces, no object names,
nothing from a credential — and deleting the file simply starts a fresh session,
which is also what happens if it is ever unreadable.

## Security posture

No credentials are ever written to disk. No account, no activation, no login.
No telemetry, no phone-home, no crash reporting. The only network calls are to
the clusters you configure — plus, if you switch it on, one request to the
update endpoint at startup.

That check is off by default. When enabled it makes a single HTTPS GET, sends
nothing but a `periscope/<version>` User-Agent, downloads nothing, installs
nothing, and shows one line with a link if there is something newer. The
endpoint must be `https`, a redirect off it is refused, and the answer is read
in bounded chunks so a misconfigured endpoint cannot stream until the process
dies.

**Everything this program writes down is redacted and owner-only.** Log lines,
audit entries and exported buffers outlive the session and get attached to bug
reports, so hostnames and anything token-shaped are stripped on the way to disk
— including a failed credential plugin's own output, which is where a live
bearer token hides — while what is on screen keeps the apiserver's exact words,
because the person at the keyboard already holds the credentials. Files are
`0600` and directories `0700`. Panics are logged, redacted, and still printed in
full to stderr, because a panic on a background task otherwise takes a watch
down in silence.

Four of these invariants were held by everybody remembering, and a review found
two of them already broken. They are held by code now: `tests/guardrails` reads
the shipped source and fails the build when a shape that has already leaked
reappears — a `tracing::` call carrying a cluster URL or a token, an audit entry
written without the redactor, a file written without being restricted. Each test
names the incident it exists for. CI runs it beside `cargo audit`, `cargo deny`
and `gitleaks`.

Secrets are masked: the table shows how many keys a Secret has and never a
value, and its YAML shows `<hidden, N bytes>` until you press **Reveal values**,
which re-fetches it. Closing the pane masks it again. While it is masked,
**Apply** and **Dry run** are disabled rather than hidden, saying why — applying
a masked Secret would send the mask.

`docs/COMPETITORS.md` is blunt about which of these the evidence supports. The
absence of an account is the best-supported decision in this project; masking by
default is the one the market argues with.

## Packaging

For a real application bundle on macOS:

```sh
packaging/macos/bundle.sh      # -> target/bundle/Periscope.app and Periscope.dmg
```

On Linux:

```sh
cargo deb --package scope            # -> target/debian/periscope_<version>_<arch>.deb
cargo generate-rpm -p crates/scope   # -> target/generate-rpm/periscope-<version>.rpm
```

Both install `scope` to `/usr/bin`, a desktop entry and an icon; CI builds them,
installs the `.deb` and validates the desktop entry on every change.

The bundle is **unsigned** unless `PERISCOPE_CODESIGN_IDENTITY` and
`PERISCOPE_NOTARY_PROFILE` are set, so on a machine other than the one that
built it Gatekeeper refuses to open it until you right-click and choose Open.
There is no Developer ID behind this project yet; `docs/LIMITATIONS.md` is
explicit about what that costs.

## Working on it

```
crates/
├── scope/     binary: flags, window setup, wiring
├── ui/        GPUI views and components          (main thread only)
├── store/     state, indexes, filtering          (no GPUI, no kube)
├── cluster/   kube clients, watchers, logs       (tokio only)
├── bridge/    tokio <-> GPUI plumbing (the pump is an optional feature)
└── config/    paths, settings, themes, logging   (no GPUI, no kube)
```

The dependency edges are the architecture: `store`, `cluster` and `config` cannot
reach GPUI — the one file in `bridge` that does is behind a feature only `ui` and
`scope` enable — and `ui` cannot reach Kubernetes. Everything crossing between them
goes through `bridge` as a bounded, coalesced message stream.

Every change must leave these green:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
```

Anything touching a credential, a log line or a file on disk also has to leave
these green. They are not installed by default:

```sh
cargo install cargo-audit cargo-deny --locked
cargo test -p periscope-guardrails   # the invariants no type can express
cargo audit                          # RUSTSEC advisories against Cargo.lock
cargo deny check                     # licences, unmaintained crates, duplicates
```

The runners have no kubeconfig, no cluster and no keychain, so anything that
reaches for one passes here and fails there. Prove it before pushing:

```sh
mkdir -p /tmp/no-kubeconfig-home
env -u KUBECONFIG HOME=/tmp/no-kubeconfig-home \
    CARGO_HOME=$HOME/.cargo RUSTUP_HOME=$HOME/.rustup cargo test --workspace
```

The YAML the detail pane shows is pinned by golden files in
`crates/cluster/tests/golden/`: a JSON object, and the exact text it must render
as. After a deliberate change to the writer, rewrite them and read the diff —
an expectation updated without being read records the bug instead of catching
it.

```sh
PERISCOPE_UPDATE_GOLDEN=1 cargo test -p periscope-cluster --test golden
```

The end-to-end suite needs a real cluster, so `cargo test` skips it:

```sh
kind create cluster --name periscope
cargo test -p periscope-e2e -- --ignored --test-threads 1
cargo run --release -p periscope-e2e --bin seed-pods -- --count 10000  # load fixture
```

CI runs it on every push against a `kind` cluster seeded with 10,000 pods,
cert-manager, Argo CD and our own `widgets` CRD, with
`PERISCOPE_E2E_REQUIRE_FIXTURES=1` so that a missing fixture fails the build
instead of silently skipping fifteen tests.

### Documentation

| Document | For |
|---|---|
| [`AGENTS.md`](AGENTS.md) | Anyone changing this repository, human or otherwise — the invariants, the loop, the traps |
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | How the six crates fit together, and where a new feature goes |
| [`docs/STYLE.md`](docs/STYLE.md) | How the code, tests and commit messages are written |
| [`docs/TESTING.md`](docs/TESTING.md) | The six kinds of test, and which one you need |
| [`docs/DECISIONS.md`](docs/DECISIONS.md) | Why things are the way they are. Append, never rewrite |
| [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md) | What does not work, measured and unhidden |
| [`docs/COMPETITORS.md`](docs/COMPETITORS.md) | What the rest of the field does, what its users complain about, and which of this project's bets that evidence does *not* support |

To look at what you changed, on macOS:

```sh
cargo run --release --bin scope &
cargo run --manifest-path tools/winid/Cargo.toml -- scope   # prints a window id
screencapture -x -o -l<id> shot.png
```

That captures the application's window and nothing else on the display. See
[`tools/winid/README.md`](tools/winid/README.md).
