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

The site is automatically built and deployed to GitHub Pages via the `.github/workflows/docs.yml` workflow whenever changes are pushed to `master` affecting the documentation sources or Cactup crate.

To enable deployment:

1. Go to your GitHub repository settings
2. Navigate to **Pages**
3. Set the source to **Deploy from a branch** → **GitHub Actions** (or keep the default if already set to GitHub Actions)
4. The workflow will automatically deploy to `https://<username>.github.io/<repo>/` (or your custom domain)

The workflow automatically detects the site's base URL path and configures the generator accordingly.
