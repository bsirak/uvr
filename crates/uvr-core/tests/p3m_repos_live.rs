//! Network-gated: every P3M Linux repo uvr maps must still exist.
//!
//! The distro list drifts — Posit adds repos as distros ship (Debian 13,
//! Ubuntu 26.04, RHEL 10) and retires them at EOL. Both directions hurt
//! silently: an unmapped repo downgrades those users to source builds, and a
//! mapped-but-gone repo makes every install 404 into the source fallback.
//!
//! Skipped by default so CI stays offline-stable. Run with:
//!     cargo test -p uvr-core --test p3m_repos_live -- --ignored

use uvr_core::registry::p3m::{ppm_linux_codename, ppm_linux_repo};

/// Every slug uvr maps, and the repo it must resolve to. Cross-checked against
/// the distro dropdown at
/// <https://packagemanager.posit.co/client/#/repos/cran/setup>.
const MAPPED: &[(&str, &str)] = &[
    // Currently offered in the dropdown.
    ("ubuntu-2204", "jammy"),
    ("ubuntu-2404", "noble"),
    ("ubuntu-2604", "resolute"),
    ("debian-12", "bookworm"),
    ("debian-13", "trixie"),
    ("rhel-7", "centos7"),
    ("rhel-8", "centos8"),
    ("rhel-9", "rhel9"),
    ("rhel-10", "rhel10"),
    ("opensuse-156", "opensuse156"),
    // Retired from the dropdown but still served — kept so users on EOL
    // systems don't silently lose their binaries.
    ("ubuntu-2004", "focal"),
    ("debian-11", "bullseye"),
    ("opensuse-154", "opensuse154"),
    ("opensuse-155", "opensuse155"),
];

fn index_url(codename: &str) -> String {
    format!("https://packagemanager.posit.co/cran/__linux__/{codename}/latest/src/contrib/PACKAGES")
}

async fn serves(client: &reqwest::Client, codename: &str) -> bool {
    match client.get(index_url(codename)).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(e) => {
            eprintln!("{codename}: {e}");
            false
        }
    }
}

#[tokio::test]
#[ignore = "requires network access to packagemanager.posit.co"]
async fn every_mapped_repo_still_serves() {
    let client = reqwest::Client::builder()
        .user_agent("uvr-test")
        .build()
        .unwrap();
    let mut gone = Vec::new();
    for (slug, codename) in MAPPED {
        assert_eq!(
            ppm_linux_codename(slug),
            Some(*codename),
            "mapping for {slug} changed"
        );
        if !serves(&client, codename).await {
            gone.push(*codename);
        }
    }
    assert!(
        gone.is_empty(),
        "P3M no longer serves these repos uvr maps: {gone:?} — installs there \
         silently fall back to source. Drop the mapping or point it elsewhere."
    );
}

#[tokio::test]
#[ignore = "requires network access to packagemanager.posit.co"]
async fn the_portable_manylinux_repo_still_serves() {
    // The fallback for every distro Posit doesn't publish for (#175). If this
    // preview repo goes away, those users drop to source builds and should
    // find out from CI rather than from a 45-second install.
    let client = reqwest::Client::builder()
        .user_agent("uvr-test")
        .build()
        .unwrap();
    assert!(
        serves(&client, "manylinux_2_28").await,
        "the manylinux_2_28 repo is gone; unknown distros now build from source"
    );
}

#[test]
#[ignore = "requires network access to packagemanager.posit.co"]
fn an_unmapped_distro_degrades_to_manylinux() {
    // Host-dependent by design: on glibc >= 2.28 an unrecognized distro must
    // reach the portable repo rather than giving up on binaries.
    let resolved = ppm_linux_repo("arch");
    match uvr_core::r_version::downloader::ppm_manylinux_repo() {
        Some(repo) => assert_eq!(
            resolved,
            Some(repo),
            "unmapped distro did not degrade to the portable repo"
        ),
        None => assert_eq!(
            resolved, None,
            "no portable repo is usable here, so binaries must be skipped"
        ),
    }
}
