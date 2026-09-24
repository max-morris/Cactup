# CactupDocs — frozen conventions (contract between modules)

This file is the single source of truth that every implementation piece agrees
on. Do not change the shapes here without updating all consumers.

CactupDocs is a Cargo workspace member at `<repo>/cactupdocs`. It generates a
static documentation site for `cactup` (the sibling `Cactup` package at the
repo root). It introspects `<repo>/src/args.rs` (clap) and
`<repo>/src/mdb/*.rs` (serde) via `syn`, merges the result with hand-authored
Markdown under `content/`, and renders HTML into an output directory.

## Directory layout

```
cactupdocs/
  src/
    main.rs                # CLI (build/serve/dump) + Config  [DONE]
    model.rs               # shared doc model types           [DONE, read-only]
    introspect/{mod,cli,mdb,template_vars}.rs
    markdown.rs render.rs serve.rs
  content/                 # hand-authored Markdown (editable, non-generated)
    _nav.toml
    index.md
    users/*.md
    authors/*.md
  templates/base.html      # minijinja layout
  assets/style.css assets/docs.js
  data/template-vars.toml  # descriptions for @VAR@ tokens
  site/                    # build output (gitignored)
```

## Data model

See `src/model.rs` for the authoritative Rust types (`DocModel`, `CliModel`,
`CliCommand`, `CliArg`, `MdbModel`, `MdbTable`, `MdbField`, `TemplateVar`).
All are `serde::Serialize`.

## `_nav.toml` schema (consumed by render.rs, authored by content agent)

```toml
site_title = "cactup"
tagline    = "Install & manage the Einstein Toolkit"
repo_url   = "https://github.com/..."   # optional; shown in header/footer

[[sections]]
title = "User Guide"

  [[sections.pages]]
  title = "Introduction"
  file  = "index.md"          # path relative to content/, WITH .md

  [[sections.pages]]
  title = "Installing cactup"
  file  = "users/installing.md"
```

Output URL for a page = its `file` with `.md` → `.html`, prefixed by
`base_url`. e.g. `users/installing.md` → `{base_url}users/installing.html`.
`index.md` → `{base_url}index.html` (the home page).

## Front matter (content pages)

A page MAY begin with a TOML front-matter block delimited by lines containing
exactly `+++`:

```
+++
title = "Running simulations"
description = "Create, submit, and monitor Cactus simulations."
+++
# body markdown here...
```

Keys: `title` (overrides the nav title for `<h1>`/`<title>`), `description`
(meta description). If the file does not start with `+++`, there is no front
matter. `markdown::parse_front_matter` returns `Page { front_matter, body }`.

## Markdown dialect (markdown.rs)

`markdown::to_html` and `markdown::to_plaintext` parse with the same options:
CommonMark plus pipe **tables**, footnotes, `~~strikethrough~~` and task lists.
Plain Markdown tables render as a bare `<table>` and are styled like
`.ref-table` through `.content table`.

Every heading gets a GitHub-style `id`: the heading text (code spans
included) lowercased, spaces turned into `-`, everything but letters, digits,
`-` and `_` dropped; a repeated slug gets `-1`, `-2`, …. So the heading
``## The `autoupdate` knob`` is linked as `page.html#the-autoupdate-knob`.

**Raw HTML** passes through unchanged. A block that starts with a tag (e.g.
`<div class="…">`) ends at the first blank line, so leave a blank line after
the opening tag and before the closing one to have Markdown (a fenced code
block, say) rendered inside it. Markdown is *not* rendered on a line that is
itself part of an HTML block: write `<code>` there, not backticks. Use raw
HTML sparingly; it exists for layout the Markdown cannot express.

### The home-page install box

`content/index.md` wraps the install one-liner in raw HTML:

````html
<div class="install-box">

To install cactup, run this in your terminal:

```sh
curl … | sh
```

<details>
<summary>No <code>curl</code>? …</summary>

```sh
wget … | sh
```

</details>

<p class="install-note">Linux x86_64 or aarch64, …</p>

</div>
````

`style.css` styles `.install-box` (a highlighted panel, larger code) and makes
its Copy button always visible (`.install-box pre .copy-btn`). The Copy button
itself is the one `docs.js` adds to every `<pre>` that has a `<code>` child
(class `copy-btn`); the install box needs no JavaScript of its own. The
install URL is the Pages root that CI deploys to
(`https://max-morris.github.io/Cactup/cactup-init.sh`).

## Generated-include tokens (in Markdown bodies)

Authors embed generated reference sections with these tokens, each on its own
line:

