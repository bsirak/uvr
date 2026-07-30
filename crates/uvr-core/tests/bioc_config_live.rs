//! Network-gated: the live R↔Bioconductor mapping must still parse, and must
//! still beat the vendored fallback table.
//!
//! `bioc_release_for_r`'s table goes stale every ~6 months — that staleness is
//! what #119/#120 were. Resolution now reads Bioconductor's own config.yaml,
//! so this test guards the thing that can break silently: the file's shape.
//! A restructured config.yaml would parse to nothing and fall back to the
//! stale table without any error surfacing.
//!
//! Skipped by default so CI stays offline-stable. Run with:
//!     cargo test -p uvr-core --test bioc_config_live -- --ignored --nocapture

use uvr_core::registry::bioconductor::release_for_r;

#[tokio::test]
#[ignore = "requires network"]
async fn live_config_yaml_maps_current_r_versions() {
    // Point the cache at an empty dir so this really fetches rather than
    // reading whatever a previous run left in ~/.uvr/cache.
    let tmp = tempfile::tempdir().expect("tempdir");
    std::env::set_var("UVR_CACHE_DIR", tmp.path());

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("client");

    // R 4.5 pairs with Bioc 3.21 and 3.22; the released newer one wins. The
    // vendored table still says 3.21, so this assertion also proves the live
    // map is what answered.
    let r45 = release_for_r(&client, "4.5.1").await;
    eprintln!("R 4.5.1 -> Bioconductor {r45}");
    assert_eq!(r45, "3.22");

    // R 4.6 pairs with 3.23 (released) and 3.24 (devel) — devel must not win.
    let r46 = release_for_r(&client, "4.6.0").await;
    eprintln!("R 4.6.0 -> Bioconductor {r46}");
    assert_eq!(r46, "3.23");

    assert!(
        tmp.path().join("bioc-config.yaml").exists(),
        "config.yaml must be cached for offline reuse"
    );
}
