"""Terminal output rendered as the final picture, like a terminal shows it."""

from cactup_tutorial.render import MAX_LINES, Terminal


def render(data: bytes, **kw) -> Terminal:
    term = Terminal(**kw)
    term.feed(data)
    return term


def test_carriage_return_redraws_in_place():
    term = render(b"progress 10%\rprogress 50%\rprogress 100%\r\ndone\r\n")
    assert term.text() == "progress 100%\ndone"


def test_cursor_up_and_erase_line_like_a_progress_renderer():
    # Draw two bar lines, move back up over both and redraw them.
    frames = b"a  [#   ]\r\nb  [#   ]\r\n" + b"\x1b[2A\x1b[2Ka  [####]\r\n\x1b[2Kb  [####]\r\n"
    assert render(frames).text() == "a  [####]\nb  [####]"


def test_scrollback_keeps_lines_that_scroll_off():
    lines = b"".join(b"line %d\r\n" % i for i in range(100))
    term = render(lines, rows=10)
    text = term.text().split("\n")
    assert text[0] == "line 0" and text[-1] == "line 99" and len(text) == 100


def test_scrollback_is_bounded():
    lines = b"".join(b"%d\r\n" % i for i in range(MAX_LINES + 100))
    text = render(lines, rows=10).text().split("\n")
    assert text[0].endswith("earlier lines not shown")
    assert text[-1] == str(MAX_LINES + 99)


def test_colors_use_jupyterlab_ansi_classes():
    html = render(b"\x1b[31mred\x1b[0m \x1b[1;92mbold bright green\x1b[0m\r\n").html()
    assert '<span class="ansi-red-fg">red</span>' in html
    assert "ansi-green-intense-fg" in html and "ansi-bold" in html


def test_256_colors_become_inline_styles():
    html = render(b"\x1b[38;5;208morange\x1b[0m\r\n").html()
    assert "color:#" in html


def test_html_is_escaped():
    html = render(b"<script>alert(1)</script> & more\r\n").html()
    assert "<script>" not in html and "&lt;script&gt;" in html and "&amp;" in html


def test_trailing_blank_lines_are_trimmed_but_partial_lines_kept():
    assert render(b"a\r\n\r\n\r\n").text() == "a"
    assert render(b"no newline yet").text() == "no newline yet"


def test_utf8_split_across_feeds():
    term = Terminal()
    data = "✓ done\r\n".encode()
    term.feed(data[:1])
    term.feed(data[1:])
    assert term.text() == "✓ done"


def test_dim_text_renders_faint_and_extended_colors_are_not_dim():
    html = render(b"\x1b[2mfaint\x1b[22m plain \x1b[38;2;1;2;3mrgb\x1b[0m\r\n").html()
    assert 'style="opacity:0.6">faint' in html
    assert "plain" in html and "rgb" in html
    assert html.count("opacity") == 1


def test_malformed_escape_sequences_never_break_the_terminal():
    import random

    term = Terminal()
    term.feed(b"x\x1b[1;2;3Cy\x1b[?1;2m\x1b[;;K\x1b[?5H ok\r\n")
    rng = random.Random(1)
    for _ in range(2000):
        term.feed(bytes(rng.choice(b"\x1b[;?>0123456789ABCDHJKLPm@r\r\n x") for _ in range(12)))
    with open("/usr/bin/bash", "rb") as f:
        term.feed(f.read(200_000))
    term.feed(b"\r\nstill here\r\n")
    assert "still here" in term.text()
    assert term.html()


def test_a_bad_escape_sequence_loses_only_itself():
    term = Terminal()
    term.feed(b"A\x1b[?1;2cB\r\nC\r\n\x1b[1;2;3Cafter\r\n")
    text = term.text()
    assert "B" in text and "C" in text and "after" in text
