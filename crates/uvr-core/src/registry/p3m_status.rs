//! Live P3M platform catalog — `GET /__api__/status`.
//!
//! Posit publishes the exact set of distributions it builds for, and for which
//! architectures, as one documented endpoint (described in the OpenAPI spec at
//! `/__api__/swagger/doc.json`, whose version tracks the server build). Each
//! entry names a distribution and release in the same vocabulary the sysreqs
//! catalog uses, the repo codename its binaries live under, and the
//! architectures that repo actually carries:
//!
//! ```text
//! name     distribution  release  binaryURL    sysReqs  binaries  arch
//! rhel8    redhat        8        centos8      true     true      x86_64
//! rhel9    rockylinux    9        rhel9        true     true      x86_64, arm64
//! sles156  sle           15.6     opensuse156  true     true      x86_64
//! jammy    ubuntu        22.04    jammy        true     true      x86_64
//! ```
//!
//! Two things follow from having this at runtime rather than in a table.
//!
//! **The arch dimension exists.** Only `rhel9`, `rhel10`, `noble`, `resolute`
//! and `manylinux_2_28` carry arm64 builds. P3M routes by the R User-Agent and
//! degrades to *source* when a repo has no build for the caller's
//! architecture — so an arm64 host pointed at `jammy` silently compiles
//! everything, while `manylinux_2_28` would have handed it arm64 binaries.
//! Nothing breaks, but nothing is fast either, and a hardcoded slug → codename
//! table cannot see the difference.
//!
//! **The table stops drifting.** Posit adds repos as distros ship and hides
//! them at EOL; both directions used to be silent.
//!
//! The static table in [`super::p3m::ppm_linux_codename`] remains the offline
//! answer. It is consulted only when this endpoint cannot be reached — never
//! to second-guess a live answer, because "Posit publishes no arm64 build for
//! this distro" is a *correct* negative that the table cannot express.

use serde::Deserialize;
use tracing::debug;

/// Host architecture in Posit's vocabulary. Their `arch` field says `arm64`
/// where Rust says `aarch64`.
pub fn posit_arch(rust_arch: &str) -> &str {
    match rust_arch {
        "aarch64" => "arm64",
        other => other,
    }
}

/// One platform entry from `/__api__/status`.
#[derive(Debug, Clone, Deserialize)]
pub struct Distro {
    /// Posit's own key, e.g. `rhel8`, `jammy`. Not always the repo name.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub os: String,
    /// Distribution in the sysreqs vocabulary: `redhat`, `rockylinux`, `sle`.
    #[serde(default)]
    pub distribution: String,
    /// Release as the catalog keys it: `8`, `15.6`, `22.04`.
    #[serde(default)]
    pub release: String,
    /// Repo codename to install binaries from — the `__linux__/<codename>`
    /// segment. Several entries share one: `rhel8` and `centos8` both serve
    /// from `centos8`.
    #[serde(default, rename = "binaryURL")]
    pub binary_url: String,
    #[serde(default)]
    pub binaries: bool,
    #[serde(default, rename = "sysReqs")]
    pub sys_reqs: bool,
    /// Architectures this repo has builds for, in Posit's spelling.
    #[serde(default)]
    pub arch: Vec<String>,
}

/// The portable repo's codename. It appears in the catalog as a distro like
/// any other — `distribution: "centos", release: "8"` — which makes
/// `(distribution, release)` ambiguous for real CentOS 8 hosts, and would let
/// a distro lookup return the portable repo without going through the glibc
/// floor check that guards it. Distro lookups exclude it by name.
const PORTABLE: &str = "manylinux_2_28";

impl Distro {
    fn serves(&self, arch: &str) -> bool {
        self.os == "linux" && self.binaries && self.arch.iter().any(|a| a == arch)
    }
}

#[derive(Debug, Deserialize)]
struct Status {
    #[serde(default)]
    distros: Vec<Distro>,
}

const STATUS_URL: &str = "https://packagemanager.posit.co/__api__/status";

