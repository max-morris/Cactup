---
jupytext:
  text_representation:
    extension: .md
    format_name: myst
kernelspec:
  display_name: Python 3
  language: python
  name: python3
---

# 4a. Living with several installations

In this notebook you will

- install two more Einstein Toolkits: a project's own thornlist, and the
  development version (`master`),
- switch between installations, and run a command on one without switching,
- see where each installation came from,
- remove one.

*Time: about 15 minutes.*

```{code-cell} ipython3
%%shell
cactup-tutorial-catch-up 4a
```

## A project's thornlist

Many projects keep a thornlist of their own: the thorns that they use, from
the repositories and branches that they trust. This one is CarpetX on its
own, with every library that it can use, so a build of it has all of
CarpetX's features:

```{code-cell} ipython3
%show /opt/cactup-tutorial/thornlists/carpetx.th --lines 1:10
```

Install it as you installed `et-mp` in notebook 3: name it with `--alias`,
and give its link in your home its own name with `--symlink-name`, so that
`~/Cactus` keeps meaning `MyCactus`.

```{code-cell} ipython3
%%shell
cactup install --thornlist /opt/cactup-tutorial/thornlists/carpetx.th --alias carpetx --symlink-name carpetx --silent
```

## The development version

`cactup releases` ended by mentioning the manifest's `master` branch: the
Einstein Toolkit as it is being developed, newer than every release.
`cactup install master` installs its tip:

```{code-cell} ipython3
%%shell
cactup install master --alias et-master --symlink-name et-master --silent
```

## Switching

`cactup list` shows every installation and what it was made from. The
active one is where `build`, `sim` and `config` commands go:

```{code-cell} ipython3
%%shell
cactup list
ls -ld ~/Cactus ~/carpetx ~/et-master
```

`cactup use` switches. Everything after that goes to the installation that
you switched to, until you switch again:

```{code-cell} ipython3
%%shell
cactup use carpetx
cactup inst show
```

To run a single command on another installation, without switching, give
it `-I NAME`:

```{code-cell} ipython3
%%shell
cactup -I et-master inst show
cactup -I MyCactus config list
```

Switch back to `MyCactus`, which the following notebooks use:

```{code-cell} ipython3
%%shell
cactup use MyCactus
```

## Where an installation came from

Each installation remembers what it was made from: a release, `master`, or a
thornlist file (`inst show` above says which). It also remembers the commit
that its last fetch left every repository on, and `cactup inst delta` compares
that with the clones as they are now, repository by repository: right after
an install, every repository is where the fetch put it. Each repository is an
ordinary git clone, so git can say more; here, the commit that `master`'s
CarpetX is on, against the release's. (CarpetX's development branch is called
`main`; the Einstein Toolkit's `master` follows it.)

```{code-cell} ipython3
%%shell
cactup -I et-master inst delta
git -C ~/et-master/repos/CarpetX log --oneline -1
git -C ~/Cactus/repos/CarpetX log --oneline -1
```

## Removing an installation

`cactup inst delete` removes an installation: its source tree and its
configurations. It asks first; `--force` doesn't. It never removes
simulations: those live apart, under `~/.cactup/simulations/<alias>`, and
stay where they are, results included.

The link made by `--symlink-name` stays behind too; remove it yourself.

```{code-cell} ipython3
%%shell
cactup inst delete carpetx --force
rm ~/carpetx
cactup list
```

## Where this is documented

- [Installing releases](https://max-morris.github.io/Cactup/users/installing-releases.html)
  (releases, `master`, thornlists, `use`, `-I`, `inst delete`)
- [Command reference](https://max-morris.github.io/Cactup/reference/cli.html)

Next: **notebook 4b**, several configurations of one installation.
