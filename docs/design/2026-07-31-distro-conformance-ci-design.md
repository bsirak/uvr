# Distro conformance CI — design

Status: proposed
Author: gdevenyi
Date: 2026-07-31
Motivating bugs: #209 (RHEL gets no system dependencies), #175 (wrong binary repo on Arch)

## Goal

Catch the class of bug #209 belongs to: **uvr identifies the host under a name some
third-party catalog doesn't use, and nothing notices until a user on that distro files
an issue.**

Concretely: compile and run uvr's full test suite natively inside every Linux
distribution uvr claims to support, and assert the three distro-keyed mappings against
the images themselves rather than against what we assume they report.

## The bug class

uvr maps `/etc/os-release` onto three *independent* third-party vocabularies:

| Axis | Code path | Vocabulary | Coverage before this work |
|---|---|---|---|
| R interpreter build | libc + arch detection | manylinux / musllinux | good — `test`, `test-musl`, `test-musl-arm64` |
| P3M binary packages | `detect_posit_distro_slug_from_os_release` → `ppm_linux_codename` | PPM codename (`jammy`, `rhel9`) | partial — `test-p3m-repos`, `test-distros` (4 images) |
| System dependencies | `detect_linux_distro` → Posit sysreqs API + vendored rules | Posit sysreqs (`redhat`, `rockylinux`, `sle`) | **none** |

Axis 1 is distro-independent, which is why it has never broken this way. Axes 2 and 3
name the same host differently (`rhel-8` vs `redhat-8`), live in two separate match
statements (`r_version/downloader.rs:288` and `sysreqs.rs::normalize_distro`), and no
test compares them. #175 was axis 2. #209 was axis 3. There will be a third.

Two failure modes, and the design has to cover both:

- **Wrong name** — uvr asks under a name the catalog doesn't publish (`rhel` vs
  `redhat`). Detectable offline.
- **Wrong input** — uvr's idea of what a host reports doesn't match what it actually
  reports (`VERSION_ID=8.10`, not `8`). *Only a real image can say.* This is the half
  that let #209 survive: a hand-written fixture saying `VERSION_ID="8"` makes every
  offline test pass while production stays broken.

Everything below follows from that second point: **fixtures are captured, never
reasoned about.**

## What the images actually report

Captured 2026-07-31 with `ci/capture-os-release.sh`, and each resulting pair probed
against the live sysreqs API and the vendored rules. Findings that no desk-designed
matrix would have produced:

- **Oracle Linux is unmapped on both axes.** `oraclelinux:8` reports `ID="ol"`,
  `VERSION_ID="8.10"` → slug `ol-8.10` → no P3M codename, and `("ol","8.10")` → no
  local rules. Oracle users get neither binaries nor sysreqs. Same shape as #209,
  still open. Two lines fix it (`"ol"` alongside `"rhel"` in both tables).
- **The two sysreqs sources disagree about coverage.** The API serves `rockylinux` 9
  and 10 but *not* 8; it serves no `fedora` at any release, and no `alpine` — all of
  which the vendored rules do cover. So the table records `api` and `local` as separate
  booleans, and a test that treats `systems.json` as a proxy for what the API serves
  would be wrong in both directions.
- **CentOS Stream is covered after all.** The API rejects `centos` 9 and 10, but the
  vendored rules carry version-less `centos` constraints, so `resolve_local` returns
  `libxml2-devel` for both. Stream users get local-rule results with a degraded-check
  warning, not silence. (This corrects an earlier reading of the gap.)
- **Arch in a container reports a version.** `archlinux:latest` sets
  `VERSION_ID="20260726.0.562117"`; a bare-metal Arch install sets none. The
  "rolling releases publish no version" reasoning in `detect_linux_distro` is right for
  the metal and wrong for the image, so Arch reaches the API as
  `arch`/`20260726.0.562117` rather than being skipped early. Harmless today — but it
  is asserted as `"*"` in the matrix rather than pinned, since it changes on every
  image rebuild.
- **`pick_sysreqs_installer` knows only apk/dnf/apt-get.** On openSUSE, SLES and Arch
  it falls through to `apt-get`, which isn't installed. `--install-system-deps` cannot
  work there. Surfaced by writing the suite, not by reading the code.

## Design

One captured table, three assertions, two lanes.

### The table — `crates/uvr-core/tests/distro_matrix.json`