/// The repo codename to use for `(distribution, release)` on `arch`, or
/// `None` when Posit publishes nothing usable there.
///
/// `None` is a real answer, not a lookup failure: it covers both "Posit does
/// not build for this distro" and "it does, but not for this architecture".
/// Both mean the caller should look at the portable repo instead.
pub fn codename_for<'a>(
    distros: &'a [Distro],
    distribution: &str,
    release: &str,
    arch: &str,
) -> Option<&'a str> {
    distros
        .iter()
        .filter(|d| d.binary_url != PORTABLE)
        .find(|d| d.distribution == distribution && d.release == release && d.serves(arch))
        .map(|d| d.binary_url.as_str())
}

/// The portable repo, if it has builds for `arch`. Posit publishes
/// `manylinux_2_28` for x86_64 and arm64; the glibc floor is the caller's
/// problem (see `ppm_manylinux_repo`).
pub fn portable_codename<'a>(distros: &'a [Distro], arch: &str) -> Option<&'a str> {
    distros
        .iter()
        .find(|d| d.binary_url == PORTABLE && d.serves(arch))
        .map(|d| d.binary_url.as_str())
}

/// Whether the catalog carries this `(distribution, release)` at all.
///
/// Distinguishes "Posit has never heard of this pair" — where the caller's
/// own table may still know an alias — from "Posit knows it and publishes no
/// binaries uvr can use here", which is an authoritative no.
///
/// Deliberately does *not* require `binaries`. Debian 10 is catalogued for
/// sysreqs with `binaries: false`; treating that as unknown would hand the
/// question back to the static table, which is exactly the second-guessing of
/// a catalog negative this module exists to prevent. The portable repo is
/// excluded for the same reason `codename_for` excludes it: it is published
/// under `centos`/`8` and would make that pair look known on its own.
pub fn is_known(distros: &[Distro], distribution: &str, release: &str) -> bool {
    distros.iter().any(|d| {
        d.binary_url != PORTABLE
            && d.os == "linux"
            && d.distribution == distribution
            && d.release == release
    })
}

/// Fetch the platform catalog, cached for the day alongside the package
/// indexes.
///
/// Returns `None` on any failure — unreachable, non-success, unparseable.
/// Callers fall back to the static table rather than losing binaries because
/// a status endpoint was down.
pub async fn fetch(client: &reqwest::Client) -> Option<Vec<Distro>> {
    let cache = cache_path();

    if let Ok(cached) = std::fs::read_to_string(&cache) {
        if let Ok(status) = serde_json::from_str::<Status>(&cached) {
            return Some(status.distros);
        }
        // A corrupt cache file should cost one refetch, not the whole feature.
        let _ = std::fs::remove_file(&cache);
    }

    debug!("Fetching P3M platform catalog from {STATUS_URL}");
    let body = match client.get(STATUS_URL).send().await {
        Ok(resp) => match resp.error_for_status() {
            Ok(resp) => resp.text().await.ok()?,
            Err(e) => {
                debug!("P3M status endpoint returned {e}; using the static table");
                return None;
            }
        },
        Err(e) => {
            debug!("P3M status endpoint unreachable ({e}); using the static table");
            return None;
        }
    };

    let status: Status = match serde_json::from_str(&body) {
        Ok(s) => s,
        Err(e) => {
            debug!("P3M status endpoint returned unparseable JSON ({e})");
            return None;
        }
    };
    if status.distros.is_empty() {
        return None;
    }

    // Cache only after a successful parse, matching the package-index cache.
    if let Some(parent) = cache.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&cache, &body);

    Some(status.distros)
}

