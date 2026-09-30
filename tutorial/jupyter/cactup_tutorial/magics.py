"""The tutorial's cell magics: `%%shell`, `%%file` and `%show`."""

from __future__ import annotations

import atexit
import os
import shlex
import time
from pathlib import Path

from IPython.core.error import UsageError
from IPython.core.magic import Magics, cell_magic, line_cell_magic, line_magic, magics_class
from IPython.core.magic_arguments import argument, magic_arguments, parse_argstring
from IPython.display import display

from . import highlight
from .render import Terminal
from .session import Result, Session

# How often a running cell's display is redrawn. Fast enough that a progress
# bar visibly moves, slow enough not to flood the front end with updates. A
# long output costs more to render and send each time, so the interval grows
# with it (to a few times the time a redraw takes).
_REFRESH = 0.1
_REFRESH_COST_FACTOR = 5
# After this long without output, an unfinished last line is probably a
# prompt the cell can't answer without --stdin.
_PROMPT_HINT_AFTER = 5.0
_PROMPT_HINT = (
    "waiting for input? Stop it with the kernel's stop button (■, or I, I), and give answers with --stdin"
)

_STYLE = """
<style>
.jp-RenderedText pre.cactup-term { margin: 0; padding: 0; line-height: 1.3;
  font-family: var(--jp-code-font-family); font-size: var(--jp-code-font-size);
  background: transparent; white-space: pre; overflow-x: auto; }
/* Long output: a box of its own height, starting scrolled to its end (a
   column-reverse flex box keeps its scroll position at the bottom). */
.jp-RenderedText.cactup-box { max-height: 36em; overflow-y: auto; display: flex;
  flex-direction: column-reverse; border: 1px solid var(--jp-border-color2);
  padding: 0.2em 0.4em; }
/* The terminal keeps its full height inside the box, so the box (not the
   terminal) is what scrolls. */
.jp-RenderedText.cactup-box > pre.cactup-term { flex: none; }
/* Colors a theme can't show: white on white, bright black on black. */
body[data-jp-theme-light='true'] .cactup-term .ansi-white-fg,
body[data-jp-theme-light='true'] .cactup-term .ansi-white-intense-fg { color: var(--jp-content-font-color0); }
body[data-jp-theme-light='true'] .cactup-term .ansi-cyan-fg,
body[data-jp-theme-light='true'] .cactup-term .ansi-cyan-intense-fg { color: #00797f; }
body[data-jp-theme-light='true'] .cactup-term .ansi-yellow-fg,
body[data-jp-theme-light='true'] .cactup-term .ansi-yellow-intense-fg { color: #8a6100; }
body[data-jp-theme-light='false'] .cactup-term .ansi-red-intense-fg { color: #ff6b6b; }
body[data-jp-theme-light='false'] .cactup-term .ansi-green-intense-fg { color: #5fd068; }
body[data-jp-theme-light='false'] .cactup-term .ansi-black-fg,
body[data-jp-theme-light='false'] .cactup-term .ansi-black-intense-fg { color: #9e9e9e; }
.cactup-status { margin-top: 0.3em; font-family: var(--jp-ui-font-family);
  font-size: var(--jp-ui-font-size0); color: var(--jp-ui-font-color2); }
.cactup-status.cactup-fail { color: var(--jp-error-color1); }
.cactup-status.cactup-hint { color: var(--jp-warn-color1); }
</style>
"""


class CellFailed(Exception):
    """Raised in strict mode when a cell's outcome was not the expected one.

    The tutorial's headless runner sets `CACTUP_TUTORIAL_STRICT=1`, so a
    command that fails unexpectedly (or a cell marked `--expect-fail` that
    succeeds) stops the run instead of being quietly recorded.
    """

    def _render_traceback_(self):
        return [f"CellFailed: {self}"]


