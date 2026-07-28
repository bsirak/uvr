//! Network-gated test: resolve a real public GitLab repo end-to-end.
//!
//! Skipped by default. Run with:
//!     cargo test -p uvr-core --test gitlab_live -- --ignored

use uvr_core::registry::gitlab::resolve_gitlab_package;

#[tokio::test]
#[ignore = "requires network access to gitlab.com"]
async fn resolve_public_gitlab_repo() {
    let client = reqwest::Client::builder()
        .user_agent("uvr-test")
        .build()
        .expect("build client");

    // gitlab.com/r-packages/raven is a small public R package hosted on
    // gitlab.com. The test exercises the API surface (commit lookup,
    // DESCRIPTION fetch, archive URL construction), not this specific repo.
    let info = resolve_gitlab_package(&client, "gitlab.com", "r-packages/raven", "main")
        .await
        .expect("resolve");

    assert_eq!(info.name, "raven");
    assert!(
        info.url
            .starts_with("https://gitlab.com/api/v4/projects/r-packages%2Fraven/"),
        "archive URL pinned to /api/v4/projects/<encoded-path>/: {}",
        info.url
    );
    assert!(
        info.url.contains("/archive.tar.gz?sha="),
        "archive URL targets archive.tar.gz with a pinned sha: {}",
        info.url
    );
}
