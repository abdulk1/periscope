# The field, and what it says about our bets

Compiled 19 August 2026 from primary sources: GitHub issue trackers (counts via
the authenticated API), vendor pricing and documentation pages, Hacker News via
the Algolia API, Reddit r/kubernetes, and the CNCF annual survey. Every
substantive claim below traces to one of those.

This exists because several of Periscope's design bets were assumptions. Some
turned out to be the best-supported thing in the market and one turned out to be
backwards. Where the evidence contradicts what we built, that is recorded here
rather than argued away.

## How to read the numbers

Three caveats, all of which matter for anything you conclude:

- **Reaction counts favour old issues on big repos.** `lensapp/lens` (23.2k
  stars) and the retired `kubernetes/dashboard` (15.4k) accumulated reactions
  over six to ten years. Headlamp's 2026-vintage memory-leak bugs sitting at 0
  reactions are not thereby unimportant — they are newer, on a smaller repo, and
  filed by the minority who look.
- **Reddit fuzzes scores.** Two snapshots of the same thread gave 127 and 155
  points. Treat every Reddit figure as ±20% and never as precise. Scores are
  also current-date, so older threads have had years to accumulate.
- **The secondary literature on this topic is unreliable.** A dense cluster of
  SEO sites cite each other, several owned by vendors in this comparison, and
  their numbers contradict the vendors' own pricing pages. None of it was used.

## The one number that dominates

`lensapp/lens#5444`, "Unable to skip login page" — **923 reactions (755 👍),
356 comments**, opened the day Lens 5.5.0 shipped a mandatory account. The
next-highest issue in any of these trackers is 165. The community cares about
the login wall roughly **5.6× more than about anything else**.

From inside the thread, with real vote counts:

> **+202** *"Just adding to the voices, this is leading to an immediate removal
> of lens from my system."*
>
> **+103** *"Here's a workaround. Add the following to your /etc/hosts file:
> `127.0.0.1 app.k8slens.dev`"*
>
> **+38** *"Mandatory accounts are a dealbreaker for me."*

The most downvoted maintainer comment anyone has measured is in the sibling
thread, where Mirantis explained why pod shell and logs were removed from
OpenLens: **437 👎 against 1 👍**. Two follow-ups drew 97 👎 each with zero 👍.

It is still policy. A Lens ID is mandatory for every user including the free
Personal tier, and offline activation is Enterprise-only.

**This is the single best-supported product decision we have made.** No account,
no activation, no phone-home. It should be the first line of anything we write
about ourselves.

## The competitive field

| Tool | Stars | Licence | State |
|---|---|---|---|
| **k9s** | 34,394 | Apache-2.0 | The incumbent. TUI. 115 open issues, releasing |
| **Headlamp** | 7,107 | Apache-2.0 | CNCF Sandbox; now `kubernetes-sigs`, under SIG UI. The anointed successor to Dashboard |
| **Freelens** | 5,441 | MIT | The live OpenLens successor. 2.34M downloads in 19 months |
| **OpenLens** | 4,434 | none declared | **Dead.** Last release 2023-06-30 |
| **Kubernetes Dashboard** | 15,423 | Apache-2.0 | **Archived 2026-01-21**, moved to `kubernetes-retired` |
| **Aptakube** | 859 | closed | The serious paid competitor. Tauri + Rust |
| **Lens** | 23,220 | closed | Source deleted 2024-01-23. Pivoting to AI-agent governance |

Two structural facts worth holding:

**Lens is leaving the category.** The source tree was deleted in a single commit
titled "first draft of new readme" (−452,320 lines). The last three major
announcements are MCP Server, Lens Agents and Lens Prism; Mirantis was acquired
by IREN, an AI-datacentre company, on 2026-08-04, and the announcement does not
mention Lens by name. Lens Plus is now $25/user/month.

**The landing zone is free.** When Dashboard was archived, the Kubernetes blog
named Headlamp its successor. So the challenger's old pitch — "Lens costs $300 a
seat and makes you log in" — no longer describes the buyer's real alternative,
which is free, officially-endorsed Headlamp or free, actively-forked Freelens.

## Where our bets are supported

**No account.** See above. Nothing else is close.

**Read-only enforced below the UI.** Read-only is *provably broken* in three
separate products — `dashboard#6489`, `dashboard#7632`, `k9s#3822` all report
the UI hiding an action while the action still executes. Enforcing it in the
store, where no UI bug can route around it, is genuinely novel. The reaction
counts are near zero, but the pattern repeats across a decade and three
codebases: this is a correctness gap filed by the people who care, not a
popularity contest.

**Confirmation naming cluster, object and operation.** The evidence is not a
feature request; it is an incident. r/kubernetes, **"Deleted All Namespaces by
mistake IN PRODUCTION!!!!"** — ~262 points, 180 comments:

