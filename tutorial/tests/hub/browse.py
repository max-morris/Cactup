"""The hub in a browser, as an attendee meets it (run by browse.sh in the
Playwright container): the login page and its token field, signing up,
JupyterLab opening notebook 1, a cell run with its highlighting and output,
the File menu's labels, and a terminal. Screenshots go to /out.

    python browse.py URL TOKEN
"""

import re
import secrets
import sys

from playwright.sync_api import sync_playwright

URL, TOKEN = sys.argv[1].rstrip("/"), sys.argv[2]
OUT = "/out"
failures = []


def check(cond, what):
    print(f"{'ok  ' if cond else 'FAIL'} {what}", flush=True)
    if not cond:
        failures.append(what)


with sync_playwright() as p:
    browser = p.chromium.launch()
    page = browser.new_page(viewport={"width": 1400, "height": 900})
    page.goto(f"{URL}/hub/login")
    page.wait_for_selector("#username_input")
    check(page.locator("#otp_input").count() == 1, "the login page has the token field")
    check("Session token" in page.inner_text("body"), "the token field is labeled")
    check("New here?" in page.inner_text("body"), "the login page says how to sign up")
    page.screenshot(path=f"{OUT}/login.png")

    # A wrong token: refused, with the hub's usual message.
    name = f"browse{secrets.token_hex(2)}"
    page.fill("#username_input", name)
    page.fill("#password_input", "a good password")
    page.fill("#otp_input", "not the token 7 at all!")
    page.click("#login_submit")
    page.wait_for_selector(".login_error")
    check("Invalid username or password" in page.inner_text(".login_error"), "a wrong token is refused")

    page.fill("#username_input", name)
    page.fill("#password_input", "a good password")
    page.fill("#otp_input", TOKEN)
    page.click("#login_submit")
    page.wait_for_url(re.compile(r".*/lab.*"), timeout=300_000)
    page.wait_for_selector(".jp-Notebook .jp-Cell", timeout=300_000)
    check("01-getting-started.ipynb" in page.title() or page.locator(".jp-mod-current >> text=01-getting-started").count() > 0,
          "JupyterLab opens notebook 1")
    page.wait_for_selector(".jp-Notebook-ExecutionIndicator[data-status='idle']", timeout=120_000)
    page.screenshot(path=f"{OUT}/notebook-1.png")

    # Run the catch-up cell (the first code cell): magic highlighting, output.
    # JupyterLab may still redraw the notebook just after it goes idle,
    # detaching the cell the locator found: try again on a fresh one.
    first = page.locator(".jp-Notebook .jp-CodeCell").first
    for attempt in range(5):
        try:
            first.scroll_into_view_if_needed(timeout=10_000)
            break
        except Exception:
            if attempt == 4:
                raise
            page.wait_for_timeout(1000)
    try:
        first.locator(".cm-cactup-magic-line").first.wait_for(timeout=30_000)
        highlighted = True
    except Exception:
        highlighted = False
    check(highlighted, "the %%shell line is highlighted")
    first.locator(".jp-InputArea-editor").click()
    page.wait_for_timeout(500)
    page.keyboard.press("Shift+Enter")
    try:
        page.wait_for_function(
            "() => /\\[\\d+\\]/.test(document.querySelector('.jp-Notebook .jp-CodeCell .jp-InputArea-prompt')"
            "?.textContent || '')", timeout=120_000)
        ran = True
    except Exception:
        ran = False
    page.screenshot(path=f"{OUT}/catch-up.png")
    check(ran, "the catch-up cell runs")

    # The File menu names the document as a notebook.
    page.click(".lm-MenuBar-itemLabel >> text=File")
    menu = page.inner_text(".lm-Menu")
    check("Reload Notebook from Disk" in menu, "File menu: Reload Notebook from Disk")
    check("default" not in menu, "File menu: no 'default' labels")
    page.screenshot(path=f"{OUT}/file-menu.png")
    page.keyboard.press("Escape")

    # A terminal (notebook 7's follow view runs in one).
    page.click(".lm-MenuBar-itemLabel >> text=File")
    page.hover(".lm-Menu-itemLabel >> text=New")
    page.click(".lm-Menu-itemLabel >> text=Terminal")
    page.wait_for_selector(".xterm-screen", timeout=60_000)
    page.wait_for_timeout(2000)
    page.locator(".xterm-screen").click()
    # The terminal draws on a canvas, with no text to read here: it leaves a
    # file, which browse.sh looks for in the container.
    page.keyboard.type("hostname > ~/.browse-terminal-check; cactup --version\n")
    page.wait_for_timeout(3000)
    page.screenshot(path=f"{OUT}/terminal.png")
    print(f"user {name}", flush=True)
    browser.close()

print(f"{len(failures)} problem(s)")
sys.exit(1 if failures else 0)
