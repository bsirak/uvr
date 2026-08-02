use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Result, UvrError};

/// Top-level `uvr.toml` structure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub project: ProjectMeta,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, DependencySpec>,

    #[serde(
        rename = "dev-dependencies",
        default,
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub dev_dependencies: BTreeMap<String, DependencySpec>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<PackageSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectMeta {
    pub name: String,

    /// SemVer requirement, e.g. `">=4.0.0"`
    #[serde(default)]
    pub r_version: Option<String>,

    /// Explicit Bioconductor release, e.g. `"3.18"`.
    /// When omitted, auto-detected from the active R version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bioc_version: Option<String>,

    #[serde(default)]
    pub description: Option<String>,
}

/// Either a bare version string (`">=3.0.0"`, `"*"`) or a detailed table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum DependencySpec {
    Version(String),
    Detailed(DetailedDep),
}

impl DependencySpec {
    pub fn version_req(&self) -> Option<&str> {
        match self {
            DependencySpec::Version(v) => Some(v),
            DependencySpec::Detailed(d) => d.version.as_deref(),
        }
    }

    pub fn is_bioc(&self) -> bool {
        match self {
            DependencySpec::Version(_) => false,
            DependencySpec::Detailed(d) => d.bioc.unwrap_or(false),
        }
    }

    pub fn git(&self) -> Option<&str> {
        match self {
            DependencySpec::Detailed(d) => d.git.as_deref(),
            _ => None,
        }
    }
}