- `{{cactup:cli}}`                          — full CLI reference (all commands)
- `{{cactup:cli command="sim submit"}}`     — one command (space-joined path)
- `{{cactup:mdb-meta}}`                     — all meta.toml tables
- `{{cactup:mdb-optionlist}}`               — optionlist `[cactup]` header + rules
- `{{cactup:template-vars}}`                — the `@VAR@` catalog table

Expansion order in render.rs: (1) strip front matter, (2) Markdown→HTML via
`markdown::to_html`, (3) regex-replace each token — which after step 2 appears
as `<p>{{cactup:...}}</p>` or bare — with a generated HTML fragment. The regex
MUST tolerate an optional wrapping `<p> … </p>`.

Generated fragments are HTML using the CSS classes below.

## Generated HTML — CSS class contract (render.rs emits, style.css styles)

CLI:
```html
<section class="cli-command" id="cmd-cactup-sim-submit">
  <h3><code>cactup sim submit</code></h3>
  <p class="cli-about">Submit a simulation to the queue…</p>
  <div class="cli-longabout">…long help paragraphs (may be omitted)…</div>
  <table class="ref-table cli-args">
    <thead><tr><th>Argument</th><th>Description</th></tr></thead>
    <tbody>
      <tr><td><code>-n, --nodes &lt;N&gt;</code></td><td>Node count… <span class="hint">(default: 1)</span></td></tr>
      …
    </tbody>
  </table>
```

The Argument cell is built from the `CliArg` model: join `-s`/`--long` with
`, `, append the value placeholder `<VALUE_NAME>` (uppercased field name when
`value_name` is absent) for value-taking args, and render positionals as
`<NAME>`. Append `(default: X)`, `(global)`, `(repeatable)` hints (in a
`<span class="hint">`) to the Description as applicable. Example above assumes
this two-column shape.

```html
</section>
```

MDB tables:
```html
<section class="mdb-table" id="mdb-machine">
  <h3><code>[machine]</code></h3>            <!-- toml_path, else struct_name -->
  <p class="mdb-about">…struct doc…</p>
  <table class="ref-table">
    <thead><tr><th>Key</th><th>Type</th><th>Required</th><th>Description</th></tr></thead>
    <tbody>
      <tr><td><code>name</code></td><td>string</td><td>optional</td><td>…field doc…</td></tr>
    </tbody>
  </table>
</section>
```

Template vars: a single `<table class="ref-table template-vars">` with columns
Name (`<code>@NAME@</code>`), Description.

Callouts available to authors via blockquotes starting with `> [!NOTE]` or
`> [!WARNING]`: render.rs converts these to `<div class="callout callout-note">`
/ `callout-warning`. (Nice-to-have; if skipped, plain blockquotes are fine.)

## minijinja template context (render.rs → templates/base.html)

`base.html` is rendered once per page with this context:

```
site_title   : string
tagline      : string
repo_url     : string | none
base_url     : string   (always ends with "/")
year         : string   (hardcode "2026" or omit — no Date in build env)
nav          : [ { title, pages: [ { title, url, active(bool) } ] } ]
page         : { title, description, content_html }   # content_html is safe HTML
search_index : string   # url to search-index.json, = base_url ~ "search-index.json"
```

`base.html` requirements (IDs/classes the JS relies on):
- `<html>` may carry `data-theme` (set by JS).
- Sidebar nav container: `<aside class="sidebar">` containing the nav list;
  active link gets `class="active"`.
- Hamburger button `id="nav-toggle"` toggles class `open` on `.sidebar`.
- Theme toggle button `id="theme-toggle"`.
- Search: `<input id="search-input">` and `<div id="search-results"></div>`.
- Loads `{{base_url}}assets/style.css` and `{{base_url}}assets/docs.js` (defer).
- Content goes in `<main class="content">{{ page.content_html|safe }}</main>`.
- Link CSS/JS/nav with `base_url` prefix so it works under a project subpath.

## Search index (render.rs emits, docs.js consumes)

render.rs writes `{out}/search-index.json`:
```json
[ { "title": "Installing cactup", "url": "…/users/installing.html", "text": "plain text of page" }, … ]
```
docs.js fetches `{base_url}search-index.json`, does case-insensitive substring
match over title+text, renders up to ~10 result links into `#search-results`.

## Asset handling (render.rs)

Copy everything under `assets/` verbatim to `{out}/assets/`. Write each page's
HTML to its output path (creating parent dirs). Write `search-index.json` at
`{out}` root.

## `base_url`

Comes from `Config.base_url` (default `/`). render.rs must normalize it to end
with exactly one `/`. All emitted internal links/asset refs are `base_url` +
relative path.