29 entries. Every distro uvr supports, the image it is exercised in, its verbatim
os-release identity, and the expected output of all three axes:

```json
{
  "key": "rhel-8",
  "image": "registry.access.redhat.com/ubi8:latest",
  "lane": "pr",
  "os_release": { "id": "rhel", "version_id": "8.10" },
  "p3m": { "slug": "rhel-8", "codename": "centos8" },
  "sysreqs": { "distribution": "redhat", "release": "8", "api": true, "local": true }
}
```

`null` on `p3m.codename` or `false` on both sysreqs sources means *not covered*, and
carries a `gap` or `note` field explaining whether that is a decision or an open bug —
so "this distro is unsupported" is recorded in review rather than looking identical to
an oversight.

JSON rather than a Rust `const` (which `p3m_repos_live.rs` uses today) for one reason:
the GitHub Actions matrix is generated from it with `jq`, in a job that needs no Rust
toolchain and no compile step. The Rust tests read it with `include_str!` +
`serde_json`, both already dependencies.

### Lane A — offline conformance (every PR, < 1s, no network)

Three tests in `crates/uvr-core/tests/distro_conformance.rs`, all pure functions over
files already in the repo.

**A1 — sysreqs resolve.** For every entry with `sysreqs.local`, the normalized pair
must actually resolve against the vendored rules:

```rust
let (d, r) = normalize_distro(&e.os_release.id, &e.os_release.version_id);
assert_eq!((d.as_str(), r.as_str()), (e.sysreqs.distribution, e.sysreqs.release));
assert!(
    !sysreqs_rules::resolve_local("libxml2 (>= 2.6.3)", &d, &r).is_empty(),
    "{}: normalized to {d}/{r}, which matches no vendored rule",
    e.key
);
```

Asserting through `resolve_local` rather than against `systems.json` membership is
deliberate — it uses the crate's own version-matching semantics, so Alpine's
`3.21.7` → `3.21` truncation is exercised rather than false-failing. This is the test
that would have caught #209 on the day it was written.

**A2 — P3M.** Same shape: `detect_posit_distro_slug_from_os_release` must produce
`p3m.slug` and `ppm_linux_codename` must produce `p3m.codename`.

**A3 — reverse coverage.** Every distribution/version in `systems.json` and every slug
in `ppm_linux_codename` must be reachable from some table entry, or be on an explicit
ignore-list with a reason. The forward tests find *wrong* mappings; only this one finds
*missing* ones — it is what surfaces "the catalog publishes `rockylinux` but nothing
resolves to it".

### Lane B — the full suite, natively, in every distro

`ci/distro-suite.sh`, run once per image:

```
docker run --rm -v "$PWD:/work" -w /work <image> sh ci/distro-suite.sh
```

`docker run` rather than a `container:` job because `actions/checkout` runs a node
binary needing a glibc the musl and minimal images don't have. Checking out on the host
and mounting the tree in gives **one shape that works for all 29 images**.

Everything is compiled and executed with the distro's own toolchain against its own
libc. Cross-compiling or shipping a static binary would test the build host, which is
the opposite of the point.

Stages, in order:

1. **prereqs** — compiler, TLS roots, fontconfig, libxml2 *runtime*. The one place
   that knows package-manager dialects (apt/dnf/microdnf/zypper/apk/pacman/yum), so
   adding a distro means adding a JSON entry, not a line of YAML. Notably absent:
   libxml2's headers — their absence is what stage 6 tests.
2. **rust** — rustup, or the distro's own rustup on musl so the toolchain links
   against musl.
3. **test** — `cargo build --all` then `cargo test --all`. Most of the suite is
   distro-independent logic, but it is seconds once built and it is the only way to
   reach the parts that aren't: package-manager probing (`sysreqs::filter_missing`),
   shell activation, path and permission handling. `*_live.rs` stays `#[ignore]`d —
   catalog conformance is its own job and must not make this one flaky.
4. **r** — install R, then re-run the activation tests with it on PATH. They skip
   during `cargo test` because no R existed yet; shell-specific cases self-skip when
   their shell is absent, which is the common case in a minimal image.
5. **binary** — install and `library()` the smoke package. #175: a binary from the
   wrong distro's repo installs happily and only fails at load, so the assertion has
   to load it, not just install it.