impl Default for DependencySpec {
    fn default() -> Self {
        DependencySpec::Version("*".to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct DetailedDep {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    /// `true` = Bioconductor package
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bioc: Option<bool>,

    /// `"user/repo"` — GitHub source
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<String>,

    /// branch / tag / commit SHA
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PackageSource {
    pub name: String,
    pub url: String,
}

impl std::str::FromStr for Manifest {
    type Err = crate::error::UvrError;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        // First parse to Value so we can detect dotted-key traps before
        // serde silently discards them.
        let raw: toml::Value =
            toml::from_str(s).map_err(|e| crate::error::UvrError::ManifestParse(e.to_string()))?;

        // Validate [dependencies] and [dev-dependencies]: every value must be
        // a string (bare version) or a table whose keys are known DetailedDep
        // fields {version, bioc, git, rev}. A TOML table-header entry like
        // `[dependencies.data.table]` creates a nested table under key `data`
        // with a sub-key `table` — not a valid DetailedDep field. This check
        // catches that case before serde silently resolves the wrong package.
        //
        // NOTE: `VALID_DEP_KEYS` must list every field of `DetailedDep`. A
        // field added to that struct without updating this slice will cause
        // valid manifests to be rejected — keep them in sync.
        const VALID_DEP_KEYS: &[&str] = &["version", "bioc", "git", "rev"];

        for section in &["dependencies", "dev-dependencies"] {
            if let Some(toml::Value::Table(deps)) = raw.get(*section) {
                for (key, val) in deps {
                    if let toml::Value::Table(inner) = val {
                        // A valid DetailedDep table has only known field keys.
                        // Any other key signals a dotted-key trap: the user
                        // wrote `[dependencies.org.Hs.eg.db]` which TOML parsed
                        // as key=`org`, sub-table={Hs: {eg: {db: ...}}}.
                        //
                        // Walk the nested table chain to reconstruct the full
                        // dotted package name (e.g. `org.Hs.eg.db` not just
                        // `org.Hs`), so the error message shows the correct
                        // name to quote.
                        let unknown: Vec<&str> = inner
                            .keys()
                            .map(String::as_str)
                            .filter(|k| !VALID_DEP_KEYS.contains(k))
                            .collect();
                        if !unknown.is_empty() {
                            // Walk into nested tables to collect the full name.
                            // Stop when we reach a leaf or a valid DetailedDep.
                            let full_name = {
                                let mut parts = vec![key.as_str()];
                                let mut cur = inner;
                                loop {
                                    // Find the first non-DetailedDep key at this level
                                    let next = cur
                                        .keys()
                                        .map(String::as_str)
                                        .find(|k| !VALID_DEP_KEYS.contains(k));
                                    match next {
                                        Some(k) => {
                                            parts.push(k);
                                            match cur.get(k) {
                                                Some(toml::Value::Table(t)) => cur = t,
                                                _ => break,
                                            }
                                        }
                                        None => break,
                                    }
                                }
                                parts.join(".")
                            };
                            return Err(crate::error::UvrError::ManifestParse(format!(
                                "dependency `{key}` in [{section}] contains dotted sub-key(s): \
                                 {unknown:?}.\n\
                                 \n\
                                 This is caused by a dotted TOML key: \
                                 `[{section}.{full_name}]` is parsed by TOML as \
                                 package `{key}` with nested sub-keys, not as \
                                 package `{full_name}`.\n\
                                 \n\
                                 If you meant to declare package `{full_name}`, \
                                 use a quoted key in the flat [{section}] table:\n\
                                 \n\
                                 \t[{section}]\n\
                                 \t\"{full_name}\" = \"*\"\n\
                                 \n\
                                 Or run: uvr add {full_name}"
                            )));
                        }
                    }
                }
            }
        }

        toml::from_str(s).map_err(|e| crate::error::UvrError::ManifestParse(e.to_string()))
    }
}

impl Manifest {
    pub fn new(name: impl Into<String>, r_version: Option<String>) -> Self {
        Manifest {
            project: ProjectMeta {
                name: name.into(),
                r_version,
                bioc_version: None,
                description: None,
            },
            dependencies: BTreeMap::new(),
            dev_dependencies: BTreeMap::new(),
            sources: Vec::new(),
        }
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(path)?;
        s.parse()
    }

    /// Parse an R `DESCRIPTION` file (DCF format) into a `Manifest`.
    ///
    /// - `Imports:` → `dependencies`
    /// - `Suggests:` → `dev_dependencies`
    /// - `Depends: R (>= x.y.z)` → `project.r_version`
    /// - Non-R entries in `Depends:` are merged into `dependencies`
    /// - `Remotes:` entries override matching `Imports:` / `Depends:` entries
    ///   with git-source specs (github only for now).
    pub fn from_description_str(content: &str) -> Result<Self> {
        let fields = parse_dcf(content);

        let name = fields
            .get("Package")
            .map(|s| s.as_str())
            .unwrap_or("")
            .to_string();

        let r_version = fields
            .get("Depends")
            .and_then(|deps| parse_r_version_from_depends(deps));

        let mut dependencies = BTreeMap::new();
        let mut dev_dependencies = BTreeMap::new();

        if let Some(imports) = fields.get("Imports") {
            for (pkg, spec) in parse_dep_field(imports) {
                dependencies.insert(pkg, spec);
            }
        }
        if let Some(depends) = fields.get("Depends") {
            for (pkg, spec) in parse_dep_field(depends) {
                if pkg != "R" {
                    dependencies.insert(pkg, spec);
                }
            }
        }
        if let Some(suggests) = fields.get("Suggests") {
            for (pkg, spec) in parse_dep_field(suggests) {
                dev_dependencies.insert(pkg, spec);
            }
        }

        // Remotes override matching Imports/Depends/Suggests entries — they
        // declare the *source* for a dep already listed by name above. The
        // package name is derived from the repo URL by default, but R's
        // companion-package idiom often names the repo `<pkg>-r` while the
        // Imports/Suggests line uses `<pkg>` (#68). When the URL-derived
        // name doesn't match an existing dep, we strip common `-r` / `_r` /
        // `.r` suffixes before giving up.
        if let Some(remotes) = fields.get("Remotes") {
            for (url_pkg, spec) in parse_remotes_field(remotes) {
                let resolved = resolve_remote_pkg_name(&url_pkg, &dependencies, &dev_dependencies);
                let target = if dev_dependencies.contains_key(&resolved) {
                    &mut dev_dependencies
                } else {
                    &mut dependencies
                };
                // A Remotes entry declares the *source*, not the version —
                // carry any constraint from the Imports/Depends/Suggests
                // line into the git spec so the resolver's pre-resolved
                // check still has something to validate against (#132).
                let existing_version = target
                    .get(&resolved)
                    .and_then(|s| s.version_req())
                    .filter(|v| *v != "*")
                    .map(str::to_string);
                let spec = match spec {
                    DependencySpec::Detailed(d)
                        if d.version.is_none() && existing_version.is_some() =>
                    {
                        DependencySpec::Detailed(DetailedDep {
                            version: existing_version,
                            ..d
                        })
                    }
                    other => other,
                };
                target.insert(resolved, spec);
            }
        }

        Ok(Manifest {
            project: ProjectMeta {
                name,
                r_version,
                bioc_version: None,
                description: fields.get("Title").cloned(),
            },
            dependencies,
            dev_dependencies,
            sources: Vec::new(),
        })
    }

    pub fn from_description_file(path: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(path)?;
        Self::from_description_str(&s)
    }

    pub fn to_toml_string(&self) -> Result<String> {
        toml::to_string_pretty(self).map_err(UvrError::TomlSer)
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        let s = self.to_toml_string()?;
        atomic_write(path, s.as_bytes())
    }

    /// Add or update a dependency. Returns `true` if a new dep was added.
    pub fn add_dep(&mut self, name: String, spec: DependencySpec, dev: bool) -> bool {
        let map = if dev {
            &mut self.dev_dependencies
        } else {
            &mut self.dependencies
        };
        let new = !map.contains_key(&name);
        map.insert(name, spec);
        new
    }

    pub fn remove_dep(&mut self, name: &str) -> bool {
        let a = self.dependencies.remove(name).is_some();
        let b = self.dev_dependencies.remove(name).is_some();
        a || b
    }
}

/// Parse a DCF (Debian Control File) string into a `BTreeMap<field, value>`.
/// Delegates to the shared `dcf::parse_dcf_fields` implementation.
fn parse_dcf(content: &str) -> BTreeMap<String, String> {
    crate::dcf::parse_dcf_fields(content)
}

/// Parse a comma-separated R dependency field (Imports, Suggests, Depends).
/// Returns `(package_name, DependencySpec)` pairs, skipping blank entries.
fn parse_dep_field(field: &str) -> Vec<(String, DependencySpec)> {
    let mut result = Vec::new();
    for entry in field.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (name, spec) = if let Some(paren) = entry.find('(') {
            let name = entry[..paren].trim().to_string();
            let inner = entry[paren + 1..entry.rfind(')').unwrap_or(entry.len())].trim();
            // Convert ">=3.0.0" or ">= 3.0.0" → ">=3.0.0"
            let version: String = inner.chars().filter(|c| !c.is_whitespace()).collect();
            (name, DependencySpec::Version(version))
        } else {
            (entry.to_string(), DependencySpec::Version("*".to_string()))
        };
        if !name.is_empty() {
            result.push((name, spec));
        }
    }
    result
}

/// Pick the dependency name that a `Remotes:` URL should bind to.
///
/// 1. If the URL-derived name (e.g. `uvr-r` from `nbafrank/uvr-r`) already
///    matches a dep declared in Imports/Depends/Suggests, use that.
/// 2. Otherwise try common R companion-package suffixes (`-r`, `_r`, `.r`):
///    `uvr-r` → look for `uvr`. Strips one suffix at a time.
/// 3. Otherwise fall back to the URL-derived name (preserves prior behavior
///    when nothing better matches; the dep is then inserted as a new entry
///    with a git source).
fn resolve_remote_pkg_name(
    url_pkg: &str,
    dependencies: &BTreeMap<String, DependencySpec>,
    dev_dependencies: &BTreeMap<String, DependencySpec>,
) -> String {
    let known = |name: &str| dependencies.contains_key(name) || dev_dependencies.contains_key(name);
    if known(url_pkg) {
        return url_pkg.to_string();
    }
    for suffix in ["-r", "_r", ".r", "-R", "_R", ".R"] {
        if let Some(stripped) = url_pkg.strip_suffix(suffix) {
            if !stripped.is_empty() && known(stripped) {
                return stripped.to_string();
            }
        }
    }
    url_pkg.to_string()
}

/// Parse an R `Remotes:` field into `(package_name, DependencySpec)` pairs.
///
/// Supports devtools/remotes-style GitHub entries plus uvr's `forgejo::`
/// and `gitlab::`:
/// - `user/repo` → `git = "user/repo"`
/// - `user/repo@ref` → `git = "user/repo", rev = "ref"`
/// - `github::user/repo[@ref]` → same (explicit prefix)
/// - `pkgname=user/repo[@ref]` → explicit package name binding
/// - `forgejo::host/owner/repo[@ref]` → `git = "forgejo::host/owner/repo", rev = "ref"`
/// - `gitlab::host/group/.../project[@ref]` → `git = "gitlab::host/group/.../project", rev = "ref"`
///
/// Entries with other prefixes (`bitbucket::`, `git::`, `url::`,
/// `local::`, `bioc::`) are skipped for now — the caller keeps whatever
/// version-based spec it already had from `Imports:`.
///
/// Visible to the github registry so it can walk transitive `Remotes:`
/// chains during `uvr lock` (#84) — and to the forgejo/gitlab registries
/// for the same reason.
pub(crate) fn parse_remotes_field(field: &str) -> Vec<(String, DependencySpec)> {
    let mut result = Vec::new();
    for entry in field.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }

        // Three registries are translated today: github (the bare/`github::`
        // form, returning `git = "owner/repo"`), forgejo (the
        // `forgejo::host/owner/repo` form), and gitlab (the
        // `gitlab::host/group/.../project` form) — the latter two return
        // their prefix verbatim in `git` so the lock-time BFS can dispatch
        // on it. Other prefixes (`bitbucket::`, `git::`, `url::`,
        // `local::`, `bioc::`) are skipped — the caller keeps whatever
        // version-based spec it already had from `Imports:`.
        if let Some(body) = entry.strip_prefix("forgejo::") {
            if let Some(parsed) = parse_forgejo_entry(body) {
                result.push(parsed);
            }
            continue;
        }
        if let Some(body) = entry.strip_prefix("gitlab::") {
            if let Some(parsed) = parse_gitlab_entry(body) {
                result.push(parsed);
            }
            continue;
        }
        if let Some((prefix, _)) = entry.split_once("::") {
            if prefix != "github" {
                continue;
            }
        }
        let body = entry.strip_prefix("github::").unwrap_or(entry);

        // Optional `pkgname=` override.
        let (explicit_name, path) = match body.split_once('=') {
            Some((n, p)) if !n.trim().is_empty() && p.trim().contains('/') => {
                (Some(n.trim().to_string()), p.trim())
            }
            _ => (None, body),
        };

        // Split off `@ref` (branch/tag/SHA) and optional `#PR` (dropped).
        let (repo_path, rev) = match path.split_once('@') {
            Some((r, ref_)) => {
                let ref_ = ref_.split_once('#').map_or(ref_, |(r, _pr)| r).trim();
                let rev = if ref_.is_empty() {
                    None
                } else {
                    Some(ref_.to_string())
                };
                (r.trim(), rev)
            }
            None => (path.split_once('#').map_or(path, |(p, _pr)| p).trim(), None),
        };

        if !repo_path.contains('/') {
            continue;
        }
        let pkg_name = explicit_name.unwrap_or_else(|| {
            repo_path
                .rsplit_once('/')
                .map_or(repo_path, |(_, name)| name)
                .to_string()
        });
        if pkg_name.is_empty() {
            continue;
        }

        let spec = DependencySpec::Detailed(DetailedDep {
            git: Some(repo_path.to_string()),
            rev,
            ..Default::default()
        });
        result.push((pkg_name, spec));
    }
    result
}

