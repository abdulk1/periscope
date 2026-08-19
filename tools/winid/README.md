# winid

Prints the window ids owned by a process, so a screenshot can be taken of one
window and nothing else:

```sh
cargo run --release --bin scope &
cargo run --manifest-path tools/winid/Cargo.toml -- scope
screencapture -x -o -l<id> shot.png
```

## Why this exists

`docs/LIMITATIONS.md` said for six phases that the application's appearance was
unverified. It was not laziness: `screencapture -R <rect>` captures whatever is
on that part of the display, which is usually somebody's editor sitting on top
of the window, and everything else on the screen along with it. Asking macOS
for the window id and capturing *that* gets the application and only the
application, even when it is behind something else — nothing private is caught,
and the layout can actually be looked at.

macOS only, and outside the workspace, so it never enters a build of the app or
runs in CI.