@magics_class
class TutorialMagics(Magics):
    def __init__(self, shell):
        super().__init__(shell)
        self._session: Session | None = None

    @property
    def session(self) -> Session:
        if self._session is None:
            # Start where the kernel started: the notebook's own directory.
            self._session = Session(cwd=os.getcwd())
            atexit.register(self._session.close)
        return self._session

    # -- %%shell -------------------------------------------------------

    @magic_arguments()
    @argument("--timeout", type=float, default=None, help="Interrupt the command after this many seconds.")
    @argument(
        "--stdin",
        default=None,
        help="Text to type into the terminal once the command starts (answers to prompts: "
        "if the cell is interrupted before reading it, the shell runs it as a command).",
    )
    @argument("--expect-fail", action="store_true", help="This cell is supposed to fail.")
    @argument("--quiet", action="store_true", help="Don't print the exit status line.")
    @line_cell_magic("shell")
    def shell_magic(self, line: str, cell: str | None = None):
        """Run the cell in the tutorial's terminal.

        The cell runs in a bash session that persists across cells (the
        working directory and exported variables carry over) on a real
        terminal, so progress bars and colors show up as they would in one.
        Interrupting the kernel sends Ctrl-C to the running command.
        """
        if cell is None:  # line form: %shell <command>
            args = parse_argstring(self.shell_magic, "")
            script = line
        else:
            args = parse_argstring(self.shell_magic, line)
            script = cell
        stdin = _unescape(args.stdin) if args.stdin else None

        term = Terminal()
        handle = display(_bundle(term, None), raw=True, display_id=True)
        # `tick` makes the session yield while the command is quiet too, so
        # output that came just before a pause (a prompt) is drawn promptly.
        items = self.session.run(script, timeout=args.timeout, stdin=stdin, tick=True)
        try:
            result = self._consume(items, term, handle)
        finally:
            # Ends the session's hold on the kernel's interrupt handler now,
            # not whenever this frame (kept by a traceback, say) goes away.
            items.close()
        handle.update(_bundle(term, None if args.quiet else result), raw=True)

        # Python cells should see the same working directory as the terminal.
        if result.cwd and os.path.isdir(result.cwd):
            os.chdir(result.cwd)
        self._check(result, args.expect_fail)

    def _consume(self, items, term: Terminal, handle) -> Result:
        last = 0.0
        interval = _REFRESH
        dirty = False
        last_output = time.monotonic()
        hinting = False
        result: Result | None = None
        while result is None:
            try:
                item = next(items)
                if isinstance(item, Result):
                    result = item
                    break
                now = time.monotonic()
                if item:
                    term.feed(item)
                    dirty = True
                    last_output = now
                    hinting = False
                elif not hinting and now - last_output > _PROMPT_HINT_AFTER and term.mid_line():
                    hinting = dirty = True
                if dirty and now - last >= interval:
                    handle.update(_bundle(term, None, hint=_PROMPT_HINT if hinting else None), raw=True)
                    last = time.monotonic()
                    interval = max(_REFRESH, _REFRESH_COST_FACTOR * (last - now))
                    dirty = False
            except KeyboardInterrupt:
                # The interrupt landed here rather than inside the reader;
                # forward it all the same and keep reading.
                self.session.interrupt()
            except StopIteration:
                # The session ended the cell without a result (it should not):
                # start the next cell in a fresh shell rather than show a
                # traceback.
                self.session.close()
                result = Result(status=-1, cwd=self.session.cwd, interrupted=True, timed_out=False, exited=True)
        return result

    def _check(self, result: Result, expect_fail: bool) -> None:
        if os.environ.get("CACTUP_TUTORIAL_STRICT") != "1":
            return
        ok = result.status == 0 and not result.interrupted
        if expect_fail and ok:
            raise CellFailed("the cell was expected to fail, but it succeeded")
        if not expect_fail and not ok:
            raise CellFailed(f"the command exited with status {result.status}")

    # -- %%file --------------------------------------------------------

    @magic_arguments()
    @argument("path", help="File to write; relative paths are relative to the terminal's directory.")
    @argument("--append", "-a", action="store_true", help="Append instead of overwriting.")
    @cell_magic("file")
    def file_magic(self, line: str, cell: str):
        """Write the cell's contents to a file.

        The editor highlights the cell in the file's language, taken from
        its extension.
        """
        args = parse_argstring(self.file_magic, line)
        path = self._resolve(args.path)
        path.parent.mkdir(parents=True, exist_ok=True)
        existed = path.exists()
        text = cell if cell.endswith("\n") else cell + "\n"
        with open(path, "a" if args.append else "w") as f:
            f.write(text)
        verb = "Appended to" if args.append else ("Overwrote" if existed else "Wrote")
        n = text.count("\n")
        status = f'{verb} {_tilde(path)} ({n} line{"s" if n != 1 else ""})'
        display(
            {
                "text/html": _STYLE + f'<div class="cactup-status">{_html(status)}</div>',
                "text/plain": status,
            },
            raw=True,
        )

    # -- %show ---------------------------------------------------------

    @magic_arguments()
    @argument("path", help="File to show.")
    @argument("--lines", "-l", default=None, help="Line range to show, e.g. 10:40.")
    @argument("--lang", default=None, help="Language to highlight as (default: from the file name).")
    @line_magic("show")
    def show_magic(self, line: str):
        """Show a file with syntax highlighting."""
        args = parse_argstring(self.show_magic, line)
        path = self._resolve(args.path)
        try:
            text = path.read_text(errors="replace")
        except OSError as e:
            raise UsageError(f"%show: cannot read {_tilde(path)}: {e.strerror or e}") from None
        first = 1
        if args.lines:
            all_lines = text.splitlines(keepends=True)
            first, last = _line_range(args.lines, len(all_lines))
            text = "".join(all_lines[first - 1 : last])
        try:
            html = highlight.to_html(text, path.name, args.lang, first_line=first, title=_tilde(path))
        except ValueError as e:
            raise UsageError(f"%show --lang: {e}") from None
        display({"text/html": html, "text/plain": text}, raw=True)

    # -- helpers -------------------------------------------------------

    def _resolve(self, raw: str) -> Path:
        raw = os.path.expandvars(os.path.expanduser(shlex.split(raw)[0] if raw.strip() else raw))
        path = Path(raw)
        if not path.is_absolute():
            base = self._session.cwd if self._session is not None else os.getcwd()
            path = Path(base) / path
        return path


