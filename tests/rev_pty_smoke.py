#!/usr/bin/env python3
"""Dependency-free PTY smoke test for rev's pane and quit key routing."""

from __future__ import annotations

import sys
import tempfile
import time
import urllib.request
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

        cmux_log = root / "cmux.log"
        cmux = root / "cmux"
        cmux.write_text(
            "#!/bin/sh\n"
            f"printf '%s\\n' \"$*\" >> {cmux_log!s}\n"
            "case \" $* \" in\n"
            "  *' browser '*' open-split '*) "
            "printf '%s\\n' '{\"result\":{\"workspace_id\":\"workspace:7\","
            "\"surface_id\":\"surface:9\"}}' ;;\n"
            "  *) printf '%s\\n' '{\"result\":{}}' ;;\n"
            "esac\n",
            encoding="utf-8",
        )
        cmux.chmod(0o755)
        child = Child(
            binary,
            repo,
            root / "app",
            extra_env={
                "CMUX_BUNDLED_CLI_PATH": str(cmux),
                "CMUX_SOCKET_PATH": str(root / "cmux.sock"),
                "CMUX_WORKSPACE_ID": "workspace:7",
                "CMUX_SURFACE_ID": "surface:3",
            },
        )
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
            child.send(b"\r")
            child.wait_for_screen_state(
                required=("NORMAL", "src/demo.rs"),
                forbidden=("files · j/k preview",),
                timeout=4,
            )
            child.send(b"t")
            child.wait_for_screen("files · j/k preview", timeout=4)
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

            child.send(b"h")
            child.wait_for_screen("README.md", timeout=4)
            child.send(b"M")
            child.wait_for_screen_state(
                required=("MARKDOWN RICH DIFF", "baseline", "changed"),
                forbidden=("Markdown diff", "Mermaid"),
                timeout=4,
            )
            for _ in range(40):
                if cmux_log.exists():
                    break
                time.sleep(0.05)
            if not cmux_log.exists():
                raise AssertionError(child.failure("cmux rich-diff opener was not invoked"))
            open_calls = cmux_log.read_text(encoding="utf-8").splitlines()
            if len(open_calls) != 1:
                raise AssertionError(child.failure(f"expected one cmux open, got {open_calls!r}"))
            if " browser --surface surface:3 open-split " not in f" {open_calls[0]} ":
                raise AssertionError(child.failure(f"bad cmux command: {open_calls[0]!r}"))
            opened_url = next(
                (part for part in open_calls[0].split() if part.startswith("http://127.0.0.1:")),
                "",
            )
            if not (
                opened_url.startswith("http://127.0.0.1:")
                and "/review/" in opened_url
            ):
                raise AssertionError(
                    child.failure(f"unexpected rich-diff URL: {opened_url!r}")
                )
            with urllib.request.urlopen(opened_url, timeout=2) as response:
                if response.status != 200:
                    raise AssertionError(child.failure("rich-diff URL was not live"))

            child.send(b"jkjk")
            time.sleep(0.35)
            open_calls_after_scroll = cmux_log.read_text(encoding="utf-8").splitlines()
            if open_calls_after_scroll != open_calls:
                raise AssertionError(
                    child.failure(
                        "scrolling opened another cmux surface: "
                        f"{open_calls_after_scroll!r}"
                    )
                )
            with urllib.request.urlopen(opened_url, timeout=2) as response:
                if response.status != 200:
                    raise AssertionError(child.failure("rich-diff URL died after scrolling"))
            child.send(b"M")
            child.wait_for_screen_state(
                required=("README.md", "NORMAL"),
                forbidden=("MARKDOWN RICH DIFF", "Markdown diff"),
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

    print(
        "REV_PTY_SMOKE_OK: tree-live-preview enter-and-t-return "
        "q-questions browser-rich-diff command-only-quit"
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (AssertionError, OSError) as error:
        print(f"REV_PTY_SMOKE_FAILED: {error}", file=sys.stderr)
        raise SystemExit(1)
