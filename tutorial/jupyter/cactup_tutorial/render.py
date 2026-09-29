"""Turn terminal output into a notebook display, the way a terminal shows it.

The raw bytes a command writes go through a `pyte` terminal emulator, so
carriage returns, cursor movement and line erasing (everything a progress bar
does to redraw itself) end up as the final picture rather than as a smear of
every intermediate frame. Lines that scroll off the top of the emulated screen
are kept as scrollback, so the rendering reads like a terminal's log.

Colors map onto JupyterLab's own ANSI classes (`ansi-red-fg`, ...), which the
active theme styles, so output looks right in both light and dark themes.
JupyterLab only styles them inside `.jp-RenderedText pre` (plain stdout), so
the HTML wraps its `<pre>` in a `jp-RenderedText` element.
"""

from __future__ import annotations

import html

import pyte

from .session import COLUMNS, ROWS

MAX_LINES = 5000

_NAMES = {
    "black": "black",
    "red": "red",
    "green": "green",
    "brown": "yellow",
    "blue": "blue",
    "magenta": "magenta",
    "cyan": "cyan",
    "white": "white",
}


class _LogScreen(pyte.Screen):
    """A screen that remembers the lines it scrolls off the top.

    pyte has no "dim" attribute (SGR 2), which cactup uses for secondary text
    such as `--trace` lines; it is kept in the blink attribute, which nothing
    the tutorial runs uses, and rendered faint.
    """

    def select_graphic_rendition(self, *attrs: int, private: bool = False) -> None:
        if private:
            return  # `ESC[?...m`: not a style, and pyte would reject it
        super().select_graphic_rendition(*attrs)
        dim = None
        i = 0
        while i < len(attrs):
            a = attrs[i]
            if a in (38, 48):  # an extended color: skip its parameters
                i += {5: 3, 2: 5}.get(attrs[i + 1] if i + 1 < len(attrs) else 0, 1)
                continue
            if a == 2:
                dim = True
            elif a in (0, 22):
                dim = False
            i += 1
        if dim is not None:
            self.cursor.attrs = self.cursor.attrs._replace(blink=dim)

    def __init__(self, columns: int, lines: int):
        super().__init__(columns, lines)
        self.scrollback: list[dict] = []
        self.dropped = 0

    def index(self) -> None:
        top, bottom = self.margins or pyte.screens.Margins(0, self.lines - 1)
        if self.cursor.y == bottom and top == 0:
            self.scrollback.append(dict(self.buffer[0]))
            if len(self.scrollback) > MAX_LINES:
                excess = len(self.scrollback) - MAX_LINES
                del self.scrollback[:excess]
                self.dropped += excess
        super().index()


class Terminal:
    def __init__(self, columns: int = COLUMNS, rows: int = ROWS):
        self.screen = _LogScreen(columns, rows)
        self.stream = pyte.ByteStream(self.screen)

    def feed(self, data: bytes) -> None:
        # pyte raises on many malformed escape sequences (`cat` of a binary
        # file prints plenty); it resets its parser first, so the rest of the
        # output still renders, less the rest of this chunk.
        try:
            self.stream.feed(data)
        except Exception:
            pass

    def mid_line(self) -> bool:
        """The output so far ends in an unfinished line (a prompt, perhaps)."""
        return self.screen.cursor.x > 0

    # -- output ----------------------------------------------------------

    def _rows(self) -> list[dict]:
        screen = self.screen
        rows = [screen.buffer[y] for y in range(screen.lines)]
        last = -1
        for y, row in enumerate(rows):
            if any(ch.data.strip() for ch in row.values()):
                last = y
        if screen.cursor.x > 0:
            last = max(last, screen.cursor.y)
        return screen.scrollback + rows[: last + 1]

    def text(self) -> str:
        lines = [_line_text(row, self.screen.columns) for row in self._rows()]
        if self.screen.dropped:
            lines.insert(0, f"... {self.screen.dropped} earlier lines not shown")
        return "\n".join(lines)

    def html(self) -> str:
        parts = []
        if self.screen.dropped:
            parts.append(
                f'<span class="ansi-bold">... {self.screen.dropped} earlier lines not shown</span>\n'
            )
        for row in self._rows():
            parts.append(_line_html(row, self.screen.columns))
            parts.append("\n")
        body = "".join(parts).rstrip("\n")
        return f'<div class="jp-RenderedText"><pre class="cactup-term">{body}</pre></div>'


def _line_text(row: dict, columns: int) -> str:
    return "".join(row[x].data if x in row else " " for x in range(columns)).rstrip()


def _line_html(row: dict, columns: int) -> str:
    # Trailing blanks carry no information and would make every line
    # full-width; drop them unless they are colored (a reverse-video bar).
    end = columns
    while end > 0:
        ch = row.get(end - 1)
        if ch is None or (not ch.data.strip() and _style(ch) == ((), ())):
            end -= 1
        else:
            break
    out = []
    run_style = None
    run_text: list[str] = []

    def flush() -> None:
        if not run_text:
            return
        text = html.escape("".join(run_text))
        classes, styles = run_style or ((), ())
        if classes or styles:
            attrs = ""
            if classes:
                attrs += f' class="{" ".join(classes)}"'
            if styles:
                attrs += f' style="{";".join(styles)}"'
            out.append(f"<span{attrs}>{text}</span>")
        else:
            out.append(text)
        run_text.clear()

    for x in range(end):
        ch = row.get(x)
        if ch is not None and ch.data == "":
            continue  # the second column of a wide character before it
        style = _style(ch) if ch is not None else ((), ())
        data = ch.data if ch is not None else " "
        if style != run_style:
            flush()
            run_style = style
        run_text.append(data)
    flush()
    return "".join(out)


def _color(value: str, layer: str) -> tuple[list[str], list[str]]:
    if value == "default":
        return [], []
    if value in _NAMES:
        return [f"ansi-{_NAMES[value]}-{layer}"], []
    if value.startswith("bright") and value[6:] in _NAMES:
        return [f"ansi-{_NAMES[value[6:]]}-intense-{layer}"], []
    if len(value) == 6:
        prop = "color" if layer == "fg" else "background-color"
        return [], [f"{prop}:#{value}"]
    return [], []


def _style(ch) -> tuple[tuple[str, ...], tuple[str, ...]]:
    fg, bg = ch.fg, ch.bg
    if ch.reverse:
        fg, bg = bg, fg
    classes: list[str] = []
    styles: list[str] = []
    c, s = _color(fg, "fg")
    classes += c
    styles += s
    c, s = _color(bg, "bg")
    classes += c
    styles += s
    if ch.reverse:
        if fg == "default":
            classes.append("ansi-default-inverse-fg")
        if bg == "default":
            classes.append("ansi-default-inverse-bg")
    if ch.bold:
        classes.append("ansi-bold")
    if ch.underscore:
        classes.append("ansi-underline")
    if ch.italics:
        styles.append("font-style:italic")
    if ch.strikethrough:
        styles.append("text-decoration:line-through")
    if ch.blink:  # dim; see _LogScreen
        styles.append("opacity:0.6")
    return tuple(classes), tuple(styles)
