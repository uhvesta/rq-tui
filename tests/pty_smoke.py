#!/usr/bin/env python3
"""Small, dependency-free PTY regression test for the compiled rq-tui binary."""

from __future__ import annotations

import errno
import fcntl
import os
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
from pathlib import Path


ANSI_RE = re.compile(
    rb"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\a]*(?:\a|\x1b\\))"
)


def git(repo: Path, *args: str) -> None:
    subprocess.run(
        ["git", *args],
        cwd=repo,
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )


def make_fixture(root: Path) -> Path:
    repo = root / "pty-fixture"
    repo.mkdir()
    git(repo, "init", "-b", "main")
    git(repo, "config", "user.email", "rq-tui-pty@example.invalid")
    git(repo, "config", "user.name", "rq-tui PTY test")
    (repo / "src").mkdir()
    (repo / "src" / "demo.rs").write_text(
        "fn main() {\n    println!(\"base\");\n}\n", encoding="utf-8"
    )
    git(repo, "add", ".")
    git(repo, "commit", "-m", "base")
    (repo / "src" / "demo.rs").write_text(
        "fn main() {\n    println!(\"pty smoke\");\n    println!(\"resize me\");\n}\n",
        encoding="utf-8",
    )
    return repo


def set_size(master: int, columns: int, rows: int) -> None:
    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))


def terminal_text(data: bytes) -> str:
    # Keep the terminal stream intact for the alternate-screen assertion while
    # providing a readable form for semantic markers.
    data = ANSI_RE.sub(b"", data)
    data = data.replace(b"\r", b"\n")
    return data.decode("utf-8", errors="replace")


class Child:
    def __init__(self, binary: Path, repo: Path, data_dir: Path) -> None:
        self.pid, self.master = pty.fork()
        if self.pid == 0:
            os.chdir(repo)
            env = os.environ.copy()
            env.update(
                {
                    "RQ_TUI_CONTROLLED_AGENT": "1",
                    "RQ_TUI_DATA_DIR": str(data_dir / "data"),
                    "RQ_TUI_CACHE_DIR": str(data_dir / "cache"),
                    "RQ_TUI_DATABASE": str(data_dir / "data" / "review.db"),
                    "TERM": "xterm-256color",
                }
            )
            os.execve(
                str(binary),
                [str(binary), "review", str(repo), "--base", "main"],
                env,
            )
            raise AssertionError("execve returned")
        self.output = bytearray()
        self.status: int | None = None

    def poll(self) -> int | None:
        if self.status is not None:
            return self.status
        try:
            waited, status = os.waitpid(self.pid, os.WNOHANG)
        except ChildProcessError:
            return self.status
        if waited == 0:
            return None
        if os.WIFEXITED(status):
            self.status = os.WEXITSTATUS(status)
        elif os.WIFSIGNALED(status):
            self.status = 128 + os.WTERMSIG(status)
        else:
            self.status = 1
        return self.status

    def read(self, timeout: float = 0.1) -> None:
        ready, _, _ = select.select([self.master], [], [], timeout)
        if not ready:
            return
        try:
            self.output.extend(os.read(self.master, 65536))
        except OSError as error:
            if error.errno != errno.EIO:
                raise

    def send(self, value: bytes) -> None:
        os.write(self.master, value)

    def wait_for(self, marker: str, timeout: float = 8.0) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.read(0.1)
            if marker in terminal_text(bytes(self.output)):
                return
            status = self.poll()
            if status is not None:
                raise AssertionError(self.failure(f"process exited {status} before {marker!r}"))
        raise AssertionError(self.failure(f"timed out waiting for {marker!r}"))

    def wait_for_exit(self, timeout: float = 8.0) -> int:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.read(0.1)
            status = self.poll()
            if status is not None:
                self.read(0.1)
                return status
        raise AssertionError(self.failure("process did not exit after :q"))

    def failure(self, reason: str) -> str:
        text = terminal_text(bytes(self.output))
        return f"{reason}\n--- terminal tail ---\n{text[-6000:]}"

    def close(self) -> None:
        status = self.poll()
        if status is None:
            try:
                os.killpg(self.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                os.waitpid(self.pid, 0)
            except ChildProcessError:
                pass
        try:
            os.close(self.master)
        except OSError:
            pass


def main() -> int:
    if sys.platform not in ("darwin", "linux"):
        raise AssertionError(f"unsupported PTY platform: {sys.platform}")

    binary = Path(sys.argv[1]).resolve()
    with tempfile.TemporaryDirectory(prefix="rq-tui-pty-") as temporary:
        root = Path(temporary)
        repo = make_fixture(root)
        child = Child(binary, repo, root / "app")
        try:
            set_size(child.master, 100, 24)
            child.wait_for("Review", timeout=8)
            child.wait_for("COPILOT MAIN", timeout=8)
            if child.poll() is not None:
                raise AssertionError(child.failure("TUI exited during entry"))

            # Resize the real terminal and require a post-resize redraw. The
            # ioctl is deliberately performed on the PTY master, so this also
            # exercises the child terminal's SIGWINCH path.
            set_size(child.master, 72, 18)
            size = struct.unpack("HHHH", fcntl.ioctl(child.master, termios.TIOCGWINSZ, bytes(8)))
            if size[:2] != (18, 72):
                raise AssertionError(f"PTY resize was not applied: {size[:2]}")
            before_resize = len(child.output)
            child.read(0.5)
            if len(child.output) <= before_resize:
                raise AssertionError(child.failure("no redraw observed after PTY resize"))

            # Navigation is a real crossterm key event, not a headless reducer
            # call. The source text remains present after the smaller layout.
            child.send(b"j")
            child.send(b"v")
            child.wait_for("rows 2-2", timeout=4)
            child.send(b"\x1b")
            child.wait_for("NORMAL", timeout=4)

            # ':' must visibly enter command mode before the quit command is
            # submitted; this catches input routing regressions as well as exit.
            child.send(b":")
            child.wait_for("COMMAND MODE", timeout=4)
            child.send(b"q\r")
            status = child.wait_for_exit(timeout=8)
            if status != 0:
                raise AssertionError(child.failure(f"rq-tui exited with status {status}"))

            raw = bytes(child.output)
            text = terminal_text(raw)
            if "panicked at" in text or "thread 'main' panicked" in text:
                raise AssertionError(child.failure("panic text appeared in terminal output"))
            if b"\x1b[?1049l" not in raw:
                raise AssertionError(child.failure("alternate screen was not restored on exit"))
        finally:
            child.close()

    print("PTY_SMOKE_OK: entry resize key command-mode clean-exit no-panic")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (AssertionError, OSError) as error:
        print(f"PTY_SMOKE_FAILED: {error}", file=sys.stderr)
        raise SystemExit(1)