> *"i accidentally deleted all (**in lens ui all namespaces are selected by
> default**)"*

with the top substantive reply: *"This is why you shouldn't use a dev tool like
lens in prod, too easy to make changes."* Nobody has filed this as a feature
request anywhere. The demand is visible only in postmortems, which is exactly
why it is unbuilt.

**Performance at scale — with one caveat.** Every GUI breaks somewhere:
Headlamp OOMs browsers at 2,000 namespaces and has open leak bugs filed in 2026;
Aptakube users report hangs at 15k–30k pods against vendor load-testing that
stops at 20k; Lens admitted on 2026-08-19 that it had cut background processing
"from over 7 seconds to about a quarter of a second." The caveat is in the next
section.

## Where our bets are not supported

This section is the reason the research was worth doing.

**No telemetry — thin.** Every telemetry issue across four repos has **0–2
reactions**: `lens#7828` (0), `lens#1543` (0), `lens#910` (0), `lens#443` (0),
`lens#3697` (2). None found in `k9s` at all. Against 755 on the login issue.
There is one genuine episode, from 2020 on Hacker News, and after that it is
background grievance and never a thread of its own.

Ship it, keep it, mention it in one line. Do not build the identity on it. What
people demonstrably shop on is *open source, no vendor lock, no account* — of
which "no telemetry" is a consequence they will simply assume.

**Secrets masked by default — actively counter-indicated.** There are **zero
issues in any tracker with "mask" in the title**. The demand runs the other way:

- `k9s#1017`, "Modify secret without worrying about base64 encode/decode" — 42
  👍, 23 comments, open. **This is the top open issue on k9s by reactions.**
- `k9s#373` "Secret decoded content is not shown", `dashboard#2782`,
  `headlamp#2412`, `k9s#123` — all asking for transparent decode.
- OpenLens users recommended a community extension whose purpose was to *decode*
  certificate secrets.

No thread anywhere complains that a GUI shows decoded secrets. The one genuine
leak report, `dashboard#2798`, is an RBAC correctness bug, not a masking
request.

We keep masking — it is defensible and the reveal action is one click — but the
framing should change. It is a **screen-share mode**, a real and unarticulated
need, not a security control the market asked for. Anything that makes reveal
slower than one deliberate action is friction we cannot justify from evidence.

**Local audit log — thin.** No client-side audit-log request exists in any
tracker. Every audit discussion found concerns *server-side* Kubernetes API
audit logs, which are authoritative, tamper-resistant, and already record
everything this client does. A local file the user can delete is not an audit
control and enterprises will not accept it as one. It stays — it is cheap and it
is genuinely useful for "what did I do at 3am" — but it is not a selling point
and should not be described as one.

**GPU acceleration — zero demand found.** Nobody asked for it. "GPU" appears in
the corpus only as a *problem*: `lens#7227`, "OpenLens Helper (GPU) process high
memory usage". Say "native" and "60fps on a 10,000-row table". Never say GPU.

**Credentials never written to disk — true but not differentiating.** Aptakube
already claims the equivalent and no user complains about the status quo. There
is a better version of this bet, below.

## What is in demand that we are not building

Ranked by evidence, strongest first.

1. **Cloud authentication that actually works.** The #1 enterprise blocker
   across every product in the field. `lens#208` — MFA for
   `aws-iam-authenticator` — has been **open six years** with 88 reactions.
   Headlamp's entire top-issue list is OIDC/EKS/AKS/exec-plugin failures, five
   of them labelled `blocker` *and* `lifecycle/frozen` simultaneously. Users say
   plainly that it is why they left: *"I was struggling with OIDC hence gave
   up"*; *"After not being able to use Keycloak, I just abandoned the project."*

   This is the highest-value thing we could build, and it converts our security
   story into a *capability* story: handling exec credential plugins, SSO, MFA
   and refresh flawlessly is the natural product of never writing credentials
   down.

2. **Metrics without Prometheus.** `dashboard#2986`, "Support metrics API" —
   **270 👍**, the highest-reacted feature request found anywhere, and more than
   6× anything else in that tracker. Still the top ask: **four of the top nine
   open Freelens issues** are some form of "support metrics-server, not just
   Prometheus". Eight years unserved by anyone. It is also the stated reason GUI
   holdouts stay on Lens: *"Lens shows you metrics graphs with requests and
   limits watermarks. It gives you instant understanding."*

3. **Aggregate logs across a workload's pods.** `lens#272` open since 2020;
   `k9s#827` at 55 👍; two open Freelens issues. Independently, the only
   log tool Hacker News ever upvoted (Kubetail, 126 points) does exactly this.
   Aptakube ships it and charges for it.