def _line_range(spec: str, count: int) -> tuple[int, int]:
    """`10:40`, `10:` or `:40` (1-based, inclusive) against a file of `count` lines."""
    lo, sep, hi = spec.partition(":")
    try:
        first = int(lo) if lo else 1
        last = int(hi) if hi else (count if sep else first)
    except ValueError:
        raise UsageError(f"%show --lines: expected FIRST:LAST line numbers, got {spec!r}") from None
    if first < 1 or last < first:
        raise UsageError(f"%show --lines: {spec!r} is not a range of lines (they count from 1)")
    if first > count:
        raise UsageError(f"%show --lines: the file has only {count} line{'s' if count != 1 else ''}")
    return first, min(last, count)


def _unescape(text: str) -> str:
    """`--stdin` text with `\\n`, `\\t` and `\\\\` turned into what they name.

    IPython hands the argument over with its quotes (`'y\\n'`), so one
    matching pair around it is removed first.
    """
    if len(text) >= 2 and text[0] == text[-1] and text[0] in "'\"":
        text = text[1:-1]
    out, i = [], 0
    while i < len(text):
        if text[i] == "\\" and i + 1 < len(text) and text[i + 1] in "nt\\":
            out.append({"n": "\n", "t": "\t", "\\": "\\"}[text[i + 1]])
            i += 2
        else:
            out.append(text[i])
            i += 1
    return "".join(out)


def _bundle(term: Terminal, result: Result | None, hint: str | None = None) -> dict:
    html = _STYLE + term.html()
    text = term.text()
    if hint:
        html += f'<div class="cactup-status cactup-hint">{_html(hint)}</div>'
        text += f"\n[{hint}]"
    if result is not None:
        status = _status_line(result)
        if status:
            fail = result.status != 0 or result.interrupted
            cls = "cactup-status cactup-fail" if fail else "cactup-status"
            html += f'<div class="{cls}">{_html(status)}</div>'
            text += f"\n[{status}]"
    return {"text/html": html, "text/plain": text}


def _status_line(result: Result) -> str:
    if result.exited and (result.interrupted or result.timed_out):
        why = "timed out" if result.timed_out else "stopped"
        return f"{why}; the shell was killed, and the next cell starts a fresh one"
    if result.timed_out:
        return f"timed out and interrupted (exit status {result.status})"
    if result.interrupted:
        return f"interrupted (exit status {result.status})"
    if result.exited:
        return f"the shell exited (status {result.status}); a fresh one starts with the next cell"
    if result.status != 0:
        return f"exit status {result.status}"
    return ""


def _tilde(path: Path) -> str:
    home = os.path.expanduser("~")
    s = str(path)
    return "~" + s[len(home) :] if s == home or s.startswith(home + "/") else s


def _html(s: str) -> str:
    import html

    return html.escape(s)
