"""Pygments lexers for %show and HTML export."""

from pygments.token import Comment, Keyword, Name, String

from cactup_tutorial.highlight import lexer_for, to_html


def tokens(text, name):
    return [(t, v) for t, v in lexer_for(name, None).get_tokens(text) if v.strip()]


def test_parfile():
    toks = tokens('ActiveThorns = "CarpetX Z4c"\nCarpetX::ncells_x = $n  # c\nIO::out_dir = @RUNDIR@\n', "wave.par")
    assert (Keyword, "ActiveThorns") in toks
    assert (Name.Namespace, "CarpetX") in toks and (Name.Attribute, "ncells_x") in toks
    assert (Name.Variable, "$n") in toks
    assert (Comment.Single, "# c") in toks
    assert (Name.Variable.Magic, "@RUNDIR@") in toks


def test_thornlist():
    toks = tokens("!URL = https://github.com/x/y.git\nCarpetX/CarpetX\n#DISABLED CarpetX/TestReal2\n# hi\n", "a.th")
    assert (Keyword, "!URL") in toks
    assert (Name.Class, "CarpetX") in toks
    assert any(t is Comment.Special for t, _ in toks)


def test_ccl_by_extension_and_case_insensitive_keywords():
    toks = tokens('implements: WaveToyX\nCCTK_REAL state TYPE=gf\n{ u } "doc"\n', "interface.ccl")
    kinds = dict((v.lower(), t) for t, v in toks)
    assert kinds["implements"] is Keyword and kinds["type"] is Keyword
    assert any(t is String.Double for t, _ in toks)


def test_template_tokens_inside_toml_and_shell():
    toml = tokens('simulation-home = "/scratch/@USER@/sims"\nx = "@ENV-OPTIONAL(HOME, /tmp)@"\n', "meta.toml")
    assert (Name.Variable.Magic, "@USER@") in toml
    assert (Name.Variable.Magic, "@ENV-OPTIONAL(HOME, /tmp)@") in toml
    sh = tokens("#SBATCH -N @NODES@\nexec @CACTUP@ sim run\n", "default.sh")
    assert (Name.Variable.Magic, "@NODES@") in sh and (Name.Variable.Magic, "@CACTUP@") in sh


def test_to_html_escapes_and_titles():
    html = to_html("<b>\n", "x.txt", title="~/x.txt")
    assert "&lt;b&gt;" in html and "~/x.txt" in html


def test_an_empty_thornlist_directive_does_not_swallow_the_next_line():
    toks = tokens("!CHECKOUT =\nCarpetX/Arith\nCarpetX/CarpetX\n", "x.th")
    assert ("Token.Name.Class", "Arith") in [(str(t), v) for t, v in toks]
