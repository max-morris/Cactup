"""Pygments lexers for the Cactus file formats the tutorial shows.

They drive `%show` and any HTML export of the notebooks; the JupyterLab
editor has its own CodeMirror definitions of the same languages.
"""

from __future__ import annotations

import re

from pygments.filter import Filter
from pygments.lexer import RegexLexer, bygroups, include, words
from pygments.token import (
    Comment,
    Keyword,
    Name,
    Number,
    Operator,
    Punctuation,
    String,
    Text,
    Whitespace,
)

# cactup's template tokens: @VAR@, @ENV(NAME)@, @KNOB(name)@ and their
# -OPTIONAL forms, and the @@ escape.
TEMPLATE_TOKEN = r"@(?:@|[A-Z][A-Z0-9_]*@|(?:ENV|KNOB)(?:-OPTIONAL)?\([^)\n]*\)@)"


class CactusParLexer(RegexLexer):
    """Cactus parameter files (`.par`)."""

    name = "Cactus parameter file"
    aliases = ["cactus-par", "par"]
    filenames = ["*.par"]
    flags = re.MULTILINE | re.IGNORECASE

    tokens = {
        "root": [
            (r"\s+", Whitespace),
            (r"#.*$", Comment.Single),
            (r"!.*$", Comment.Preproc),
            (TEMPLATE_TOKEN, Name.Variable.Magic),
            (r"\bActiveThorns\b", Keyword),
            (r"([A-Za-z_]\w*)(::)([A-Za-z_]\w*)", bygroups(Name.Namespace, Punctuation, Name.Attribute)),
            (r"\$[A-Za-z_]\w*", Name.Variable),
            (r'"', String.Double, "string"),
            (r"\b(yes|no|true|false)\b", Keyword.Constant),
            (r"[+-]?(\d+\.\d*|\.\d+|\d+)([eEdD][+-]?\d+)?", Number),
            (r"[=*/+\-]", Operator),
            (r"[\[\](),]", Punctuation),
            (r"[A-Za-z_]\w*", Name),
            (r".", Text),
        ],
        "string": [
            (TEMPLATE_TOKEN, Name.Variable.Magic),
            (r'[^"@]+', String.Double),
            (r"@", String.Double),
            (r'"', String.Double, "#pop"),
        ],
    }


class ThornlistLexer(RegexLexer):
    """Component Retrieval Language thornlists (`.th`)."""

    name = "Cactus thornlist"
    aliases = ["thornlist", "crl"]
    filenames = ["*.th"]
    flags = re.MULTILINE

    tokens = {
        "root": [
            (r"\s+", Whitespace),
            (r"(#DISABLED)(\s+)(\S+)", bygroups(Comment.Special, Whitespace, Name.Class)),
            (r"#.*$", Comment.Single),
            (
                r"(!\w+)([ \t]*)(=)([ \t]*)(.*)$",
                bygroups(Keyword, Whitespace, Operator, Whitespace, String),
            ),
            (r"(!\w+)", Keyword),
            (r"([A-Za-z0-9_\-]+)(/)([A-Za-z0-9_\-]+)", bygroups(Name.Namespace, Punctuation, Name.Class)),
            (r"\$\w+", Name.Variable),
            (r".", Text),
        ],
    }


_CCL_KEYWORDS = (
    "implements inherits friend includes include source in uses provides requires "
    "function subroutine with private public protected restricted shares extends "
    "schedule group before after while if as lang storage trigger triggers sync "
    "reads writes options option tags type dim timelevels size distrib "
    "global local level singlemap array scalar gf "
    "int real keyword string boolean cctk_int cctk_real cctk_complex cctk_pointer "
    "cctk_pointer_to_const cctk_string void steerable accumulator accumulator_base "
    "never always recover optional optional_ifactive thorn thorns "
    "startup wragh paramcheck basegrid initial postinitial poststep prestep evol "
    "analysis checkpoint terminate shutdown recover_variables recover_parameters "
    "postrestrict postregrid postregridinitial poststep cpinitial postrestrictinitial "
    "centering tags inout in out"
).split()


class CCLLexer(RegexLexer):
    """Cactus Configuration Language (`interface.ccl`, `param.ccl`, ...)."""

    name = "Cactus CCL"
    aliases = ["ccl"]
    filenames = ["*.ccl"]
    flags = re.MULTILINE | re.IGNORECASE

    tokens = {
        "root": [
            (r"\s+", Whitespace),
            (r"#.*$", Comment.Single),
            (r'"', String.Double, "string"),
            (r"'", String.Single, "sstring"),
            (words(_CCL_KEYWORDS, prefix=r"\b", suffix=r"\b"), Keyword),
            (r"[+-]?(\d+\.\d*|\.\d+|\d+)([eEdD][+-]?\d+)?", Number),
            (r"(\*|::|:|=|\.\.|,)", Operator),
            (r"[{}()\[\]]", Punctuation),
            (r"[A-Za-z_]\w*", Name),
            (r".", Text),
        ],
        "string": [
            (r'[^"\\]+', String.Double),
            (r"\\.", String.Escape),
            (r'"', String.Double, "#pop"),
        ],
        "sstring": [
            (r"[^'\\]+", String.Single),
            (r"\\.", String.Escape),
            (r"'", String.Single, "#pop"),
        ],
    }


class OptionlistLexer(RegexLexer):
    """Cactus optionlists (`.cfg`): `NAME = value` lines."""

    name = "Cactus optionlist"
    aliases = ["optionlist"]
    filenames = ["*.cfg"]
    flags = re.MULTILINE

    tokens = {
        "root": [
            (r"\s+", Whitespace),
            (r"#.*$", Comment.Single),
            (r"^([A-Za-z_]\w*)(\s*)(=)", bygroups(Name.Variable, Whitespace, Operator)),
            include("value"),
        ],
        "value": [
            (TEMPLATE_TOKEN, Name.Variable.Magic),
            (r"\$\{?\w+\}?", Name.Variable),
            (r"[^\n@$]+", String),
            (r"[@$]", String),
        ],
    }


class TemplateTokenFilter(Filter):
    """Mark cactup template tokens inside any other language's tokens.

    MDB files are ordinary TOML and shell with `@VAR@` tokens sprinkled in;
    this lets any lexer show those tokens distinctly.
    """

    _re = re.compile(TEMPLATE_TOKEN)

    def filter(self, lexer, stream):
        for ttype, value in stream:
            if ttype is Name.Variable.Magic or "@" not in value:
                yield ttype, value
                continue
            pos = 0
            for m in self._re.finditer(value):
                if m.start() > pos:
                    yield ttype, value[pos : m.start()]
                yield Name.Variable.Magic, m.group(0)
                pos = m.end()
            if pos < len(value):
                yield ttype, value[pos:]