/// Extract R version constraint from a `Depends:` field value.
/// e.g. `"R (>= 4.0.0), methods"` → `Some(">=4.0.0")`
fn parse_r_version_from_depends(depends: &str) -> Option<String> {
    for entry in depends.split(',') {
        let entry = entry.trim();
        if let Some(stripped) = entry.strip_prefix('R') {
            let rest = stripped.trim();
            if rest.is_empty() || rest.starts_with('(') {
                if let Some(paren) = entry.find('(') {
                    let inner = entry[paren + 1..entry.rfind(')').unwrap_or(entry.len())].trim();
                    let version: String = inner.chars().filter(|c| !c.is_whitespace()).collect();
                    if !version.is_empty() {
                        return Some(version);
                    }
                }
                return None;
            }
        }
    }
    None
}

/// Write `data` to `path` atomically via a temp file in the same directory.
/// Uses `tempfile::NamedTempFile` for a unique temp name, then renames.
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(data)?;
    tmp.persist(path)
        .map_err(|e| crate::error::UvrError::Io(e.error))?;
    Ok(())
}

/// Parse the `host/owner/repo[@ref]` body of a `forgejo::`-prefixed
/// `Remotes:` entry into `(pkg_name, DependencySpec)`. The package name
/// defaults to the repo segment. Returns `None` if the body is malformed.
///
/// The `git` field stores the *full* `"forgejo::host/owner/repo"` string
/// (prefix included) so downstream code that walks `Remotes:` chains can
/// tell forgejo and github specs apart by string prefix.
fn parse_forgejo_entry(body: &str) -> Option<(String, DependencySpec)> {
    // Optional `pkgname=` override.
    let (explicit_name, path) = match body.split_once('=') {
        Some((n, p)) if !n.trim().is_empty() && p.trim().contains('/') => {
            (Some(n.trim().to_string()), p.trim())
        }
        _ => (None, body),
    };

    let (path_no_anchor, rev) = match path.split_once('@') {
        Some((p, r)) => {
            let r = r.split('#').next().unwrap_or(r).trim();
            (
                p.trim(),
                if r.is_empty() {
                    None
                } else {
                    Some(r.to_string())
                },
            )
        }
        None => (path.split('#').next().unwrap_or(path).trim(), None),
    };

    // Validate the host/owner/repo shape through the shared parser so the
    // manifest accepts/rejects the same specs as the CLI and resolver (#108).
    // The pkgname= override and @ref/#anchor handling above are Remotes-field
    // syntax the shared parser doesn't know about, so we strip them first.
    let parsed = crate::registry::forgejo::parse_forgejo_parts(path_no_anchor)?;
    let pkg_name = explicit_name.unwrap_or_else(|| parsed.repo.clone());
    if pkg_name.is_empty() {
        return None;
    }

    let spec = DependencySpec::Detailed(DetailedDep {
        git: Some(format!(
            "forgejo::{}/{}/{}",
            parsed.host, parsed.owner, parsed.repo
        )),
        rev,
        ..Default::default()
    });
    Some((pkg_name, spec))
}