fn cache_path() -> std::path::PathBuf {
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    crate::env_vars::cache_dir_or_temp().join(format!("p3m-status-{date}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim shape of six entries from the live endpoint (2026-07-31),
    /// chosen for the cases the lookups have to get right: an x86_64-only
    /// distro repo, one with arm64, a `binaries: false` entry, and the
    /// portable repo sharing `centos`/`8` with a real distro entry.
    const SAMPLE: &str = r#"{"distros":[
        {"name":"rhel8","os":"linux","distribution":"redhat","release":"8",
         "binaryURL":"centos8","sysReqs":true,"binaries":true,"hidden":false,
         "arch":["x86_64"]},
        {"name":"rhel9","os":"linux","distribution":"rockylinux","release":"9",
         "binaryURL":"rhel9","sysReqs":true,"binaries":true,"hidden":false,
         "arch":["x86_64","arm64"]},
        {"name":"jammy","os":"linux","distribution":"ubuntu","release":"22.04",
         "binaryURL":"jammy","sysReqs":true,"binaries":true,"hidden":false,
         "arch":["x86_64"]},
        {"name":"buster","os":"linux","distribution":"debian","release":"10",
         "binaryURL":"buster","sysReqs":true,"binaries":false,"hidden":true,
         "arch":["x86_64"]},
        {"name":"manylinux_2_28","os":"linux","distribution":"centos","release":"8",
         "binaryURL":"manylinux_2_28","sysReqs":false,"binaries":true,
         "hidden":false,"arch":["x86_64","arm64"]},
        {"name":"centos8","os":"linux","distribution":"centos","release":"8",
         "binaryURL":"centos8","sysReqs":true,"binaries":true,"hidden":true,
         "arch":["x86_64"]}
    ]}"#;

    fn sample() -> Vec<Distro> {
        serde_json::from_str::<Status>(SAMPLE).unwrap().distros
    }

    #[test]
    fn resolves_a_distro_repo_for_its_architecture() {
        let d = sample();
        assert_eq!(codename_for(&d, "redhat", "8", "x86_64"), Some("centos8"));
        assert_eq!(codename_for(&d, "rockylinux", "9", "x86_64"), Some("rhel9"));
        assert_eq!(codename_for(&d, "ubuntu", "22.04", "x86_64"), Some("jammy"));
    }

    #[test]
    fn declines_a_repo_that_has_no_build_for_this_architecture() {
        // The arm64 gap this module exists for: jammy is x86_64-only, so an
        // arm64 host must be told "no" here and sent to the portable repo,
        // which does have arm64 builds. The old slug table answered "jammy"
        // on both architectures and left arm64 users compiling everything.
        let d = sample();
        assert_eq!(codename_for(&d, "ubuntu", "22.04", "arm64"), None);
        assert_eq!(codename_for(&d, "rockylinux", "9", "arm64"), Some("rhel9"));
        assert_eq!(portable_codename(&d, "arm64"), Some("manylinux_2_28"));
    }

    #[test]
    fn declines_a_distro_that_has_no_binaries_at_all() {
        // Debian 10 is in the catalog for sysreqs but `binaries: false`.
        let d = sample();
        assert_eq!(codename_for(&d, "debian", "10", "x86_64"), None);
    }

    #[test]
    fn the_portable_repo_never_answers_a_distro_lookup() {
        // manylinux_2_28 is published as distribution `centos` release `8` —
        // the same key real CentOS 8 has. The sample lists it *before* the
        // centos8 entry on purpose: a plain scan would return whichever comes
        // first, so this passing on the live catalog today is luck, not
        // design. On arm64 it is not even luck: centos8 has no arm64 build,
        // so the portable entry is the only match and would be returned
        // without ever going through the glibc floor check that guards it.
        let d = sample();
        assert_eq!(codename_for(&d, "centos", "8", "x86_64"), Some("centos8"));
        assert_eq!(codename_for(&d, "centos", "8", "arm64"), None);
        // Reachable only through the front door, where the floor is checked.
        assert_eq!(portable_codename(&d, "arm64"), Some("manylinux_2_28"));
    }

    #[test]
    fn is_known_ignores_the_portable_repo() {
        let d = sample();
        assert!(is_known(&d, "centos", "8"), "the real centos8 entry");
        assert!(is_known(&d, "ubuntu", "22.04"));
        assert!(is_known(&d, "rockylinux", "9"));
        // Catalogued for sysreqs with binaries: false. Known — the catalog
        // has an opinion about it — so the static table does not get to
        // resurrect a repo Posit is declining.
        assert!(is_known(&d, "debian", "10"));
        assert_eq!(codename_for(&d, "debian", "10", "x86_64"), None);
        // Never catalogued at all: the table may still know an alias.
        assert!(!is_known(&d, "ol", "8.10"));
    }

    #[test]
    fn unknown_distros_resolve_to_nothing() {
        let d = sample();
        assert_eq!(codename_for(&d, "ol", "8.10", "x86_64"), None);
        assert_eq!(codename_for(&d, "arch", "", "x86_64"), None);
    }

    #[test]
    fn posit_spells_aarch64_as_arm64() {
        assert_eq!(posit_arch("aarch64"), "arm64");
        assert_eq!(posit_arch("x86_64"), "x86_64");
    }
}
