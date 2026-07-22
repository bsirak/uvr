use semver::Version;
use tracing::debug;

use crate::error::{Result, UvrError};
use crate::lockfile::PackageSource;
use crate::manifest::DependencySpec;
use crate::registry::{Dep, PackageInfo};

/// A gitlab-sourced dependency declared in another package's `Remotes:`
/// field. `(dep_name, "gitlab::host/group/.../project", optional_ref)`.
pub type GitlabRemote = (String, String, Option<String>);

/// A validated GitLab spec. Unlike Forgejo/GitHub, GitLab projects
/// routinely live under nested groups (`group/subgroup/.../project`), so
/// `project_path` holds the full namespace path rather than a single
/// `owner` segment. `git_ref` is `None` when the spec carried no `@ref`
/// segment — callers default it as they see fit (e.g. the registry
/// resolver uses `"HEAD"`, the manifest/CLI parsers keep `None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitlabSpec {
    pub host: String,
    pub project_path: String,
    pub git_ref: Option<String>,
}

impl GitlabSpec {
    /// The project name — the last segment of `project_path`. Used as the
    /// default package name when DESCRIPTION carries no `Package:` field
    /// (mirrors forgejo/github behavior).
    pub fn project_name(&self) -> &str {
        self.project_path
            .rsplit_once('/')
            .map_or(self.project_path.as_str(), |(_, name)| name)
    }
}

/// Parse and validate `"[gitlab::]host/group[/subgroup...]/project[@ref]"`
/// into structured parts. Mirrors [`crate::registry::forgejo::parse_forgejo_parts`]
/// except the middle segment is a namespace *path* rather than a single
/// owner, since GitLab groups nest arbitrarily deep.
///
/// Accepts:
/// - `gitlab::gitlab.com/my-group/mypkg@v0.1.0`
/// - `gitlab.com/my-group/my-subgroup/mypkg` (no ref → `git_ref = None`)
/// - `git.local:3000/g/r` (port allowed)
///
/// Rejects:
/// - hosts containing a scheme (`https://...`)
/// - empty host or any empty path segment
/// - fewer than two path segments (need at least a namespace + project)
/// - host chars outside `[alnum].-:` or path-segment chars outside `[alnum].-_`
/// - refs containing whitespace or URL/git metacharacters (`& # ? * ...`),
///   per [`crate::registry::github::is_valid_git_ref`]
pub fn parse_gitlab_parts(spec: &str) -> Option<GitlabSpec> {
    let body = spec.strip_prefix("gitlab::").unwrap_or(spec);

    let (path_part, git_ref) = match body.rfind('@') {
        Some(at) => {
            let r = &body[at + 1..];
            let git_ref = if r.is_empty() {
                None
            } else {
                // Refs with `&`, `#`, `?`, whitespace, etc. would mis-parse
                // the registry API request — reject at parse time.
                if !crate::registry::github::is_valid_git_ref(r) {
                    return None;
                }
                Some(r.to_string())
            };
            (&body[..at], git_ref)
        }
        None => (body, None),
    };

    if path_part.contains("://") {
        return None;
    }

    let parts: Vec<&str> = path_part.split('/').collect();
    // host + at least a two-segment project path (namespace + project).
    if parts.len() < 3 {
        return None;
    }
    let host = parts[0];
    if host.is_empty() {
        return None;
    }
    // Host shape: letters, digits, dot, hyphen, optional :port. Path-segment
    // shape: letters, digits, dot, hyphen, underscore. Anything else is a
    // user error worth catching before we make a request.
    let host_ok = host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'));
    if !host_ok {
        return None;
    }
    let seg_ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    };
    if !parts[1..].iter().all(|s| seg_ok(s)) {
        return None;
    }

    Some(GitlabSpec {
        host: host.to_string(),
        project_path: parts[1..].join("/"),
        git_ref,
    })
}

/// Parse `"gitlab::host/group/.../project[@ref]"` into
/// `(host, project_path, ref)`, defaulting a missing ref to `"HEAD"`. Thin
/// wrapper over [`parse_gitlab_parts`] for the registry resolver / BFS,
/// which want a concrete ref to query.
pub fn parse_gitlab_spec(spec: &str) -> Option<(String, String, String)> {
    let p = parse_gitlab_parts(spec)?;
    Some((
        p.host,
        p.project_path,
        p.git_ref.unwrap_or_else(|| "HEAD".to_string()),
    ))
}

