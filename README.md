# rq-tui

Minimal Rust terminal UI foundation for the code-review harness described in
the project specification.

## Run

```sh
bazel run //:rq-tui -- review .
bazel run //:rq-tui -- review --pr acme/api#123
```

The current milestone renders a local working-tree diff with keyboard scrolling
and establishes the `review`/`--pr` command shape. `q` or `Esc` exits the TUI.
Remote checkout/version history, SQLite persistence, and Copilot SDK session
integration are intentionally staged as the next vertical slices.

## Build

```sh
bazel build //:rq-tui
```

Bazel 9 and bzlmod are required. `MODULE.bazel` manages `rules_rust` and the
Cargo dependencies from the checked-in `Cargo.lock`.
