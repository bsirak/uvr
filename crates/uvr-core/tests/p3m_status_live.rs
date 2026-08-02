//! Network-gated: the compiled-in platform table must still agree with Posit.
//!
//! `registry::p3m_status` reads the catalog at runtime, so drift no longer
//! breaks users — but the static table is what answers when the endpoint is
//! unreachable, and a stale offline answer is exactly the failure mode that is
//! invisible until someone is offline. This is the job that notices.
//!
//! Skipped by default so CI stays offline-stable. Run with:
//!     cargo test -p uvr-core --test p3m_status_live -- --ignored --nocapture

use uvr_core::registry::p3m::ppm_linux_codename;
use uvr_core::registry::p3m_status;

/// Slugs `detect_posit_distro_slug_from_os_release` can produce, paired with
/// the `(distribution, release)` the sysreqs catalog keys them by — which is
/// also how `/__api__/status` keys them.
const MAPPED: &[(&str, &str, &str)] = &[
    ("ubuntu-2004", "ubuntu", "20.04"),
    ("ubuntu-2204", "ubuntu", "22.04"),
    ("ubuntu-2404", "ubuntu", "24.04"),
    ("ubuntu-2604", "ubuntu", "26.04"),
    ("debian-11", "debian", "11"),
    ("debian-12", "debian", "12"),
    ("debian-13", "debian", "13"),
    ("rhel-7", "redhat", "7"),
    ("rhel-8", "redhat", "8"),
    ("rhel-9", "rockylinux", "9"),
    ("rhel-10", "rockylinux", "10"),
    ("opensuse-154", "opensuse", "15.4"),
    ("opensuse-155", "opensuse", "15.5"),
    ("opensuse-156", "opensuse", "15.6"),
];

async fn catalog() -> Vec<p3m_status::Distro> {
    let client = reqwest::Client::builder()
        .user_agent("uvr-test")
        .build()
        .unwrap();
    p3m_status::fetch(&client)
        .await
        .expect("the platform catalog must be reachable for this test")
}

#[tokio::test]
#[ignore = "requires network access to packagemanager.posit.co"]
async fn the_static_table_matches_the_live_catalog() {
    let distros = catalog().await;
    let mut wrong = Vec::new();
    for (slug, distribution, release) in MAPPED {
        let live = p3m_status::codename_for(&distros, distribution, release, "x86_64");
        let table = ppm_linux_codename(slug);
        if live != table {
            wrong.push(format!(
                "{slug} ({distribution}/{release}): table says {table:?}, catalog says {live:?}"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "the compiled-in table disagrees with Posit's catalog — it is what \
         answers when the endpoint is unreachable, so fix it:\n  {}",
        wrong.join("\n  ")
    );
}

#[tokio::test]
#[ignore = "requires network access to packagemanager.posit.co"]
async fn the_portable_repo_still_covers_both_architectures() {
    // The whole arm64 story rests on this: distros without a native arm64
    // build are routed here instead. If Posit ever drops arm64 from the
    // portable repo, those users go back to compiling and should hear it from
    // CI rather than from a slow install.
    let distros = catalog().await;
    for arch in ["x86_64", "arm64"] {
        assert_eq!(
            p3m_status::portable_codename(&distros, arch),
            Some("manylinux_2_28"),
            "no portable repo for {arch}"
        );
    }
}

#[tokio::test]
#[ignore = "requires network access to packagemanager.posit.co"]
async fn the_aliased_pairs_resolve_in_the_platform_catalog_too() {
    // The pairs `catalog_alias` rewrites are absent from *both* Posit
    // catalogs, not just the sysreqs one. If Posit ever starts publishing
    // rockylinux 8 or centos 9/10 the alias becomes unnecessary here, and if
    // it stops publishing the RHEL names the alias becomes wrong — either way
    // this is where it shows up.
    let distros = catalog().await;
    for (from, to, release) in [
        ("rockylinux", "redhat", "8"),
        ("centos", "redhat", "9"),
        ("centos", "redhat", "10"),
    ] {
        assert!(
            p3m_status::codename_for(&distros, from, release, "x86_64").is_none(),
            "{from}/{release} is published after all — the alias is now unnecessary"
        );
        assert!(
            p3m_status::codename_for(&distros, to, release, "x86_64").is_some(),
            "{to}/{release} is not published — the alias now points nowhere"
        );
    }
}

#[tokio::test]
#[ignore = "requires network access to packagemanager.posit.co"]
async fn some_distro_repo_carries_arm64() {
    // Guards the arch plumbing itself. If every entry lost its arm64 build,
    // `codename_for(.., "arm64")` would answer None everywhere and the arch
    // filter would look like it worked while quietly routing everyone to the
    // portable repo.
    let distros = catalog().await;
    let native: Vec<&str> = ["rockylinux/9", "rockylinux/10", "ubuntu/24.04"]
        .iter()
        .filter(|k| {
            let (d, r) = k.split_once('/').unwrap();
            p3m_status::codename_for(&distros, d, r, "arm64").is_some()
        })
        .copied()
        .collect();
    assert!(
        !native.is_empty(),
        "no distro repo reports an arm64 build; the arch field or its spelling changed"
    );
    eprintln!("distro repos with arm64 builds: {native:?}");
}