4. **User-definable JSONPath columns.** `k9s#755` (199 👍, open nearly five
   years before it shipped) plus `k9s#605` (147 👍) — **346 👍 combined**, the
   highest-signal pure feature ask in the TUI world. Lens has never had it. We
   have CRD printer columns, which is the hard half; user-defined columns are
   the adjacent win.

5. **A real answer for CRDs.** *"no Kubernetes UI has provided a good interface
   for CRDs. My usecases traffic in them heavily. I ended up vibing my own UI
   from scratch."* Uncontested territory.

6. **File transfer to and from pods.** `lens#1369` (98 👍, open since 2020),
   `k9s#658` (84 👍).

## The design correction this produced

Headlamp's headline differentiator is RBAC-derived UI that hides what your role
forbids. It is also the most criticised thing about it, consistently since its
2020 launch:

> *"Hiding UI based on permissions creates an infuriating UX, because there's no
> way for the operator to tell what actions are possible-but-forbidden."*
>
> *"I strongly recommend disabling buttons instead of hiding them… **hiding
> buttons gaslights people**. To go the extra mile, put a tooltip that actually
> states why the button is disabled."*
>
> *"this breaks completely if another authorizer than RBAC is involved."*

Periscope was doing the same thing — every mutating control vanished on a
read-only cluster — and it has been changed. See **ADR-0043**: the control
renders, disabled, with the reason attached. `docs/LIMITATIONS.md` and the
`gated()` helper carry the rule.

The real-world failure mode is worse than the argument suggests: `lens#1682` and
`lens#7385` report that restricted users get **empty sections and infinite
spinners** rather than any explanation at all. `lens#3558`, "Gracefully degrade
features when missing certain permissions", has been open since 2021. Doing this
properly is a genuine enterprise story and nobody has done it.

## The uncomfortable part

**k9s is not a competitor; it is the default.** In *every* "alternative to X"
thread examined, the top comment is some form of "k9s". When the official
Dashboard was retired, the top comment on the r/kubernetes thread was *"k9s. Am
I the only user here? :)"* at 59–73 points. "Just a k9s appreciation post" is
the highest-scoring tooling thread in the corpus at ~430 points.

Our performance claim beats Lens, Headlamp and the browser. It does not
obviously beat k9s. On a rival's launch thread, founded explicitly on the
premise that existing UIs do not scale, the community's answer was *"k9s does
1000+ nodes without a problem at all."*

**The failure mode is abandonment, not rejection.** The most useful single
sentence found:

> *"I always liked the idea of Headlamp, have it installed full w plugins but
> somehow I end up not using it… In the end of the day I feel like my best UI is
> little to no ui."*

A GUI can win the download and still lose the habit. The metric that matters is
not "did they try it" but "did it survive the third incident at 3am".

**Distribution is a harder problem than engineering.** Every Show HN in this
exact position:

| Post | Points |
|---|---|
| Seabird — native Kubernetes desktop client (Go + GTK) | **3** |
| Krust — fast open-source Kubernetes TUI in Rust | **2** |
| Freelens | **2** |
| Sofka — "much faster k9s alternative in Rust" | **5** |

Against Headlamp's launch at 126 points and Kubetail's at 126. **"Native, fast,
Rust" is empirically not a story anyone shows up for.** Headlamp landed on
governance; Kubetail landed on solving logs. And r/kubernetes is now openly
hostile to new entrants: *"all these vibe coded dashboard projects don't last a
post, people try it and drop it immediately."*

**Willingness to pay is low, and the reasoning is consistent.** A commenter
arguing that a $249/year tool is obviously worth it was downvoted to −12; the
reply that landed was *"I personally couldn't justify paying it, with the free
alternatives being as good as they are."* Even k9s failed to monetise. Aptakube,
the best-executed paid product in the field, reports "a few new paid users every
week" after three years — that is probably the honest ceiling.

**Two things a terminal structurally cannot do**, and therefore worth more than
speed:

- **A URL you can send to someone who has installed nothing.** *"when my users
  ask questions like 'what is a kubernetes?' it's nice to be able to send them a
  link."* This is the strongest argument found for a web-deployable mode, and it
  is not a different market — it is the same SRE needing to hand a link to a
  developer.
- **Plugins that render as first-class UI.** Headlamp has 99 published plugins
  with vendors shipping their own; k9s has ~50 YAML shims that shell out and
  take over the screen. In-app rendering is the mechanism that produces an
  ecosystem rather than a config directory.

## What we would say if we had to say one thing

Our safety bets — no account, read-only enforced below the UI, confirmation
naming the cluster and object, performance at real scale — are backed by genuine
and under-served demand and are worth building.

Our privacy bets — no telemetry, secrets masked, local audit log — are answers
to questions nobody is asking, and one of them contradicts the loudest open
feature request in the space.

And the thing most likely to decide whether anyone gets far enough to notice
either is whether the app can log into their EKS cluster on the first try.
