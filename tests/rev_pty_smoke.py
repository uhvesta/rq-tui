#!/usr/bin/env python3
"""Dependency-free PTY smoke test for rev's pane and quit key routing."""

from __future__ import annotations

import sys
import tempfile
from pathlib import Path

from pty_smoke import Child, git, make_fixture, terminal_text


def main() -> int:
    binary = Path(sys.argv[1]).resolve()
    with tempfile.TemporaryDirectory(prefix="rev-pty-") as temporary:
        root = Path(temporary)
        repo = make_fixture(root, "rev-fixture")

        # Commit two baseline files, then change both so the live-preview tree
        # has a meaningful second file to navigate to.
        (repo / "README.md").write_text("baseline\n", encoding="utf-8")
        git(repo, "add", ".")
        git(repo, "commit", "-m", "two-file baseline")
        (repo / "src" / "demo.rs").write_text(
            'fn main() {\n    println!("rev pane");\n}\n', encoding="utf-8"
        )
        (repo / "README.md").write_text("baseline\nchanged\n", encoding="utf-8")

        child = Child(binary, repo, root / "app")
        try:
            child.resize(120, 24)
            child.wait_for_screen("rev · rev-fixture", timeout=8)
            child.wait_for_screen("review · unified", timeout=8)

            child.send(b"t")
            child.wait_for_screen_state(
                required=("files · j/k preview", "review · unified", "src/demo.rs"),
                forbidden=(),
                timeout=4,
            )
            child.send(b"j")
            child.wait_for_screen("file 2/2", timeout=4)
            child.send(b"t")
            child.wait_for_screen_state(
                required=("NORMAL", "src/demo.rs"),
                forbidden=("files · j/k preview",),
                timeout=4,
            )

            child.send(b"q")
            child.wait_for_screen_state(
                required=("QUESTIONS", "No question threads yet."),
                forbidden=(),
                timeout=4,
            )
            child.send(b"q")
            child.wait_for_screen_state(
                required=("NORMAL", "src/demo.rs"),
                forbidden=("QUESTIONS ·",),
                timeout=4,
            )

            child.send(b":q\r")
            status = child.wait_for_exit(timeout=8)
            if status != 0:
                raise AssertionError(child.failure(f"rev exited with status {status}"))

            raw = bytes(child.output)
            text = terminal_text(raw)
            if "panicked at" in text or "thread 'main' panicked" in text:
                raise AssertionError(child.failure("panic text appeared in rev output"))
            if b"\x1b[?1049l" not in raw:
                raise AssertionError(child.failure("rev did not restore the alternate screen"))
        finally:
            child.close()

    print("REV_PTY_SMOKE_OK: tree-live-preview q-questions command-only-quit")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (AssertionError, OSError) as error:
        print(f"REV_PTY_SMOKE_FAILED: {error}", file=sys.stderr)
        raise SystemExit(1)
