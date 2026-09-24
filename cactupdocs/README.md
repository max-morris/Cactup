# CactupDocs

A static documentation site generator for the Cactup project.

## What it does

CactupDocs introspects the `Cactup` crate's command-line interface (defined in `../src/args.rs` via clap) and machine database schema (defined in `../src/mdb/` via serde), merges the result with hand-authored Markdown from the `content/` directory, and generates a complete static documentation site.

## Building locally

Build the documentation site into `cactupdocs/site/`:

```bash
cargo run -p CactupDocs --release -- build
```

Or specify a custom output directory:

```bash
cargo run -p CactupDocs --release -- build --out /path/to/output
```

The output directory contains the complete static site ready to serve.

## Previewing locally

Start a local preview server on `127.0.0.1:8080`:

```bash
cargo run -p CactupDocs --release -- serve
```

Then open the URL printed to the console in your browser.

You can customize the bind address:

```bash
cargo run -p CactupDocs --release -- serve --addr 127.0.0.1:3000
```

## Inspecting the raw model

To see the extracted CLI and MDB schema as JSON (useful for debugging or analysis):

```bash
cargo run -p CactupDocs --release -- dump
```

This prints the complete `DocModel` containing `CliModel`, `MdbModel`, and template variables.

## Content structure

Hand-authored documentation lives in `content/`:

- `_nav.toml` — Site navigation and metadata (title, tagline, repo URL, page hierarchy)
- `index.md` — Home page
- `users/*.md`, `authors/*.md` — User and author guides

Each Markdown file may begin with TOML front matter (delimited by `+++` lines) to override the page title or set a meta description:

```markdown
+++
title = "Running simulations"
description = "Create, submit, and monitor Cactus simulations."
+++
# Body markdown here…
```

### Generated-include tokens

Authors can embed auto-generated reference sections using these tokens, each on its own line:

- `{{cactup:cli}}` — Full CLI reference (all commands and arguments)
- `{{cactup:cli command="sim submit"}}` — Single command reference (space-separated command path)
- `{{cactup:mdb-meta}}` — All machine database (`meta.toml`) tables and fields
- `{{cactup:mdb-optionlist}}` — The `[cactup]` section rules from optionlist
- `{{cactup:template-vars}}` — Catalog of `@VAR@` template variable names and descriptions

The generator expands these tokens into formatted HTML reference tables during the build.

## Theme and assets

Static assets (CSS, JavaScript) live in `assets/` and are copied verbatim to the output. The HTML layout template lives in `templates/base.html` and uses minijinja to render each page with site navigation, search, and theme toggle.

Additional data (like template variable descriptions) is in `data/`.

## Deployment via GitHub Pages

The site is built and deployed by the repository's single CI workflow, `.github/workflows/ci.yml`. Its `docs` job builds this site with `--base-url "/<repo>/"` (derived from the repository name) on every push and pull request; the `site` job adds the release artifacts next to it at the Pages root (the static cactup binaries under `<target>/`, `cactup-init.sh`, and `latest.json`, which installed binaries read to update themselves); on `master` the `deploy` job publishes the result to `https://<owner>.github.io/<repo>/`, after `publish-mdb` has published the machine database branch.

So the docs are the project's home page: `content/index.md` carries the install one-liner, and there is no separate landing page.

To enable deployment (once per repository): **Settings → Pages → Source: GitHub Actions**. To build exactly what CI builds, locally:

```bash
cargo run -p CactupDocs --release -- build --out _site --base-url /Cactup/
```
