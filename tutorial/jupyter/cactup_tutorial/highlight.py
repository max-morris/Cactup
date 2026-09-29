"""Syntax-highlighted HTML for `%show`, colored by the JupyterLab theme."""

from __future__ import annotations

import html

from pygments import highlight
from pygments.formatters import HtmlFormatter
from pygments.lexers import get_lexer_by_name, get_lexer_for_filename
from pygments.lexers.special import TextLexer
from pygments.util import ClassNotFound

from .lexers import CactusParLexer, CCLLexer, OptionlistLexer, TemplateTokenFilter, ThornlistLexer

_OURS = {
    "cactus-par": CactusParLexer,
    "par": CactusParLexer,
    "thornlist": ThornlistLexer,
    "th": ThornlistLexer,
    "ccl": CCLLexer,
    "optionlist": OptionlistLexer,
    "cfg": OptionlistLexer,
}

# Pygments token classes mapped onto the CSS variables JupyterLab's editor
# theme defines, so %show output matches the editor in light and dark mode.
_CSS = """
<style>
.cactup-show { margin: 0.2em 0; }
.cactup-show .cactup-show-title { font-family: var(--jp-ui-font-family);
  font-size: var(--jp-ui-font-size0); color: var(--jp-ui-font-color2); margin-bottom: 0.2em; }
.jp-RenderedHTMLCommon .cactup-show pre, .cactup-show pre { margin: 0; padding: 0.4em 0.6em; line-height: 1.35;
  background: var(--jp-cell-editor-background); border: 1px solid var(--jp-cell-editor-border-color);
  border-radius: 2px; font-family: var(--jp-code-font-family); font-size: var(--jp-code-font-size);
  color: var(--jp-content-font-color1); overflow-x: auto; }
.cactup-show .linenos { color: var(--jp-ui-font-color3); padding-right: 0.8em; user-select: none; }
.cactup-show .c, .cactup-show .c1, .cactup-show .cm, .cactup-show .ch { color: var(--jp-mirror-editor-comment-color); font-style: italic; }
.cactup-show .cp, .cactup-show .cs { color: var(--jp-mirror-editor-meta-color); }
.cactup-show .k, .cactup-show .kn, .cactup-show .kd, .cactup-show .kr, .cactup-show .kt, .cactup-show .ow { color: var(--jp-mirror-editor-keyword-color); font-weight: bold; }
.cactup-show .kc { color: var(--jp-mirror-editor-atom-color); }
.cactup-show .s, .cactup-show .s1, .cactup-show .s2, .cactup-show .sb, .cactup-show .sh, .cactup-show .sd, .cactup-show .se { color: var(--jp-mirror-editor-string-color); }
.cactup-show .m, .cactup-show .mi, .cactup-show .mf, .cactup-show .mh { color: var(--jp-mirror-editor-number-color); }
.cactup-show .o { color: var(--jp-mirror-editor-operator-color); font-weight: bold; }
.cactup-show .p { color: var(--jp-mirror-editor-punctuation-color); }
.cactup-show .nv, .cactup-show .vi, .cactup-show .vg { color: var(--jp-mirror-editor-variable-2-color); }
.cactup-show .vm { color: var(--jp-mirror-editor-variable-3-color); font-weight: bold;
  background: var(--jp-layout-color2); border-radius: 2px; }
.cactup-show .nn { color: var(--jp-mirror-editor-def-color); }
.cactup-show .na, .cactup-show .nt { color: var(--jp-mirror-editor-property-color); }
.cactup-show .nc, .cactup-show .nf, .cactup-show .nb { color: var(--jp-mirror-editor-def-color); }
.cactup-show .gh, .cactup-show .gu { color: var(--jp-mirror-editor-header-color); font-weight: bold; }
</style>
"""


def lexer_for(name: str, lang: str | None):
    if lang:
        cls = _OURS.get(lang.lower())
        try:
            lexer = cls() if cls else get_lexer_by_name(lang)
        except ClassNotFound:
            raise ValueError(f"no highlighting for language {lang!r}") from None
    else:
        ext = name.rsplit(".", 1)[-1].lower() if "." in name else ""
        cls = _OURS.get(ext)
        if cls:
            lexer = cls()
        else:
            try:
                lexer = get_lexer_for_filename(name)
            except ClassNotFound:
                lexer = TextLexer()
    lexer.add_filter(TemplateTokenFilter())
    return lexer


def to_html(text: str, name: str, lang: str | None = None, first_line: int = 1, title: str | None = None) -> str:
    formatter = HtmlFormatter(nowrap=False, cssclass="cactup-src", linenos="inline", linenostart=first_line)
    body = highlight(text, lexer_for(name, lang), formatter)
    heading = f'<div class="cactup-show-title">{html.escape(title)}</div>' if title else ""
    return f'{_CSS}<div class="cactup-show">{heading}{body}</div>'
