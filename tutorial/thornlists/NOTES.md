# Tutorial thornlists — maintainer notes

Not part of the image: attendees see the `.th` files, never this file. The
thornlists' own headers say only what a real project's would; everything
about how they were made, and what the tutorial relies on, is here.

| File | Used by | Source |
|---|---|---|
| `tutorial.th` | the `tutorial` config (stock install), the `et-mp` install (notebook 3) | curated from the release, below |
| `carpetx.th` | the CarpetX-only install (notebook 4a) | Max's private `carpetx-mp.th`, with stock repositories |
| `carpetx-mp-forks.th` | notebook 3's repoint to the forks | Max's `carpetx-mp-forks.th`, headers rewritten |

All three are parsed by cactup's own parser (a scratch dump test over
`thornlist::parse_with_base`) with no warnings, and `mirrors/mirror.py`'s
port of that parser gives identical components for them and for the
release and `master` lists.

## Rules every list follows

- **Release repositories and branches.** Every `!URL` and `!REPO_BRANCH` in
  `tutorial.th` and `carpetx.th` is the one in the manifest's
  `einsteintoolkit.th` at tag `ET_2026_05_v0`, spelled the same way, so the
  mirrors' `insteadOf` rules cover them with no new spellings.
- **Only thorns the release links (`tutorial.th`).** The stock install's
  `tutorial` config is built with `--thornlist tutorial.th`, and `cactup
  build` creates no arrangement links (only a fetch does), so every thorn in
  `tutorial.th` must be one the release itself enables. This is why the
  evolution is Cottonmouth's and not `SpacetimeX/Z4c`: the release checks
  out only `SpacetimeX/NewRadX`, so a stock install has no
  `arrangements/SpacetimeX/Z4c`. Checked by script: all 37 thorns are
  enabled in the release list.
- **Repository names match the forks.** cactup names a repository's
  directory after `!NAME`, or else after the URL's basename. `tutorial.th`
  keeps `!NAME = flesh` (as the release and the forks list do), and
  `CarpetX` and `SpacetimeX` have the forks' basenames, so notebook 3's
  refetch to the forks moves them by commit in the same directories.

## `tutorial.th`: how the thorns were chosen

A scratch script read every thorn's `.ccl` files at `origin/ET_2026_05` in
the `ET_2026_05_v0` installation (369 thorns) and followed every hard
requirement from the seeds (the CarpetX thorns the release enables, and the
two Cottonmouth thorns): `interface.ccl` `INHERITS`/`FRIEND` and `REQUIRES
FUNCTION`, `param.ccl` `SHARES`, `configuration.ccl` `REQUIRES`
(capabilities and `THORNS`). `USES INCLUDE` is not a requirement: the
flesh's `BuildHeaders.pl` writes a used header even when no thorn provides
it (CarpetX's only such include, `silo.hxx`, is guarded by
`HAVE_CAPABILITY_Silo`). `OPTIONAL` capabilities are left out, to keep the
object tree small, with one exception found by building: NSIMD.

| Thorn(s) | Why |
|---|---|
| the 28 CarpetX thorns the release enables | seeds: the driver, its infrastructure, examples and tests |
| `Cottonmouth/CottonmouthZ4c4m` | seed: INHERITS ADMBaseX Driver ODESolvers TmunuBaseX; REQUIRES Arith Loop AMReX NewRadX |
| `Cottonmouth/CottonmouthLinearWaveID` | seed: INHERITS ADMBaseX Driver ODESolvers; REQUIRES Arith Loop AMReX NewRadX |
| `SpacetimeX/NewRadX` | REQUIRES NewRadX (both Cottonmouth thorns) |
| `CactusBase/IOUtil` | CarpetX `SHARES: IO`, `REQUIRES IOUtil` |
| `ExternalLibraries/AMReX` | CarpetX, Loop, Cottonmouth REQUIRE it; it REQUIRES MPI |
| `ExternalLibraries/MPI`, `yaml_cpp`, `zlib` | CarpetX REQUIRES them |
| `ExternalLibraries/NSIMD` | Arith's `OPTIONAL NSIMD` is not optional: `simd.hxx` includes `nsimd/nsimd-all.hpp` unless `SIMD_DISABLE` (or `SIMD_CPU`) is defined, and its own comment says a capability check there can't work. Without NSIMD, Arith's first object fails to compile |

CarpetX implements `Driver`; CarpetX itself also REQUIRES Arith,
CarpetXRegrid and Loop. Left out as OPTIONAL: ADIOS2, openPMD_api and Silo
(CarpetX output formats beyond its own TSV and AMReX plotfiles), CMake
(AMReX, yaml_cpp), hwloc (MPI), CUDA (CarpetX; a flesh and
optionlist capability, no thorn), and the release's checkout-only CarpetX
thorns Algo, PDESolvers and PoissonX.