/// Parse the `host/group[/subgroup...]/project[@ref]` body of a
/// `gitlab::`-prefixed `Remotes:` entry into `(pkg_name, DependencySpec)`.
/// The package name defaults to the project segment. Returns `None` if the
/// body is malformed. Mirrors [`parse_forgejo_entry`] — the difference is
/// that GitLab's namespace path can nest arbitrarily deep, which
/// [`crate::registry::gitlab::parse_gitlab_parts`] already accounts for.
///
/// The `git` field stores the *full* `"gitlab::host/group/.../project"`
/// string (prefix included) so downstream code that walks `Remotes:`
/// chains can tell gitlab specs apart from github/forgejo ones by string
/// prefix.
fn parse_gitlab_entry(body: &str) -> Option<(String, DependencySpec)> {
    // Optional `pkgname=` override.
    let (explicit_name, path) = match body.split_once('=') {
        Some((n, p)) if !n.trim().is_empty() && p.trim().contains('/') => {
            (Some(n.trim().to_string()), p.trim())
        }
        _ => (None, body),
    };

    let (path_no_anchor, rev) = match path.split_once('@') {
        Some((p, r)) => {
            let r = r.split('#').next().unwrap_or(r).trim();
            (
                p.trim(),
                if r.is_empty() {
                    None
                } else {
                    Some(r.to_string())
                },
            )
        }
        None => (path.split('#').next().unwrap_or(path).trim(), None),
    };

    // Validate the host/group/.../project shape through the shared parser
    // so the manifest accepts/rejects the same specs as the CLI and
    // resolver. The pkgname= override and @ref/#anchor handling above are
    // Remotes-field syntax the shared parser doesn't know about, so we
    // strip them first.
    let parsed = crate::registry::gitlab::parse_gitlab_parts(path_no_anchor)?;
    let pkg_name = explicit_name.unwrap_or_else(|| parsed.project_name().to_string());
    if pkg_name.is_empty() {
        return None;
    }

    let spec = DependencySpec::Detailed(DetailedDep {
        git: Some(format!("gitlab::{}/{}", parsed.host, parsed.project_path)),
        rev,
        ..Default::default()
    });
    Some((pkg_name, spec))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[project]
name = "my-project"
r_version = ">=4.0.0"

[dependencies]
ggplot2 = ">=3.0.0"
dplyr = "*"

[dependencies.DESeq2]
bioc = true

[dependencies.myPkg]
git = "user/repo"
rev = "main"
"#;

    #[test]
    fn round_trip() {
        let m: Manifest = SAMPLE.parse().expect("parse");
        assert_eq!(m.project.name, "my-project");
        assert_eq!(m.project.r_version.as_deref(), Some(">=4.0.0"));

        let ggplot2 = m.dependencies.get("ggplot2").unwrap();
        assert!(matches!(ggplot2, DependencySpec::Version(v) if v == ">=3.0.0"));

        let deseq2 = m.dependencies.get("DESeq2").unwrap();
        assert!(deseq2.is_bioc());

        let my_pkg = m.dependencies.get("myPkg").unwrap();
        assert_eq!(my_pkg.git(), Some("user/repo"));

        // Re-serialize and re-parse
        let toml_str = m.to_toml_string().expect("serialize");
        let m2: Manifest = toml_str.parse().expect("reparse");
        assert_eq!(m, m2);
    }

    const DESCRIPTION_SAMPLE: &str = r#"Package: myanalysis
Title: My Analysis Project
Version: 0.1.0
Depends:
    R (>= 4.1.0),
    methods
Imports:
    ggplot2 (>= 3.4.0),
    dplyr,
    stringr
Suggests:
    testthat (>= 3.0.0),
    knitr
"#;

    #[test]
    fn description_basic() {
        let m = Manifest::from_description_str(DESCRIPTION_SAMPLE).expect("parse");
        assert_eq!(m.project.name, "myanalysis");
        assert_eq!(m.project.r_version.as_deref(), Some(">=4.1.0"));
        assert_eq!(
            m.project.description.as_deref(),
            Some("My Analysis Project")
        );
    }

    #[test]
    fn description_imports_as_deps() {
        let m = Manifest::from_description_str(DESCRIPTION_SAMPLE).expect("parse");
        // ggplot2 with version constraint
        let gg = m.dependencies.get("ggplot2").unwrap();
        assert!(matches!(gg, DependencySpec::Version(v) if v == ">=3.4.0"));
        // dplyr without version
        let dp = m.dependencies.get("dplyr").unwrap();
        assert!(matches!(dp, DependencySpec::Version(v) if v == "*"));
        // methods from Depends (non-R entry)
        assert!(m.dependencies.contains_key("methods"));
    }

    #[test]
    fn description_suggests_as_dev_deps() {
        let m = Manifest::from_description_str(DESCRIPTION_SAMPLE).expect("parse");
        let tt = m.dev_dependencies.get("testthat").unwrap();
        assert!(matches!(tt, DependencySpec::Version(v) if v == ">=3.0.0"));
        assert!(m.dev_dependencies.contains_key("knitr"));
    }

    #[test]
    fn description_no_r_in_deps() {
        let m = Manifest::from_description_str(DESCRIPTION_SAMPLE).expect("parse");
        assert!(!m.dependencies.contains_key("R"));
        assert!(!m.dev_dependencies.contains_key("R"));
    }

    #[test]
    fn description_remotes_override_imports_with_git_source() {
        let dcf = r#"Package: AQmap
Imports:
    airquality,
    dplyr,
    handyr
Remotes:
    B-Nilson/airquality,
    github::B-Nilson/handyr@dev,
    gitlab::other/thing,
    gitlab::gitlab.com/my-group/mypkg@v1.0,
    pkg=user/repo@v1.0
"#;
        let m = Manifest::from_description_str(dcf).expect("parse");

        // airquality: bare user/repo
        let aq = m.dependencies.get("airquality").unwrap();
        assert_eq!(aq.git(), Some("B-Nilson/airquality"));

        // handyr: github:: prefix + @ref
        let hr = m.dependencies.get("handyr").unwrap();
        assert_eq!(hr.git(), Some("B-Nilson/handyr"));
        if let DependencySpec::Detailed(d) = hr {
            assert_eq!(d.rev.as_deref(), Some("dev"));
        } else {
            panic!("handyr should be a detailed git dep");
        }

        // dplyr: not in Remotes, stays as "*"
        let dp = m.dependencies.get("dplyr").unwrap();
        assert!(matches!(dp, DependencySpec::Version(v) if v == "*"));

        // gitlab:: is parsed, but a real spec needs host/group/project at
        // minimum — "other/thing" has only two segments (host + project,
        // no group), so it fails validation and "thing" correctly does
        // not appear as a dep.
        assert!(!m.dependencies.contains_key("thing"));

        // gitlab:: with a valid host/group/project shape does parse.
        let ml = m.dependencies.get("mypkg").unwrap();
        assert_eq!(ml.git(), Some("gitlab::gitlab.com/my-group/mypkg"));
        if let DependencySpec::Detailed(d) = ml {
            assert_eq!(d.rev.as_deref(), Some("v1.0"));
        } else {
            panic!("mypkg should be a detailed git dep");
        }

        // pkg=user/repo → explicit pkg name with @ref
        let pk = m.dependencies.get("pkg").unwrap();
        assert_eq!(pk.git(), Some("user/repo"));
        if let DependencySpec::Detailed(d) = pk {
            assert_eq!(d.rev.as_deref(), Some("v1.0"));
        }
    }

    #[test]
    fn description_remotes_override_preserves_version_constraint() {
        // #132 — a Remotes entry declares the *source* for a dep, not its
        // version. The constraint from the Imports line must survive the
        // override so the resolver's pre-resolved check can still validate
        // the git version against it.
        let dcf = r#"Package: thing
Imports:
    foo (>= 2.0.0),
    bar
Remotes:
    user/foo,
    user/bar
"#;
        let m = Manifest::from_description_str(dcf).expect("parse");

        let foo = m.dependencies.get("foo").unwrap();
        assert_eq!(foo.git(), Some("user/foo"));
        assert_eq!(
            foo.version_req(),
            Some(">=2.0.0"),
            "Imports version constraint should survive the Remotes override"
        );

        // Unconstrained ("*") deps should not grow a version field.
        let bar = m.dependencies.get("bar").unwrap();
        assert_eq!(bar.git(), Some("user/bar"));
        assert!(bar.version_req().is_none());
    }

    #[test]
    fn description_remotes_companion_suffix_binds_to_runtime_dep() {
        // Companion #2 to the Suggests-side test: when the existing dep
        // came from `Imports:` (runtime), the git source should land on
        // dependencies, not dev_dependencies. Reviewer flagged this code
        // path as untested — different target-selection branch.
        let dcf = r#"Package: thing
Imports:
    uvr (>= 0.1.0)
Remotes:
    nbafrank/uvr-r
"#;
        let m = Manifest::from_description_str(dcf).expect("parse");
        assert!(
            !m.dependencies.contains_key("uvr-r"),
            "uvr-r should not be a runtime dep, got {:?}",
            m.dependencies
        );
        assert!(
            !m.dev_dependencies.contains_key("uvr"),
            "uvr should not have moved to dev-deps, got {:?}",
            m.dev_dependencies
        );
        let uvr = m
            .dependencies
            .get("uvr")
            .expect("uvr should remain a runtime dep");
        assert_eq!(uvr.git(), Some("nbafrank/uvr-r"));
    }

    #[test]
    fn description_remotes_companion_suffix_binds_to_existing_dep() {
        // #68 — `Remotes: nbafrank/uvr-r` paired with `Suggests: uvr` should
        // bind to the `uvr` dev-dep, not create a new `uvr-r` runtime dep.
        let dcf = r#"Package: templates
Suggests:
    uvr (>= 0.1.0)
Remotes:
    nbafrank/uvr-r
"#;
        let m = Manifest::from_description_str(dcf).expect("parse");

        // No spurious runtime dep was created.
        assert!(
            !m.dependencies.contains_key("uvr-r"),
            "uvr-r should not be a runtime dep, got {:?}",
            m.dependencies
        );

        // The Suggests entry survives as a dev-dep with git source merged in.
        let uvr = m
            .dev_dependencies
            .get("uvr")
            .expect("uvr should remain a dev-dep");
        assert_eq!(uvr.git(), Some("nbafrank/uvr-r"));
    }

    #[test]
    fn description_remotes_underscore_r_suffix_binds() {
        let dcf = r#"Package: thing
Imports:
    foo
Remotes:
    user/foo_r
"#;
        let m = Manifest::from_description_str(dcf).expect("parse");
        assert!(!m.dependencies.contains_key("foo_r"));
        assert_eq!(m.dependencies.get("foo").unwrap().git(), Some("user/foo_r"));
    }

    #[test]
    fn description_remotes_unmatched_falls_back_to_url_name() {
        // Remote name doesn't match anything in Imports/Suggests and doesn't
        // strip to a known dep — keep prior behavior (insert as new dep).
        let dcf = r#"Package: thing
Imports:
    foo
Remotes:
    user/totally-unrelated
"#;
        let m = Manifest::from_description_str(dcf).expect("parse");
        assert_eq!(
            m.dependencies.get("totally-unrelated").unwrap().git(),
            Some("user/totally-unrelated")
        );
        // foo stays unsourced.
        let foo = m.dependencies.get("foo").unwrap();
        assert!(foo.git().is_none(), "foo should remain version-only");
    }

    #[test]
    fn description_without_package_yields_empty_name() {
        let dcf = "Imports:\n    dplyr\n";
        let m = Manifest::from_description_str(dcf).expect("parse");
        assert_eq!(m.project.name, "");
    }

    #[test]
    fn bioc_version_round_trip() {
        let toml = r#"
[project]
name = "bioc-test"
r_version = ">=4.3.0"
bioc_version = "3.18"

[dependencies.DESeq2]
bioc = true
"#;
        let m: Manifest = toml.parse().expect("parse");
        assert_eq!(m.project.bioc_version.as_deref(), Some("3.18"));

        let serialized = m.to_toml_string().expect("serialize");
        let m2: Manifest = serialized.parse().expect("reparse");
        assert_eq!(m, m2);
    }

    #[test]
    fn bioc_version_omitted() {
        // bioc_version should be None when not specified (backward compat)
        let m: Manifest = SAMPLE.parse().expect("parse");
        assert!(m.project.bioc_version.is_none());
        // And not serialized
        let s = m.to_toml_string().expect("serialize");
        assert!(!s.contains("bioc_version"));
    }

    #[test]
    fn description_no_depends() {
        let content = "Package: minimal\nImports: ggplot2\n";
        let m = Manifest::from_description_str(content).expect("parse");
        assert_eq!(m.project.name, "minimal");
        assert!(m.project.r_version.is_none());
        assert!(m.dependencies.contains_key("ggplot2"));
    }

    #[test]
    fn add_remove_dep() {
        let mut m = Manifest::new("test", None);
        assert!(m.add_dep("ggplot2".into(), DependencySpec::Version("*".into()), false));
        assert!(!m.add_dep(
            "ggplot2".into(),
            DependencySpec::Version(">=3.0.0".into()),
            false
        ));
        assert!(m.remove_dep("ggplot2"));
        assert!(!m.remove_dep("ggplot2"));
    }

    #[test]
    fn parse_remotes_field_keeps_forgejo_and_gitlab() {
        let field = "forgejo::codefloe.com/pat-s/mypkg@v0.1.0, github::user/a, \
                     gitlab::other/x, gitlab::gitlab.com/my-group/my-sub/thing@v2.0";
        let v = parse_remotes_field(field);
        let names: Vec<&str> = v.iter().map(|(n, _)| n.as_str()).collect();
        // "other/x" has only two segments (host + project, no group) —
        // not a valid gitlab spec — so it's dropped, not because gitlab
        // is unsupported.
        assert_eq!(names, vec!["mypkg", "a", "thing"]);

        // The forgejo entry stores the full `forgejo::host/owner/repo` in
        // the `git` field, with the ref split into `rev`.
        match &v[0].1 {
            DependencySpec::Detailed(d) => {
                assert_eq!(d.git.as_deref(), Some("forgejo::codefloe.com/pat-s/mypkg"));
                assert_eq!(d.rev.as_deref(), Some("v0.1.0"));
            }
            other => panic!("expected Detailed, got {other:?}"),
        }

        // The gitlab entry stores the full `gitlab::host/group/.../project`
        // string (nested subgroup included), with the ref split into `rev`.
        match &v[2].1 {
            DependencySpec::Detailed(d) => {
                assert_eq!(
                    d.git.as_deref(),
                    Some("gitlab::gitlab.com/my-group/my-sub/thing")
                );
                assert_eq!(d.rev.as_deref(), Some("v2.0"));
            }
            other => panic!("expected Detailed, got {other:?}"),
        }
    }

    // --- Dotted-key trap detection ---

    #[test]
    fn dotted_key_single_dot_is_rejected() {
        // `[dependencies.data.table]` → TOML key `data`, sub-key `table`
        let toml = r#"
[project]
name = "test"
r_version = "4.5"

[dependencies.data.table]
version = "*"
"#;
        let err = toml.parse::<Manifest>().unwrap_err().to_string();
        assert!(
            err.contains("data") && err.contains("table"),
            "expected dotted-key error mentioning 'data' and 'table', got: {err}"
        );
        assert!(
            err.contains("dotted TOML key") || err.contains("sub-key"),
            "expected actionable error message, got: {err}"
        );
    }

    #[test]
    fn dotted_key_r_prefix_is_rejected() {
        // `[dependencies.R.utils]` → key `R`, sub-key `utils`
        let toml = r#"
[project]
name = "test"
r_version = "4.5"

[dependencies.R.utils]
version = "*"
"#;
        let err = toml.parse::<Manifest>().unwrap_err().to_string();
        assert!(
            err.contains("utils"),
            "expected mention of 'utils', got: {err}"
        );
    }

    #[test]
    fn dotted_key_bioc_flag_is_rejected() {
        // `[dependencies.org.Hs.eg.db]` with `bioc = true` silently drops flag.
        // The error must name the *full* package (`org.Hs.eg.db`), not just the
        // first two segments (`org.Hs`), so the user can follow the advice verbatim.
        let toml = r#"
[project]
name = "test"
r_version = "4.5"

[dependencies.org.Hs.eg.db]
bioc = true
"#;
        let err = toml.parse::<Manifest>().unwrap_err().to_string();
        assert!(
            err.contains("org.Hs.eg.db"),
            "error must name the full package 'org.Hs.eg.db', got: {err}"
        );
        assert!(
            err.contains("uvr add org.Hs.eg.db"),
            "error must suggest 'uvr add org.Hs.eg.db', got: {err}"
        );
    }

    #[test]
    fn dotted_key_four_segments_reconstructed() {
        // TxDb.Hsapiens.UCSC.hg38.knownGene — five segments, common Bioc annotation package.
        // The advice must name the full package, not just the first two segments.
        let toml = r#"
[project]
name = "test"
r_version = "4.5"

[dependencies.TxDb.Hsapiens.UCSC.hg38.knownGene]
bioc = true
"#;
        let err = toml.parse::<Manifest>().unwrap_err().to_string();
        assert!(
            err.contains("TxDb.Hsapiens.UCSC.hg38.knownGene"),
            "error must name the full package, got: {err}"
        );
        assert!(
            err.contains("uvr add TxDb.Hsapiens.UCSC.hg38.knownGene"),
            "error must suggest correct uvr add command, got: {err}"
        );
    }

    #[test]
    fn dotted_key_dev_dependencies_is_rejected() {
        // Same trap applies to [dev-dependencies]
        let toml = r#"
[project]
name = "test"
r_version = "4.5"

[dev-dependencies.shiny.test]
version = "*"
"#;
        let err = toml.parse::<Manifest>().unwrap_err().to_string();
        assert!(err.contains("shiny") || err.contains("test"), "got: {err}");
    }

    #[test]
    fn quoted_dot_package_names_are_accepted() {
        // Quoted keys `"data.table"`, `"R.utils"`, `"org.Hs.eg.db"` must parse fine.
        let toml = r#"
[project]
name = "test"
r_version = "4.5"

[dependencies]
"data.table" = "*"
"R.utils" = { version = "*" }
"shiny.i18n" = "*"

[dependencies."org.Hs.eg.db"]
bioc = true
"#;
        let m: Manifest = toml.parse().expect("quoted dot-names must parse");
        assert!(
            m.dependencies.contains_key("data.table"),
            "data.table missing"
        );
        assert!(m.dependencies.contains_key("R.utils"), "R.utils missing");
        assert!(
            m.dependencies.contains_key("shiny.i18n"),
            "shiny.i18n missing"
        );
        let org = m.dependencies.get("org.Hs.eg.db").unwrap();
        assert!(org.is_bioc(), "org.Hs.eg.db should be bioc=true");
    }

    #[test]
    fn table_header_syntax_with_valid_fields_is_accepted() {
        // `[dependencies.DESeq2]` with known DetailedDep fields must still work.
        let toml = r#"
[project]
name = "test"
r_version = "4.5"

[dependencies.DESeq2]
bioc = true

[dependencies.myPkg]
git = "user/repo"
rev = "main"
"#;
        let m: Manifest = toml.parse().expect("valid table-header syntax must parse");
        assert!(m.dependencies.get("DESeq2").unwrap().is_bioc());
        assert_eq!(
            m.dependencies.get("myPkg").unwrap().git(),
            Some("user/repo")
        );
    }
}