/// Look up a GitLab API token from the environment.
///
/// Lookup order:
/// 1. `UVR_GITLAB_TOKEN_<NORMALIZED_HOST>` — per-host.
/// 2. `UVR_GITLAB_TOKEN` — single token for users with one instance.
///
/// Host normalization: strip `:port`, uppercase, replace `.` and `-`
/// with `_`. E.g. `gitlab.com` → `GITLAB_COM`, `git.local:3000` →
/// `GIT_LOCAL`. Whitespace-only env values are treated as unset so a
/// shell that exports `UVR_GITLAB_TOKEN=` doesn't fail authenticated
/// requests with a literal empty bearer.
pub fn gitlab_token(host: &str) -> Option<String> {
    let host_no_port = host.split_once(':').map_or(host, |(h, _port)| h);
    let normalized: String = host_no_port
        .to_ascii_uppercase()
        .chars()
        .map(|c| if c == '.' || c == '-' { '_' } else { c })
        .collect();
    let per_host = format!("UVR_GITLAB_TOKEN_{normalized}");
    for var in [per_host.as_str(), "UVR_GITLAB_TOKEN"] {
        if let Ok(v) = std::env::var(var) {
            let t = v.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// Resolve a GitLab-hosted R package: fetch commit SHA, fetch DESCRIPTION,
/// build a tarball URL for the lockfile.
pub async fn resolve_gitlab_package(
    client: &reqwest::Client,
    host: &str,
    project_path: &str,
    git_ref: &str,
) -> Result<PackageInfo> {
    resolve_gitlab_package_with_remotes(client, host, project_path, git_ref)
        .await
        .map(|(info, _)| info)
}

/// Resolve a GitLab package and return its `Remotes:`-declared gitlab deps,
/// so callers can walk transitive gitlab→gitlab chains during `uvr lock`.
pub async fn resolve_gitlab_package_with_remotes(
    client: &reqwest::Client,
    host: &str,
    project_path: &str,
    git_ref: &str,
) -> Result<(PackageInfo, Vec<GitlabRemote>)> {
    // GitLab's `:id` path parameter accepts a URL-encoded `namespace/path`
    // in place of the numeric project ID — encode the full nested path
    // (`urlencoding::encode` turns `/` into `%2F`, which is what the
    // project-path form requires).
    let project_id = urlencoding::encode(project_path).into_owned();
    let commit_sha = fetch_commit_sha(client, host, &project_id, project_path, git_ref).await?;

    let desc_url = format!(
        "https://{host}/api/v4/projects/{project_id}/repository/files/DESCRIPTION/raw?ref={commit_sha}"
    );
    let mut desc_req = client
        .get(&desc_url)
        .header("User-Agent", concat!("uvr/", env!("CARGO_PKG_VERSION")));
    if let Some(tok) = gitlab_token(host) {
        desc_req = desc_req.header("Authorization", format!("Bearer {tok}"));
    }
    let desc_resp = desc_req.send().await?;
    if !desc_resp.status().is_success() {
        return Err(map_gitlab_error(
            desc_resp.status(),
            host,
            project_path,
            &commit_sha,
        ));
    }
    let desc_text = desc_resp.text().await?;

    let desc_fields = crate::dcf::parse_dcf_fields(&desc_text);
    let project_name = project_path
        .rsplit_once('/')
        .map_or(project_path, |(_, name)| name);
    let pkg_name = desc_fields
        .get("Package")
        .cloned()
        .unwrap_or_else(|| project_name.to_string());
    let pkg_version = desc_fields
        .get("Version")
        .cloned()
        .unwrap_or_else(|| "0.0.0".to_string());
    let version = Version::parse(&crate::resolver::normalize_version(&pkg_version))
        .unwrap_or_else(|_| Version::new(0, 0, 0));

    let requires = parse_description_deps(&desc_fields);
    let remotes = parse_gitlab_remotes(&desc_fields);

    let url = format!(
        "https://{host}/api/v4/projects/{project_id}/repository/archive.tar.gz?sha={commit_sha}"
    );

    debug!("GitLab {host}/{project_path}@{git_ref} → {pkg_name} {version} ({commit_sha})");

    Ok((
        PackageInfo {
            name: pkg_name,
            version,
            source: PackageSource::Gitlab {
                host: host.to_string(),
            },
            checksum: Some(format!("git:{commit_sha}")),
            requires,
            url,
            raw_version: None,
            system_requirements: None,
        },
        remotes,
    ))
}

async fn fetch_commit_sha(
    client: &reqwest::Client,
    host: &str,
    project_id: &str,
    project_path: &str,
    git_ref: &str,
) -> Result<String> {
    // Unlike Forgejo (whose `/commits/{ref}` 404s and needs a list-commits
    // workaround), GitLab's single-commit endpoint accepts a branch, tag, or
    // SHA directly in the path and returns one commit object. The ref sits
    // in path position here, so percent-encode it — a slashed branch name
    // (`feature/x`) must not be split into extra path segments, and refs
    // can reach here without going through `parse_gitlab_parts` (lockfile
    // revs, `Remotes:` fields), so encode defensively (mirrors forgejo's
    // #152 handling).
    let encoded_ref = urlencoding::encode(git_ref);
    let url =
        format!("https://{host}/api/v4/projects/{project_id}/repository/commits/{encoded_ref}");
    let mut req = client
        .get(&url)
        .header("User-Agent", concat!("uvr/", env!("CARGO_PKG_VERSION")))
        .header("Accept", "application/json");
    if let Some(tok) = gitlab_token(host) {
        req = req.header("Authorization", format!("Bearer {tok}"));
    }
    let resp = req.send().await?;

    if !resp.status().is_success() {
        return Err(map_gitlab_error(resp.status(), host, project_path, git_ref));
    }

    #[derive(serde::Deserialize)]
    struct CommitObj {
        id: String,
    }
    let body = resp.text().await?;
    let commit: CommitObj = serde_json::from_str(&body).map_err(|e| {
        UvrError::Other(format!(
            "GitLab {host}/{project_path}@{git_ref}: could not parse commit JSON ({e}). Body: {}",
            body.chars().take(200).collect::<String>()
        ))
    })?;
    Ok(commit.id)
}

fn map_gitlab_error(
    status: reqwest::StatusCode,
    host: &str,
    project_path: &str,
    ref_or_sha: &str,
) -> UvrError {
    match status.as_u16() {
        401 | 403 => UvrError::Other(format!(
            "GitLab returned {status} for {host}/{project_path}; \
             set UVR_GITLAB_TOKEN_<HOST> if the project is private."
        )),
        404 => UvrError::Other(format!(
            "GitLab project not found: {host}/{project_path}@{ref_or_sha}. \
             Check the spec and that the project exists."
        )),
        _ => UvrError::Other(format!(
            "GitLab error for {host}/{project_path}@{ref_or_sha}: HTTP {status}"
        )),
    }
}

fn parse_gitlab_remotes(
    desc_fields: &std::collections::BTreeMap<String, String>,
) -> Vec<GitlabRemote> {
    // Return ALL git-bearing entries — github, forgejo, and gitlab alike.
    // The lock-time BFS dispatches by prefix via `classify_git`, so a
    // gitlab package whose DESCRIPTION declares `Remotes: github::user/repo`
    // walks correctly into the github registry. Mirrors
    // `forgejo::parse_forgejo_remotes` / `github::parse_github_remotes`.
    let Some(remotes_field) = desc_fields.get("Remotes") else {
        return Vec::new();
    };
    crate::manifest::parse_remotes_field(remotes_field)
        .into_iter()
        .filter_map(|(name, spec)| match spec {
            DependencySpec::Detailed(d) => d.git.map(|repo| (name, repo, d.rev)),
            DependencySpec::Version(_) => None,
        })
        .collect()
}

/// Same shape as `github::parse_description_deps` — small enough that
/// reusing it via a shared helper isn't worth the cross-module coupling.
fn parse_description_deps(fields: &std::collections::BTreeMap<String, String>) -> Vec<Dep> {
    let mut deps = Vec::new();
    for field in &["Imports", "Depends"] {
        if let Some(value) = fields.get(*field) {
            for d in crate::registry::cran::parse_dep_field(value) {
                if !crate::resolver::is_base_package(&d.name) {
                    deps.push(Dep {
                        name: d.name,
                        constraint: d.req.as_ref().map(|r| r.to_string()),
                    });
                }
            }
        }
    }
    deps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_spec_happy() {
        let (host, project_path, git_ref) =
            parse_gitlab_spec("gitlab::gitlab.com/my-group/mypkg@v0.1.0").unwrap();
        assert_eq!(host, "gitlab.com");
        assert_eq!(project_path, "my-group/mypkg");
        assert_eq!(git_ref, "v0.1.0");
    }

    #[test]
    fn parse_spec_nested_subgroups() {
        let (host, project_path, git_ref) =
            parse_gitlab_spec("gitlab::gitlab.com/group/subgroup/mypkg@main").unwrap();
        assert_eq!(host, "gitlab.com");
        assert_eq!(project_path, "group/subgroup/mypkg");
        assert_eq!(git_ref, "main");

        let parsed = parse_gitlab_parts("gitlab::gitlab.com/group/subgroup/mypkg@main").unwrap();
        assert_eq!(parsed.project_name(), "mypkg");
    }

    #[test]
    fn parse_spec_default_ref() {
        let (_h, _p, git_ref) = parse_gitlab_spec("gitlab::gitlab.com/my-group/mypkg").unwrap();
        assert_eq!(git_ref, "HEAD");
    }

    #[test]
    fn parse_spec_with_port() {
        let (host, _, _) = parse_gitlab_spec("gitlab::git.local:3000/g/r").unwrap();
        assert_eq!(host, "git.local:3000");
    }

    #[test]
    fn parse_spec_accepts_unprefixed() {
        // Callers (lock.rs BFS) may strip the prefix before calling us.
        let parsed = parse_gitlab_spec("gitlab.com/my-group/mypkg@main").unwrap();
        assert_eq!(parsed.0, "gitlab.com");
        assert_eq!(parsed.2, "main");
    }

    #[test]
    fn parse_spec_rejects_scheme_in_host() {
        assert!(parse_gitlab_spec("gitlab::https://gitlab.com/g/r").is_none());
    }

    #[test]
    fn parse_spec_rejects_too_few_segments() {
        assert!(parse_gitlab_spec("gitlab::gitlab.com/onlyone").is_none());
        assert!(parse_gitlab_spec("gitlab::onlyhost").is_none());
    }

    #[test]
    fn parse_spec_rejects_empty_segments() {
        assert!(parse_gitlab_parts("gitlab:://g/r").is_none());
        assert!(parse_gitlab_parts("gitlab::gitlab.com//r").is_none());
        assert!(parse_gitlab_parts("gitlab::gitlab.com/g/").is_none());
        assert!(parse_gitlab_parts("gitlab::gitlab.com/g//r").is_none());
    }

    #[test]
    fn parts_ref_is_none_when_absent_head_when_via_spec() {
        // The shared core keeps "no ref" as None; the spec wrapper defaults it
        // to HEAD for the resolver. This is the distinction add/manifest rely on.
        let p = parse_gitlab_parts("gitlab::gitlab.com/my-group/mypkg").unwrap();
        assert_eq!(p.git_ref, None);
        assert_eq!(p.project_path, "my-group/mypkg");
        assert_eq!(
            parse_gitlab_spec("gitlab::gitlab.com/my-group/mypkg")
                .unwrap()
                .2,
            "HEAD"
        );

        let p = parse_gitlab_parts("gitlab::gitlab.com/my-group/mypkg@v1.0").unwrap();
        assert_eq!(p.git_ref.as_deref(), Some("v1.0"));
    }

    #[test]
    fn parts_validates_ref_chars() {
        // A ref with `&` would inject a second query parameter; `#`, `?`,
        // and whitespace break the URL too.
        assert!(parse_gitlab_parts("gitlab::gitlab.com/g/r@feat&x").is_none());
        assert!(parse_gitlab_parts("gitlab::gitlab.com/g/r@v1#frag").is_none());
        assert!(parse_gitlab_parts("gitlab::gitlab.com/g/r@a b").is_none());
        assert!(parse_gitlab_parts("gitlab::gitlab.com/g/r@x?y").is_none());
        // Legitimate ref shapes still pass, including slashed branches.
        assert_eq!(
            parse_gitlab_parts("gitlab::gitlab.com/g/r@main")
                .unwrap()
                .git_ref
                .as_deref(),
            Some("main")
        );
        assert_eq!(
            parse_gitlab_parts("gitlab::gitlab.com/g/r@v1.2.3")
                .unwrap()
                .git_ref
                .as_deref(),
            Some("v1.2.3")
        );
        assert_eq!(
            parse_gitlab_parts("gitlab::gitlab.com/g/r@feature/x")
                .unwrap()
                .git_ref
                .as_deref(),
            Some("feature/x")
        );
    }

    #[test]
    fn parts_validates_path_segment_chars() {
        // A segment with shell metacharacters is rejected.
        assert!(parse_gitlab_parts("gitlab::gitlab.com/my-group/my;rm -rf").is_none());
        assert!(parse_gitlab_parts("gitlab::gitlab.com/own$er/mypkg").is_none());
        // Underscores, dots, hyphens stay valid, in any segment.
        assert!(parse_gitlab_parts("gitlab::gitlab.com/my-group/my_pkg.v2").is_some());
        assert!(parse_gitlab_parts("gitlab::gitlab.com/a.b-c/sub_d/my_pkg.v2").is_some());
    }

    // All token lookup tests are combined into a single test to avoid races
    // from env-mutation across parallel test threads (std::env is global).
    #[test]
    fn token_lookup() {
        // Serialize with all other env-mutating tests (process-global env).
        let _env = crate::env_vars::env_lock();
        // --- sub-test: per-host var takes precedence over global ---
        let host = "lookup-test-host.example";
        let per_host_var = "UVR_GITLAB_TOKEN_LOOKUP_TEST_HOST_EXAMPLE";
        std::env::set_var(per_host_var, "host-specific");
        std::env::set_var("UVR_GITLAB_TOKEN", "global");
        assert_eq!(gitlab_token(host).as_deref(), Some("host-specific"));
        std::env::remove_var(per_host_var);
        assert_eq!(gitlab_token(host).as_deref(), Some("global"));
        std::env::remove_var("UVR_GITLAB_TOKEN");
        assert_eq!(gitlab_token(host), None);

        // --- sub-test: port is stripped before normalization ---
        std::env::set_var("UVR_GITLAB_TOKEN_GIT_LOCAL", "t");
        assert_eq!(gitlab_token("git.local:3000").as_deref(), Some("t"));
        std::env::remove_var("UVR_GITLAB_TOKEN_GIT_LOCAL");

        // --- sub-test: whitespace-only values are treated as unset ---
        std::env::set_var("UVR_GITLAB_TOKEN", "   ");
        assert_eq!(gitlab_token("any.host").as_deref(), None);
        std::env::remove_var("UVR_GITLAB_TOKEN");
    }

    #[test]
    fn parse_gitlab_remotes_keeps_all_git_bearing_entries() {
        // A gitlab package's DESCRIPTION may declare git-bearing Remotes
        // pointing at any registry. We pass them all through; the
        // lock-time BFS dispatches per-prefix via classify_git.
        let desc = "\
Package: x
Version: 0.1.0
Remotes: gitlab::gitlab.com/my-group/mypkg@v0.1.0,
    github::user/other,
    forgejo::codefloe.com/pat-s/skipme
";
        let fields = crate::dcf::parse_dcf_fields(desc);
        let remotes = parse_gitlab_remotes(&fields);
        let pairs: Vec<(&str, &str)> = remotes
            .iter()
            .map(|(n, g, _)| (n.as_str(), g.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("mypkg", "gitlab::gitlab.com/my-group/mypkg"),
                ("other", "user/other"),
                ("skipme", "forgejo::codefloe.com/pat-s/skipme"),
            ]
        );
        // The gitlab entry still carries its ref.
        assert_eq!(remotes[0].2.as_deref(), Some("v0.1.0"));
    }
}