**Built from source:** with `mdb/cactup-tutorial/optionlists/default.toml`,
MPI, yaml-cpp and zlib come from Debian, so AMReX and NSIMD (which Debian
doesn't package) are the libraries ExternalLibraries builds into
`scratch/external`. That optionlist's `OPENPMD_DIR = "BUILD"` is unused with
this list (the thorn is not in it). Defining `SIMD_DISABLE` in the machine's
optionlist would have saved building NSIMD, but for every thornlist built on
the machine, which a real site wouldn't do.

## A short linear-wave run (for notebook 2's parameter file)

Cottonmouth ships a stock test that is exactly this:
`arrangements/Cottonmouth/CottonmouthZ4c4m/test/linear_wave_z4c.par`
(50 × 8 × 8 cells, RK4, 100 iterations, periodic; reference output in
`test/linear_wave_z4c/*.tsv`, `test.ccl` sets `ABSTOL 1e-10`). What a
parameter file needs, from the thorns' `param.ccl` and `schedule.ccl`:

- **ActiveThorns:** `ADMBaseX CarpetX CottonmouthLinearWaveID
  CottonmouthZ4c4m IOUtil ODESolvers TmunuBaseX`, as in the test. The
  inherited implementations must be listed; the flesh activates the
  providers of required capabilities (Arith, Loop, CarpetXRegrid, NewRadX)
  itself, which is why the stock test does not list them.
- **Initial data:** CottonmouthLinearWaveID fills ADMBaseX's variables in
  `ADMBaseX_InitialData`, so `ADMBaseX::initial_data`, `initial_lapse`,
  `initial_shift`, `initial_dtlapse` and `initial_dtshift` are all `"none"`.
  Its parameters: `CottonmouthLinearWaveID::amplitude` (default 1e-8) and
  `::wavelength` (1.0).
- **Evolution** (defaults in parentheses): `CottonmouthZ4c4m::`
  `dissipation_epsilon` (0.32; the test uses 0.02), `kappa_1` (0.02),
  `kappa_2` (0), `eta_beta` (2.0), `chi_floor`, `evolved_lapse_floor`;
  `apply_NewRadX` stays `no` on a periodic grid. `ODESolvers::method` (the
  test uses `"RK4"`), `CarpetX::dtfac` (the test uses 0.5).
- **Grid:** `CarpetX::xmin`…`zmax` (the test: −0.5 to 0.5),
  `ncells_x/y/z`, `blocking_factor_x/y/z = 1`, `ghost_size = 3`, `periodic`
  and `periodic_x/y/z = yes`; `Cactus::terminate` with `cctk_itlast` (or
  `cctk_final_time`).
- **Output:** `IO::out_dir`, `IO::out_every`; `CarpetX::out_tsv_vars` for 1D
  lines along the axes (the test: `CottonmouthZ4c4m::gt`, `::HamCons`,
  `::MomCons`); `CarpetX::out_tsv = no` (that one is 3D TSV);
  `CarpetX::out_norm_vars` for norms.

## `carpetx.th`

Max's `carpetx-mp.th` (a build-only list for CarpetX work, in his
`ET_2026_05_v0` installation) with the checkout sections cactup needs to
install from it, the release's repositories instead of the forks, and the
two fork-only thorns (`TestReal4`, `TestReal2`) left out. Its dependency
closure checks out with the same script. Requirements it follows:

| Thorn | Requires / uses |
|---|---|
| CarpetX | REQUIRES AMReX IOUtil MPI yaml_cpp zlib; OPTIONAL ADIOS2 openPMD_api Silo |
| Algo | REQUIRES Boost |
| Arith | OPTIONAL NSIMD yaml_cpp |
| AMReX | REQUIRES MPI; OPTIONAL CMake |
| MPI | OPTIONAL hwloc |
| Silo | REQUIRES HDF5 |
| HDF5, hwloc | REQUIRE zlib |
| openPMD | OPTIONAL ADIOS2 CMake HDF5 MPI |

## `carpetx-mp-forks.th`

Max's list, unchanged below its header apart from two comments. It is the
full release list, derived mechanically from `einsteintoolkit.th`, with the
nine repositories in the header table pointed at forks (URL and
`!REPO_BRANCH` only) and `CarpetX/TestReal4` and `CarpetX/TestReal2` added.
Cottonmouth stays at the stock `EinsteinToolkit/Cottonmouth`, `ET_2026_05`,
so `et-mp` keeps stock Cottonmouth after the refetch.

Kept from the original header, for the record:

- The mixed-precision changes were regression-clean for the CarpetX subset
  (57 tests passed, 0 failed, GPU-validated on an A100); the milestone-5
  acceptance bar was byte-identical regenerated bindings for every
  pre-existing thorn. A full-ET build of this list was not compile-tested
  end to end.
- The CarpetX fork's branch carries tags `mixed-precision-1` to `-5`.
- `TestReal2` needs `_Float16`: GCC 12 or newer on x86-64 (nvcc units use
  `__half`). LSU Deep Bayou had gcc 11.2 at most as of 2026-07, so it had to
  be disabled there.
- The ExternalLibraries forks exist because from-source MPI builds fail to
  configure AMReX, ADIOS2 and openPMD without MPI hints, and from-source
  hwloc plus MPI fail where libudev-dev is installed.

### Which repositories notebook 3 must repoint

The README says notebook 3 points the flesh and CarpetX at the forks. The
fork's CarpetX changes what NewRadX builds against, and the SpacetimeX fork
exists precisely to template `NewRadX_Apply` over the grid function's
precision; so stock NewRadX may not compile against the fork's CarpetX, and
repointing SpacetimeX too may be needed. `tutorial.th` keeps SpacetimeX's
fork basename, so either works. Confirmed by building (Stage 5): stock
NewRadX compiles against the fork's CarpetX, and the build of `tutorial.th`
with only the flesh and CarpetX repointed (plus `CarpetX/TestReal4`) succeeds
and runs TestReal4's `testreal4.par` to PASS. So notebook 3 repoints just the
two.

Also note that the forks list moves `ExternalLibraries-AMReX` (to
`mpi-cmake-hints`) and `ExternalLibraries-MPI` (to
`export-hwloc-in-mpi-libs`), both of which are in `tutorial.th`; they move
only if notebook 3 copies those sections too. The tutorial image uses
Debian's MPI, so neither fix is needed there.
