#!/usr/bin/env python3
"""Small, dependency-free PTY regression test for the compiled rq-tui binary."""

from __future__ import annotations

import base64
import errno
import fcntl
import os
import pty
import re
import select
import signal
import sqlite3
import struct
import subprocess
import sys
import tempfile
import termios
import time
import unicodedata
from pathlib import Path


ANSI_RE = re.compile(
    rb"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\a]*(?:\a|\x1b\\))"
)
OSC52_RE = re.compile(rb"\x1b\]52;c;([A-Za-z0-9+/=]*)\x07")
CHAT_ROWS_RE = re.compile(r"rows (\d+)-(\d+)/(\d+)")

# A semantic deadline for the compiled PTY regression below. This is not a
# sleep: once the initial frame is visible, a large navigation burst must
# acknowledge a new input mode within this bound.
NAVIGATION_BURST = b"j" * 2048 + b"k" * 2048
BURST_RESPONSE_TIMEOUT = 2.0


def git(repo: Path, *args: str) -> None:
    subprocess.run(
        ["git", *args],
        cwd=repo,
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )


def make_fixture(
    root: Path, name: str = "pty-fixture", extra_changed_lines: int = 0
) -> Path:
    repo = root / name
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
    generated = "".join(
        f"    let generated_{index} = {index};\n"
        for index in range(extra_changed_lines)
    )
    (repo / "src" / "demo.rs").write_text(
        "fn main() {\n"
        '    println!("pty smoke");\n'
        '    println!("resize me");\n'
        f"{generated}"
        "}\n",
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


def character_width(character: str) -> int:
    if unicodedata.combining(character) or character in ("\u200d", "\ufe0f"):
        return 0
    return 2 if unicodedata.east_asian_width(character) in ("W", "F") else 1


class TerminalScreen:
    """Minimal ANSI screen model for Crossterm/Ratatui differential frames."""

    def __init__(self, columns: int = 80, rows: int = 24) -> None:
        self.columns = columns
        self.rows = rows
        self.cells = [[" " for _ in range(columns)] for _ in range(rows)]
        self.row = 0
        self.column = 0
        self.saved = (0, 0)
        self.pending = bytearray()

    def resize(self, columns: int, rows: int) -> None:
        resized = [[" " for _ in range(columns)] for _ in range(rows)]
        for row in range(min(self.rows, rows)):
            for column in range(min(self.columns, columns)):
                resized[row][column] = self.cells[row][column]
        self.columns = columns
        self.rows = rows
        self.cells = resized
        self.row = min(self.row, rows - 1)
        self.column = min(self.column, columns - 1)
        self.saved = (
            min(self.saved[0], rows - 1),
            min(self.saved[1], columns - 1),
        )

    def clear(self) -> None:
        self.cells = [[" " for _ in range(self.columns)] for _ in range(self.rows)]
        self.row = 0
        self.column = 0

    def text(self) -> str:
        return "\n".join("".join(row).rstrip() for row in self.cells)

    @staticmethod
    def _parameters(raw: bytes) -> list[int]:
        text = raw.decode("ascii", errors="ignore").lstrip("?<>")
        result = []
        for value in text.split(";"):
            try:
                result.append(int(value) if value else 0)
            except ValueError:
                result.append(0)
        return result or [0]

    def _csi(self, parameters: bytes, final: int) -> None:
        values = self._parameters(parameters)
        command = chr(final)
        amount = values[0] or 1
        if command in ("H", "f"):
            self.row = min(self.rows - 1, max(0, (values[0] or 1) - 1))
            self.column = min(
                self.columns - 1,
                max(0, (values[1] if len(values) > 1 else 1) - 1),
            )
        elif command == "A":
            self.row = max(0, self.row - amount)
        elif command == "B":
            self.row = min(self.rows - 1, self.row + amount)
        elif command == "C":
            self.column = min(self.columns - 1, self.column + amount)
        elif command == "D":
            self.column = max(0, self.column - amount)
        elif command == "G":
            self.column = min(self.columns - 1, max(0, amount - 1))
        elif command == "d":
            self.row = min(self.rows - 1, max(0, amount - 1))
        elif command == "J":
            if values[0] in (2, 3):
                self.clear()
            elif values[0] == 0:
                for column in range(self.column, self.columns):
                    self.cells[self.row][column] = " "
                for row in range(self.row + 1, self.rows):
                    self.cells[row] = [" " for _ in range(self.columns)]
        elif command == "K":
            if values[0] == 1:
                for column in range(0, self.column + 1):
                    self.cells[self.row][column] = " "
            elif values[0] == 2:
                self.cells[self.row] = [" " for _ in range(self.columns)]
            else:
                for column in range(self.column, self.columns):
                    self.cells[self.row][column] = " "
        elif command == "s":
            self.saved = (self.row, self.column)
        elif command == "u":
            self.row, self.column = self.saved
            self.row = min(self.row, self.rows - 1)
            self.column = min(self.column, self.columns - 1)
        elif command == "h" and parameters.startswith(b"?1049"):
            self.clear()

    def _write(self, character: str) -> None:
        width = character_width(character)
        if width == 0:
            if self.column > 0:
                self.cells[self.row][self.column - 1] += character
            return
        if self.column >= self.columns:
            self.column = 0
            self.row = min(self.rows - 1, self.row + 1)
        self.cells[self.row][self.column] = character
        if width == 2 and self.column + 1 < self.columns:
            self.cells[self.row][self.column + 1] = ""
        self.column += width

    def feed(self, data: bytes) -> None:
        if self.pending:
            data = bytes(self.pending) + data
            self.pending.clear()
        index = 0
        while index < len(data):
            byte = data[index]
            if byte == 0x1B:
                if index + 1 >= len(data):
                    self.pending.extend(data[index:])
                    return
                kind = data[index + 1]
                if kind == ord("["):
                    end = index + 2
                    while end < len(data) and not 0x40 <= data[end] <= 0x7E:
                        end += 1
                    if end >= len(data):
                        self.pending.extend(data[index:])
                        return
                    self._csi(data[index + 2 : end], data[end])
                    index = end + 1
                    continue
                if kind == ord("]"):
                    end = index + 2
                    terminated = False
                    while end < len(data):
                        if data[end] == 0x07:
                            end += 1
                            terminated = True
                            break
                        if data[end : end + 2] == b"\x1b\\":
                            end += 2
                            terminated = True
                            break
                        end += 1
                    if not terminated:
                        self.pending.extend(data[index:])
                        return
                    index = end
                    continue
                if kind == ord("7"):
                    self.saved = (self.row, self.column)
                elif kind == ord("8"):
                    self.row, self.column = self.saved
                    self.row = min(self.row, self.rows - 1)
                    self.column = min(self.column, self.columns - 1)
                index += 2
                continue
            if byte == 0x0D:
                self.column = 0
                index += 1
                continue
            if byte == 0x0A:
                self.row = min(self.rows - 1, self.row + 1)
                index += 1
                continue
            if byte == 0x08:
                self.column = max(0, self.column - 1)
                index += 1
                continue
            if byte < 0x20 or byte == 0x7F:
                index += 1
                continue
            length = 1
            if byte & 0xE0 == 0xC0:
                length = 2
            elif byte & 0xF0 == 0xE0:
                length = 3
            elif byte & 0xF8 == 0xF0:
                length = 4
            if index + length > len(data):
                self.pending.extend(data[index:])
                return
            character = data[index : index + length].decode("utf-8", errors="replace")
            self._write(character)
            index += length


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
                    "RQ_TUI_CLIPBOARD": "osc52",
                    "RQ_TUI_CONTROLLED_AUDIT_EVENTS": "1",
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
        self.screen = TerminalScreen()

    def resize(self, columns: int, rows: int) -> None:
        set_size(self.master, columns, rows)
        self.screen.resize(columns, rows)

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
            chunk = os.read(self.master, 65536)
            self.output.extend(chunk)
            self.screen.feed(chunk)
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

    def wait_for_since(self, marker: str, offset: int, timeout: float = 8.0) -> None:
        self.wait_for_since_within(marker, offset, timeout)

    def wait_for_since_within(
        self, marker: str, offset: int, timeout: float = 8.0
    ) -> float:
        started = time.monotonic()
        deadline = started + timeout
        while time.monotonic() < deadline:
            self.read(0.1)
            if marker in terminal_text(bytes(self.output[offset:])):
                return time.monotonic() - started
            status = self.poll()
            if status is not None:
                raise AssertionError(self.failure(f"process exited {status} before {marker!r}"))
        raise AssertionError(self.failure(f"timed out waiting for new {marker!r}"))

    def wait_for_screen(self, marker: str, timeout: float = 8.0) -> None:
        self.wait_for_screen_within(marker, timeout)

    def wait_for_screen_within(
        self, marker: str, timeout: float = 8.0
    ) -> float:
        started = time.monotonic()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.read(0.1)
            if marker in self.screen.text():
                return time.monotonic() - started
            status = self.poll()
            if status is not None:
                raise AssertionError(
                    self.failure(f"process exited {status} before screen marker {marker!r}")
                )
        raise AssertionError(self.failure(f"timed out waiting for screen marker {marker!r}"))

    def press_until_screen(
        self, marker: str, key: bytes = b"j", max_steps: int = 32
    ) -> None:
        for _ in range(max_steps + 1):
            self.read(0.1)
            if marker in self.screen.text():
                return
            self.send(key)
        raise AssertionError(
            self.failure(f"screen marker {marker!r} was not reachable by scrolling")
        )

    def wait_for_chat_rows(
        self,
        predicate=lambda _rows: True,
        timeout: float = 8.0,
    ) -> tuple[int, int, int]:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.read(0.1)
            matches = CHAT_ROWS_RE.findall(self.screen.text())
            if matches:
                rows = tuple(int(value) for value in matches[-1])
                if predicate(rows):
                    return rows
            status = self.poll()
            if status is not None:
                raise AssertionError(
                    self.failure(f"process exited {status} before a Chat row redraw")
                )
        raise AssertionError(self.failure("timed out waiting for a matching Chat row redraw"))

    def assert_screen_stays(
        self,
        required: tuple[str, ...],
        forbidden: tuple[str, ...],
        duration: float,
    ) -> None:
        deadline = time.monotonic() + duration
        while time.monotonic() < deadline:
            self.read(0.1)
            text = self.screen.text()
            missing = tuple(marker for marker in required if marker not in text)
            present = tuple(marker for marker in forbidden if marker in text)
            if missing or present:
                raise AssertionError(
                    self.failure(
                        f"screen did not stay quiescent; missing={missing!r}, "
                        f"forbidden={present!r}"
                    )
                )
            status = self.poll()
            if status is not None:
                raise AssertionError(
                    self.failure(f"process exited {status} during quiescence check")
                )

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
        return (
            f"{reason}\n--- current screen ---\n{self.screen.text()}"
            f"\n--- terminal tail ---\n{text[-6000:]}"
        )

    def close(self) -> None:
        status = self.poll()
        if status is None:
            try:
                os.kill(self.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            deadline = time.monotonic() + 1.0
            while self.poll() is None and time.monotonic() < deadline:
                time.sleep(0.01)
            if self.poll() is None:
                try:
                    os.kill(self.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                deadline = time.monotonic() + 1.0
                while self.poll() is None and time.monotonic() < deadline:
                    time.sleep(0.01)
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
        app_root = root / "app"
        old_repo = make_fixture(root, "old-fixture")
        setup = Child(binary, old_repo, app_root)
        try:
            setup.resize(80, 18)
            setup.wait_for(
                "Inspecting local repositories and computing diffs", timeout=8
            )
            setup.wait_for("Ctrl-C cancels", timeout=8)
            setup.wait_for("entering TUI", timeout=8)
            setup.wait_for("Review", timeout=8)
            setup.wait_for("COPILOT MAIN", timeout=8)
            setup.send(b"\t")
            setup.wait_for_screen("Chat", timeout=4)
            setup.send(b"i")
            setup.wait_for_screen("INSERT", timeout=4)
            setup.send(
                b"\x1b[200~"
                b"PASTE_FIRST_LINE\nPASTE_SECOND_LINE"
                b"\x1b[201~"
            )
            setup.wait_for_screen("PASTE_FIRST_LINE", timeout=4)
            setup.wait_for_screen("PASTE_SECOND_LINE", timeout=4)
            setup.wait_for_screen("Pasted 34 bytes across 2 lines", timeout=4)
            setup.send(b"\x03")
            setup.wait_for_screen("Type a message", timeout=4)
            setup.send(b"\t")
            setup.wait_for_screen("Review", timeout=4)
            setup.send(b":q\r")
            if setup.wait_for_exit(timeout=8) != 0:
                raise AssertionError(setup.failure("setup review did not exit cleanly"))
        finally:
            setup.close()

        # A repository-scale source file guards the real input-latency path.
        # Rendering must syntax-highlight only the viewport; a full-diff pass
        # here starves Crossterm input for many seconds.
        repo = make_fixture(root, extra_changed_lines=12_000)
        child = Child(binary, repo, app_root)
        burst_latency = 0.0
        try:
            child.resize(100, 24)
            child.wait_for(
                "Inspecting local repositories and computing diffs", timeout=8
            )
            child.wait_for("entering TUI", timeout=8)
            child.wait_for("Review", timeout=8)
            child.wait_for("COPILOT MAIN", timeout=8)
            if child.poll() is not None:
                raise AssertionError(child.failure("TUI exited during entry"))

            # PTY-29: a held j/k burst must not leave the user waiting behind
            # thousands of queued navigation events. The trailing ':' is an
            # unambiguous, actionable acknowledgement: it proves the input
            # loop reached a new mode after the burst, not merely that the
            # terminal kept repainting the old review screen.
            child.send(NAVIGATION_BURST + b":")
            burst_latency = child.wait_for_screen_within(
                "COMMAND MODE · Command palette", BURST_RESPONSE_TIMEOUT
            )
            child.send(b"\x1b")
            child.wait_for_screen("NORMAL", timeout=4)

            # PTY-31: the key that changes mode and the text following it can
            # arrive in one terminal read. Process that burst online so the
            # composer receives every character instead of dropping the text
            # against the previous Normal-mode snapshot.
            child.send(b"aIMMEDIATE_INPUT")
            child.wait_for_screen("IMMEDIATE_INPUT", timeout=2)
            child.send(b"\x03")
            child.wait_for_screen("NORMAL", timeout=4)

            # PTY-30: the spec-required Ctrl-W focus chord must acknowledge
            # its pending state and then move between the file tree and diff.
            child.send(b"\x17")
            child.wait_for_screen("CTRL-W", timeout=4)
            child.send(b"h")
            child.wait_for_screen("Focus: files", timeout=4)
            child.send(b"\x17l")
            child.wait_for_screen("Focus: diff", timeout=4)

            # Resize the real terminal and require a post-resize redraw. The
            # ioctl is deliberately performed on the PTY master, so this also
            # exercises the child terminal's SIGWINCH path.
            child.resize(72, 18)
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
            child.wait_for_screen("VISUAL CHAR", timeout=4)
            child.send(b"\x1b")
            child.wait_for_screen("NORMAL", timeout=4)
            child.send(b"\x16")
            child.wait_for_screen("VISUAL BLOCK", timeout=4)
            child.send(b"\x1b")
            child.wait_for_screen("NORMAL", timeout=4)

            # The compiled TUI's prune screen must visibly disable the open
            # Work Item and retain the live Copilot progress surface.
            child.send(b":prune\r")
            child.wait_for("OPEN(disabled)", timeout=4)
            if "COPILOT MAIN" not in terminal_text(bytes(child.output)):
                raise AssertionError(
                    child.failure("prune screen hid the Copilot progress surface")
                )
            child.send(b" ")
            child.send(b"d")
            child.wait_for("Prune complete", timeout=8)
            before_return = len(child.output)
            child.send(b"q")
            child.wait_for_since("NORMAL", before_return, timeout=4)

            # Exercise the compiled Chat bottom stack at the supported minimum:
            # the long composer must remain bordered, COMMAND mode must keep a
            # sticky input plus local completions, and Esc must restore the
            # preserved draft.
            child.send(b"\t")
            child.wait_for("Chat", timeout=4)

            # PTY-24/25: keep accepting input while the controlled agent is
            # active, expose the background FIFO plus typed activity, and
            # prove that the compiled transcript scrolls by rendered rows.
            first_prompt = (
                b"First alpha beta gamma delta epsilon zeta eta theta iota kappa "
                b"lambda mu nu xi omicron pi rho sigma tau upsilon phi chi psi omega "
                b"repeated context keeps this first question tall enough for several "
                b"terminal rows while Copilot inspects the selected source."
            )
            second_prompt = (
                b"Second queued question remains editable and cancellable while the "
                b"first controlled response is active; this deliberately wraps across "
                b"many more rendered rows so page and mouse scrolling have room to move."
            )
            child.send(b"i" + first_prompt + b"\r")
            child.wait_for_screen("Using read_file", timeout=4)
            child.wait_for_screen("Finished read_file", timeout=4)
            child.send(b"i" + second_prompt + b"\r")
            child.wait_for_screen("queued", timeout=4)

            child.send(b":queue\r")
            child.wait_for_screen("Copilot queue", timeout=4)
            child.wait_for_screen("2 pending", timeout=4)
            child.wait_for_screen("ACTIVE", timeout=4)
            child.wait_for_screen("QUEUED", timeout=4)
            child.send(b"q")
            child.wait_for_screen("Chat", timeout=4)

            child.send(b":agent-status\r")
            child.wait_for_screen("COPILOT SDK LIVENESS", timeout=4)
            child.wait_for_screen("outbound id:", timeout=4)
            child.press_until_screen("Running review skill: exhaustive audit")
            child.press_until_screen("Subagent inspecting terminal edge cases")
            child.press_until_screen("Retrying transient controlled operation")
            child.wait_for_screen("q/Esc return", timeout=4)
            child.send(b"q")
            child.wait_for_screen("Chat", timeout=4)

            child.resize(44, 14)
            narrow_bottom = child.wait_for_chat_rows(
                lambda rows: rows[2] > 12, timeout=4
            )
            child.send(b"\x1b[A")
            arrow_up = child.wait_for_chat_rows(
                lambda rows: rows[0] < narrow_bottom[0],
                timeout=4,
            )
            child.send(b"\x1b[5~")
            page_up = child.wait_for_chat_rows(
                lambda rows: rows[0] < arrow_up[0],
                timeout=4,
            )
            child.send(b"\x1b[<65;10;5M")
            mouse_down = child.wait_for_chat_rows(
                lambda rows: rows[0] > page_up[0],
                timeout=4,
            )
            child.send(b"\x1b[<64;10;5M")
            child.wait_for_chat_rows(
                lambda rows: rows[0] < mouse_down[0],
                timeout=4,
            )

            # Select five exact source characters from the first message and
            # force OSC 52 so the PTY can decode and verify the payload.
            child.send(b"ggvllll")
            child.wait_for_screen("VISUAL CHAR", timeout=4)
            before_yank = len(child.output)
            child.send(b"y")
            child.wait_for_screen("Sent 5 bytes via OSC 52", timeout=4)
            payloads = OSC52_RE.findall(bytes(child.output[before_yank:]))
            if not payloads:
                raise AssertionError(child.failure("semantic yank emitted no OSC 52 payload"))
            copied = base64.b64decode(payloads[-1], validate=True)
            if copied != b"First":
                raise AssertionError(f"unexpected semantic yank payload: {copied!r}")

            # Line mode is inclusive and copies the exact semantic source row
            # with one hard newline, never the Chat border or soft-wrap cells.
            before_line_yank = len(child.output)
            child.send(b"ggVy")
            child.wait_for_screen("Sent 39 bytes via OSC 52", timeout=4)
            line_payloads = OSC52_RE.findall(bytes(child.output[before_line_yank:]))
            if not line_payloads:
                raise AssertionError(child.failure("line yank emitted no OSC 52 payload"))
            line_copy = base64.b64decode(line_payloads[-1], validate=True)
            expected_line = b"First alpha beta gamma delta epsilon z\n"
            if line_copy != expected_line:
                raise AssertionError(f"unexpected semantic line yank: {line_copy!r}")

            child.send(b"G")
            child.wait_for_chat_rows(
                lambda rows: rows[1] == rows[2],
                timeout=4,
            )
            child.resize(100, 24)
            child.wait_for_chat_rows(
                lambda rows: rows[2] < narrow_bottom[2],
                timeout=4,
            )

            # Exercise selection painting after a large streamed delta, then
            # prove G lands on visible latest source rather than only a
            # trailing row number.
            child.wait_for_screen("audit-token-349", timeout=8)
            before_long_yank = len(child.output)
            child.send(b"ggvG")
            child.wait_for_screen("VISUAL CHAR", timeout=4)
            child.send(b"y")
            child.wait_for_screen("via OSC 52", timeout=4)
            long_payloads = OSC52_RE.findall(bytes(child.output[before_long_yank:]))
            if not long_payloads:
                raise AssertionError(child.failure("long Visual yank emitted no OSC 52 payload"))
            long_copy = base64.b64decode(long_payloads[-1], validate=True)
            if not long_copy.startswith(b"you: First ") or b"audit-token-349" not in long_copy:
                raise AssertionError(
                    f"long Visual yank missed semantic endpoints: "
                    f"{long_copy[:32]!r} ... {long_copy[-64:]!r}"
                )
            child.send(b"G")
            child.wait_for_chat_rows(lambda rows: rows[1] == rows[2], timeout=4)
            child.wait_for_screen("audit-token-349", timeout=4)

            # No events after the large delta should become visibly quiet
            # rather than looking frozen. Ctrl-C then stops only the active
            # response; the waiting prompt is then promoted immediately and
            # remains independently stoppable.
            child.wait_for_screen("quiet for", timeout=8)
            child.send(b"\x03")
            child.wait_for_screen("STOPPING", timeout=4)
            child.wait_for_screen("response cancelled", timeout=4)
            child.send(b":queue\r")
            child.wait_for_screen("1 pending", timeout=4)
            child.wait_for_screen("ACTIVE", timeout=4)
            child.send(b"s")
            child.wait_for_screen("No active or queued questions", timeout=4)
            child.send(b"q")
            child.wait_for_screen("Chat", timeout=4)
            child.assert_screen_stays(
                required=("response cancelled",),
                forbidden=(
                    "◐ streaming",
                    " · active",
                    " · queued",
                    "CONNECTING",
                    "QUEUED",
                    "RESPONDING",
                    "STOPPING",
                    "THINKING",
                    "TOOL",
                ),
                duration=4,
            )

            # PTY-28: exercise the compiled ephemeral SIDE lifecycle. The
            # SIDE prompt starts on its isolated lane; /main restores the
            # persistent transcript immediately while abort/cleanup finishes.
            child.resize(80, 18)
            child.wait_for_screen("Type a message", timeout=4)
            child.send(b"i")
            child.wait_for_screen("INSERT", timeout=4)
            child.send(b"/side inspect the cleanup path in isolation\r")
            child.wait_for_screen("COPILOT SIDE", timeout=4)
            child.wait_for_screen("you · SIDE", timeout=4)
            child.wait_for_screen("Using read_file", timeout=6)
            child.send(b"i")
            child.wait_for_screen("INSERT", timeout=4)
            child.send(b"/main\r")
            child.wait_for_screen("COPILOT MAIN", timeout=4)
            child.wait_for_screen("SIDE cleanup complete", timeout=6)

            child.resize(40, 9)
            child.wait_for_chat_rows(lambda rows: rows[2] > 100, timeout=8)
            child.send(
                b"i"
                b"a long compiled PTY draft that wraps and scrolls without clipping"
            )
            child.wait_for_screen("INSERT", timeout=8)
            child.wait_for_screen("Enter send", timeout=8)
            child.wait_for_screen("Esc keep", timeout=8)
            before_keep = len(child.output)
            child.send(b"\x1b")
            child.wait_for_since("NORMAL", before_keep, timeout=4)
            child.read(0.2)
            before_command = len(child.output)
            child.send(b":")
            child.wait_for_since("COMMAND COMPLETIONS", before_command, timeout=4)
            child.wait_for_since("COMMAND MODE ACTIVE", before_command, timeout=4)
            child.wait_for_since("COPILOT MAIN", before_command, timeout=4)
            child.wait_for_since("B held", before_command, timeout=4)
            before_close = len(child.output)
            child.send(b"\x1b")
            child.wait_for_since("NORMAL", before_close, timeout=4)
            before_discard = len(child.output)
            child.send(b"\x03")
            child.wait_for_since("Type a message", before_discard, timeout=4)
            child.resize(72, 18)
            child.read(0.3)

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
            with sqlite3.connect(app_root / "data" / "review.db") as database:
                work_items = database.execute(
                    "SELECT name FROM work_items ORDER BY name"
                ).fetchall()
            if work_items != [("pty-fixture",)]:
                raise AssertionError(f"unexpected Work Items after prune: {work_items!r}")
        finally:
            child.close()

    print(
        "PTY_SMOKE_OK: entry resize prune-progress chat-composer "
        f"held-key-burst={burst_latency:.3f}s ctrl-w clean-exit no-panic"
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (AssertionError, OSError) as error:
        print(f"PTY_SMOKE_FAILED: {error}", file=sys.stderr)
        raise SystemExit(1)
