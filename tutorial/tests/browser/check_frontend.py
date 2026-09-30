"""Drive JupyterLab in headless Chromium and check the tutorial's frontend.

Runs in its own container next to a tutorial container (see run.sh). It opens
languages.ipynb, runs every cell, and checks that

- every code cell's editor uses the language its magic names
  (CodeMirror puts the language name on the editor's content element);
- the magic line and @VAR@ tokens are decorated;
- %%shell output rendered as a terminal (colors, a redrawn progress bar);
- a Jupyter interrupt stops a running %%shell cell;

and saves screenshots (light and dark theme) to /out for a human look.
"""

import re
import sys
import time

from playwright.sync_api import expect, sync_playwright

URL = sys.argv[1]  # e.g. http://lab:8888/lab/tree/tutorial/languages.ipynb?token=...
OUT = "/out"

EXPECTED = {
    "%%shell\necho": "shell",
    "%%file lw.par": "cactus-par",
    "%%file mylab/meta.toml": "toml",
    "%%file forks.th": "cactus-thornlist",
    "%%file interface.ccl": "cactus-ccl",
    "%%file default.sh": "shell",
    "%show lw.par": "python",
    "import numpy": "python",
}

failures = []


def check(cond, what):
    print(("ok   " if cond else "FAIL ") + what, flush=True)
    if not cond:
        failures.append(what)