6. **sysreqs** — the #209 regression test at the real end of the chain:

   ```sh
   UVR_INSTALL_SYSREQS=1 uvr add xml2 --no-binary
   uvr run check.R
   ```

   The image has libxml2's runtime but not its headers, so this compiles only if every
   link works: os-release parse → catalog naming → package-manager probe → installer
   dialect. Nothing greps a warning string; the build either succeeds or it doesn't.
   That is exactly what #209 broke — RHEL asked under a name Posit doesn't publish, got
   nothing, installed nothing, and the build failed.

   On zypper/pacman hosts, where `pick_sysreqs_installer` can't run the install, the
   stage asserts the *diagnosis* instead (uvr named `libxml2-devel`) and still fails if
   uvr printed "System dependency check skipped".

**Identity check.** After the suite, the workflow re-reads `/etc/os-release` from the
image and compares it to the matrix entry. Cheap, and it is what keeps Lane A honest —
a fixture nobody re-checks is how #209 survived.

### Lanes and triggers

| Trigger | Images | Why |
|---|---|---|
| every PR | Lane A only | free, catches everything offline-detectable |
| PR touching `sysreqs*.rs`, `p3m.rs`, `downloader.rs`, `os_release.rs`, the vendored rules, or the matrix | 6 `lane: "pr"` images | one per package-manager dialect plus musl: ubuntu-2204, rhel-8, rocky-9, opensuse-156, arch, alpine-321 |
| nightly cron | all 29 | upstream drift, catalog drift |
| `workflow_dispatch` | any subset by key | debugging a specific distro |

`fail-fast: false` — every distro is an independent question and one answer must not
suppress the other 28. `max-parallel: 12` keeps the nightly from monopolising the
runner pool.

**Nightly failures open an issue rather than turning the tree red.** `rockylinux:9` was
9.3 and is now 9.8; `redhat/ubi8` will one day report 8.11. Upstream images moving is a
maintenance ticket, not a broken build, and a red main branch that nobody caused is a
red branch everyone learns to ignore.

## Cost

Public repo, so standard-runner minutes are free; the constraints are wall clock and
the 20-job concurrency limit.

| Lane | Per job | Jobs | Wall clock |
|---|---|---|---|
| A | < 1s | 1 (folded into `cargo test`) | none |
| B, PR subset | ~8 min | 6 | ~8 min |
| B, nightly | ~8 min | 29 | ~25 min at `max-parallel: 12` |

Roughly 4 runner-hours nightly. No cargo cache is shared between distros on purpose:
objects linked against one libc must never be picked up by another, so each container
builds into its own `/tmp` target dir from scratch.

## Delivery

1. **`distro_matrix.json` + `ci/capture-os-release.sh` + Lane A + `uvr doctor --json`.**
   No CI cost, no network, closes the offline-detectable half. `doctor` currently prints
   the P3M slug inside coloured prose and says nothing about the sysreqs identity, so
   `--json` is what lets any later job assert on axis 3 without grepping ANSI output —
   and is independently the thing to paste into a bug report.
2. **`ci/distro-suite.sh` + `distro-suite.yml`, PR lane only** (6 images).
3. **Nightly lane + drift-to-issue** across all 29.
4. **`sysreqs_live.rs`**, folded into `test-p3m-repos` renamed `test-catalogs`: probe
   the live API for every entry marked `api: true`. Catches Posit adding or retiring
   releases, which the offline tests cannot see.

Bugs this design has already surfaced, before any of it runs in CI: the Oracle Linux
gap, the zypper/pacman installer gap, the `rockylinux` 8 API gap, and — from running
the suite locally — that `curl` is a package conflict on RHEL images shipping
`curl-minimal`, so tools must be requested by command rather than by package name.

## What this deliberately does not do

- **No VM-based testing.** Containers cover every axis here; a VM adds systemd and a
  kernel, neither of which uvr touches.
- **No prebuilt or cross-compiled test binaries.** Shipping one static binary to 29
  containers would be far faster and would test the build host's libc instead of the
  distro's — the exact substitution this whole design exists to prevent.
- **No auto-generation of the mapping tables from the catalogs.** Tempting, and wrong:
  the mapping encodes judgement (AlmaLinux takes Rocky's entries; SLES takes openSUSE's
  *binaries* but its own *sysreqs*) that a scraper would flatten. Tests that fail
  loudly are the right mechanism; codegen would just move the guess.
- **No arm64 distro matrix.** All three axes are arch-independent below the R-build
  layer, which `ubuntu-24.04-arm` already covers.