with sync_playwright() as p:
    browser = p.chromium.launch()
    page = browser.new_page(viewport={"width": 1400, "height": 2400})
    page.goto(URL)
    page.wait_for_selector(".jp-Notebook .jp-Cell", timeout=60000)
    # Wait for the kernel so the notebook's language is known.
    page.wait_for_selector(".jp-Notebook-ExecutionIndicator[data-status='idle']", timeout=60000)
    time.sleep(2)

    cells = page.locator(".jp-Notebook .jp-CodeCell")
    langs = {}
    for i in range(cells.count()):
        cell = cells.nth(i)
        text = cell.locator(".cm-content").inner_text()
        lang = cell.locator(".cm-content").get_attribute("data-language")
        langs[text.split("\n")[0]] = lang
        for prefix, want in EXPECTED.items():
            first, _, rest = prefix.partition("\n")
            if text.startswith(first) and (not rest or rest in text):
                check(lang == want, f"cell {i} ({first!r}) highlighted as {lang}, want {want}")
    check(page.locator(".cm-cactup-magic-line").count() >= 8, "magic lines are decorated")
    check(page.locator(".cm-cactup-template").count() >= 4, "@VAR@ tokens are decorated")

    # Run everything except the last cell (the long sleep), then run that one
    # and interrupt it.
    page.keyboard.press("Escape")
    last = cells.count() - 1
    for i in range(last):
        cells.nth(i).click()
        page.keyboard.press("Escape")
        page.keyboard.press("Shift+Enter")
        page.wait_for_function(
            "i => { const c = document.querySelectorAll('.jp-Notebook .jp-CodeCell')[i];"
            " const p = c.querySelector('.jp-InputPrompt').innerText; return /\\[\\d+\\]/.test(p); }",
            arg=i,
            timeout=60000,
        )
    term = page.locator(".cactup-term")
    check(term.count() >= 2, "%%shell output renders as a terminal")
    red = page.locator(".cactup-term .ansi-red-fg").first
    check(red.count() == 1, "ANSI colors map to JupyterLab classes")
    # The theme must really style them: a class JupyterLab's CSS doesn't
    # reach renders in the default text color.
    colors = red.evaluate(
        "e => [getComputedStyle(e).color, getComputedStyle(e.closest('pre')).color]"
    ) if red.count() else ["", ""]
    check(colors[0] != colors[1], f"ANSI colors are visible (red {colors[0]}, text {colors[1]})")
    ws = term.first.evaluate("e => getComputedStyle(e).whiteSpace")
    check(ws == "pre", f"terminal output never wraps a second time (white-space: {ws})")
    progress = [t for t in term.all_inner_texts() if "100%" in t]
    check(bool(progress) and "10%" not in progress[0].replace("100%", ""), "a redrawn progress bar shows only its final frame")

    cells.nth(last).click()
    page.keyboard.press("Escape")
    # Replace the timeout cell's own --timeout: interrupt it by hand instead.
    page.keyboard.press("Enter")
    page.keyboard.press("Control+a")
    page.keyboard.type("%%shell\nsleep 60\n")
    page.keyboard.press("Escape")
    page.keyboard.press("Shift+Enter")
    time.sleep(2)
    page.keyboard.press("Escape")
    page.keyboard.press("i")
    page.keyboard.press("i")
    started = time.monotonic()
    page.wait_for_function(
        "() => [...document.querySelectorAll('.cactup-status')].some(e => /interrupted/.test(e.innerText))",
        timeout=20000,
    )
    check(time.monotonic() - started < 10, "an interrupt stops a running %%shell cell promptly")

    def rerun_last(source):
        cells.nth(last).click()
        page.keyboard.press("Escape")
        page.keyboard.press("Enter")
        page.keyboard.press("Control+a")
        page.keyboard.insert_text(source)  # no auto-closed quotes or brackets
        page.keyboard.press("Escape")
        page.keyboard.press("Shift+Enter")

    def last_has(pattern, timeout):
        try:
            page.wait_for_function(
                "([i, p]) => { const c = document.querySelectorAll('.jp-Notebook .jp-CodeCell')[i];"
                " return new RegExp(p).test(c.querySelector('.jp-OutputArea').innerText); }",
                arg=[last, pattern],
                timeout=timeout,
            )
            return True
        except Exception:
            return False

    def interrupt():
        page.keyboard.press("Escape")
        page.keyboard.press("i")
        page.keyboard.press("i")

    # Live output: a prompt printed just before a quiet wait shows while the
    # cell is still running, not when it ends.
    rerun_last("%%shell\necho first; sleep 0.3; printf 'Continue? '; sleep 30\n")
    shown = last_has("Continue\\?", 3000)
    check(shown and not last_has("interrupted|exit status", 100), "output before a pause shows while the cell runs")
    interrupt()
    last_has("interrupted", 15000)

    # Double interrupt: the first Ctrl-C is caught (the program keeps going),
    # the second stops it, the way cactup's own two-stage interrupt works.
    rerun_last("%%shell\nbash -c 'trap \"echo got-one; trap - INT\" INT; while :; do sleep 0.1; done'\n")
    time.sleep(1.5)
    interrupt()
    first = last_has("got-one", 5000) and not last_has("interrupted", 500)
    check(first, "a first interrupt reaches the program and it keeps running")
    interrupt()
    check(last_has("interrupted", 10000), "a second interrupt stops it")

    # Long output: a box that scrolls itself and starts at its end.
    rerun_last("%%shell\nseq 1 300\n")
    last_has("300", 15000)
    box = cells.nth(last).locator(".cactup-box")
    check(box.count() == 1, "long output goes in a box")
    if box.count() == 1:
        sizes = box.evaluate("e => [e.scrollHeight, e.clientHeight, e.scrollTop,"
                             " e.querySelector('pre').scrollHeight, e.querySelector('pre').clientHeight]")
        scroll_height, client_height, _, pre_scroll, pre_client = sizes
        check(scroll_height > client_height and pre_scroll == pre_client,
              f"the box scrolls, not the terminal inside it (box {scroll_height}/{client_height}, "
              f"terminal {pre_scroll}/{pre_client})")
        # Where the output's last character is drawn, against the box's bottom.
        box_rect, end = box.evaluate(
            "e => { const w = document.createTreeWalker(e.querySelector('pre'), NodeFilter.SHOW_TEXT);"
            " let last = null; while (w.nextNode()) if (w.currentNode.data.trim()) last = w.currentNode;"
            " const r = document.createRange(); r.setStart(last, last.data.trimEnd().length - 1);"
            " r.setEnd(last, last.data.trimEnd().length);"
            " return [e.getBoundingClientRect().bottom, r.getBoundingClientRect().bottom]; }")
        check(abs(box_rect - end) < 40, f"the box starts at its end (last line {end:.0f}, box bottom {box_rect:.0f})")

    page.screenshot(path=f"{OUT}/notebook-light.png", full_page=True)
    # Save, so the outputs survive the reload into the dark theme.
    page.keyboard.press("Control+s")
    time.sleep(2)

    # The dark theme, set the way the settings editor would.
    base, _, query = URL.partition("/lab/")
    token = re.search(r"token=([^&]+)", URL).group(1)
    page.request.put(
        f"{base}/lab/api/settings/@jupyterlab/apputils-extension:themes",
        headers={"Authorization": f"token {token}"},
        data={"raw": '{"theme": "JupyterLab Dark", "adaptive-theme": false}'},
    )
    page.reload()
    page.wait_for_selector(".jp-Notebook .jp-Cell", timeout=60000)
    time.sleep(3)
    page.screenshot(path=f"{OUT}/notebook-dark.png", full_page=True)

    # A .par file opened in the file editor is highlighted too.
    page.goto(re.sub(r"/lab/tree/[^?]*", "/lab/tree/tutorial/lw.par", URL))
    page.wait_for_selector(".jp-FileEditor .cm-content", timeout=60000)
    lang = page.locator(".jp-FileEditor .cm-content").get_attribute("data-language")
    check(lang == "cactus-par", f"the file editor highlights .par files (got {lang})")
    page.screenshot(path=f"{OUT}/file-editor.png")

    # Files the file editor must not mistake for something else.
    auth = {"Authorization": f"token {token}"}
    for name, want in (("x.cfg", "cactus-optionlist"), ("make.code.defn", "shell")):
        page.request.put(
            f"{base}/api/contents/tutorial/{name}",
            headers=auth,
            data={"type": "file", "format": "text", "content": "CPP = cpp\n"},
        )
        # A fresh workspace each time, so the only editor open is this file's.
        page.goto(re.sub(r"/lab/tree/[^?]*", f"/lab/tree/tutorial/{name}", URL) + "&reset")
        page.wait_for_selector(".jp-FileEditor .cm-content", timeout=60000)
        time.sleep(1)
        editors = page.locator(".jp-FileEditor .cm-content")
        got = editors.first.get_attribute("data-language") if editors.count() == 1 else f"{editors.count()} editors"
        check(got == want, f"the file editor opens {name} as {want} (got {got})")

    # Opening and saving files checkpointed them somewhere other than beside
    # them: a stray .ipynb_checkpoints in a thorn's src/ changes its shape.
    listing = page.request.get(
        f"{base}/api/contents/tutorial", headers={"Authorization": f"token {token}"}
    ).json()
    names = [item["name"] for item in listing["content"]]
    check(".ipynb_checkpoints" not in names, f"no checkpoints beside the files (found {names})")
    browser.close()

print(f"{len(failures)} failure(s)")
sys.exit(1 if failures else 0)
