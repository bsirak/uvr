use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use flate2::read::GzDecoder;
use tracing::{debug, warn};

use crate::error::{Result, UvrError};
use crate::lockfile::PackageSource;
use crate::registry::cran::{parse_dcf_block, CranPackageEntry};
use crate::registry::PackageInfo;
use crate::resolver::PackageRegistry;

/// Bioconductor's own release metadata, including the `r_ver_for_bioc_ver`
/// pairing table that [`bioc_release_for_r`] vendors a copy of (#120).
const BIOC_CONFIG_URL: &str = "https://bioconductor.org/config.yaml";

/// How long a cached `config.yaml` is served before it is refetched.
/// Bioconductor releases twice a year and the file is ~13 KB, so a week is
/// both generous and cheap: at worst uvr runs on a mapping that is a few days
/// behind a release, which the hardcoded table was permanently.
const BIOC_CONFIG_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Map an R major.minor to its paired Bioconductor release, per Bioconductor's
/// own `r_ver_for_bioc_ver` table (https://bioconductor.org/config.yaml). Each
/// entry must name a release built for *that* R — installing a release paired
/// with a different R version pulls package sources written against the wrong R
/// C API, which fail to compile (e.g. S4Vectors' `PRENV`/`Rf_findVar` against
/// R 4.6 headers). The fallback is the newest known release, so a newer-than-
/// known R errs toward the latest API rather than a stale one.
///
/// NOTE: this table is the **offline fallback** only. It goes stale every ~6
/// months (that staleness is what #119 was), so resolution prefers the live
/// `config.yaml` — see [`release_for_r`]. Keep it in the tree anyway: it is
/// what makes `uvr lock` work on a machine with no network.
fn bioc_release_for_r(r_major: u64, r_minor: u64) -> &'static str {
    match (r_major, r_minor) {
        (4, 6) => "3.23",
        (4, 5) => "3.21",
        (4, 4) => "3.20",
        (4, 3) => "3.18",
        (4, 2) => "3.16",
        (4, 1) => "3.14",
        (4, 0) => "3.12",
        // Unknown/newer R: newest known release beats a hardcoded old one.
        _ => "3.23",
    }
}

/// The two `config.yaml` fields uvr reads.
struct BiocConfig {
    /// `(bioc_version, r_version)` pairs from `r_ver_for_bioc_ver`, in file
    /// order. Note the direction: the key is the Bioconductor release, the
    /// value the R it was built against, and the mapping is many-to-one (two
    /// Bioc releases per R year).
    pairs: Vec<(String, String)>,
    /// `devel_version` — the unreleased branch, excluded from resolution.
    devel: Option<String>,
}

impl BiocConfig {
    /// The newest *released* Bioconductor version paired with R
    /// `major.minor`, or `None` when the config has nothing to say about that R.
    ///
    /// Two Bioc releases share each R version; the newer one is the right
    /// answer (it's the one whose packages are current). `devel_version` is
    /// excluded — its packages are built against an unreleased branch and
    /// change under you, which is not what a lockfile should pin.
    fn release_for_r(&self, major: u64, minor: u64) -> Option<String> {
        let released = || {
            self.pairs
                .iter()
                .filter(|(bioc, _)| Some(bioc.as_str()) != self.devel.as_deref())
        };
        let newest = |it: &mut dyn Iterator<Item = &(String, String)>| -> Option<String> {
            it.filter_map(|(bioc, _)| version_key(bioc).map(|k| (k, bioc)))
                .max_by_key(|(k, _)| *k)
                .map(|(_, bioc)| bioc.clone())
        };

        if let Some(found) =
            newest(&mut released().filter(|(_, r)| version_key(r) == Some((major, minor))))
        {
            return Some(found);
        }

        // R newer than every version the config knows (a release uvr's cached
        // copy predates): the newest released Bioc is the closest match, same
        // intent as the vendored table's catch-all arm but with live data.
        let newest_r = released().filter_map(|(_, r)| version_key(r)).max()?;
        if (major, minor) > newest_r {
            return newest(&mut released());
        }
        None
    }
}

/// Parse an `X.Y[.Z]` version into a comparable `(major, minor)`. Used for
/// both R and Bioconductor versions, which share the shape.
fn version_key(v: &str) -> Option<(u64, u64)> {
    let mut it = v.trim().split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next().unwrap_or("0").parse().ok()?;
    Some((major, minor))
}

/// Parse the two `config.yaml` fields uvr needs out of the raw YAML.
///
/// Hand-parsed rather than pulled in behind a YAML dependency: the file uses
/// exactly two shapes we care about — a top-level `key: "value"` scalar and a
/// one-level-indented `"key": "value"` mapping — and both are trivially
/// recognizable. A malformed or restructured file yields empty/partial data,
/// which resolution treats as "no config" and falls back to the vendored table.
fn parse_bioc_config(text: &str) -> BiocConfig {
    let mut pairs = Vec::new();
    let mut devel = None;
    let mut in_map = false;

    for raw in text.lines() {
        // Trailing comments are common in this file ("# R switching to yearly
        // releases"). Splitting on `#` is safe here because every value we
        // read is a quoted version number, which never contains one.
        let line = raw.split('#').next().unwrap_or("");
        if line.trim().is_empty() {
            continue;
        }
        // Any unindented line ends the mapping block — including the `- "3.23"`
        // sequence items of the `versions:` list that follows it.
        if !line.starts_with([' ', '\t']) {
            in_map = line.starts_with("r_ver_for_bioc_ver:");
            if let Some(v) = line.strip_prefix("devel_version:") {
                let v = unquote(v);
                if !v.is_empty() {
                    devel = Some(v);
                }
            }
            continue;
        }
        if !in_map {
            continue;
        }
        let Some((bioc, r)) = line.split_once(':') else {
            continue;
        };
        let (bioc, r) = (unquote(bioc), unquote(r));
        if !bioc.is_empty() && !r.is_empty() {
            pairs.push((bioc, r));
        }
    }

    BiocConfig { pairs, devel }
}

/// Strip surrounding whitespace and YAML quoting from a scalar.
fn unquote(s: &str) -> String {
    s.trim().trim_matches(['"', '\'']).trim().to_string()
}

fn bioc_config_cache_path() -> PathBuf {
    // cache_dir_or_temp, not `.`: a HOME-less sandbox must not have a live-
    // fetched file land in the project directory (#161 — this site was added
    // after that sweep and reintroduced the exact pattern it removed).
    crate::env_vars::cache_dir_or_temp().join("bioc-config.yaml")
}

/// True when `path` exists and is younger than [`BIOC_CONFIG_TTL`]. An
/// unreadable mtime counts as stale — refetching a 13 KB file is cheaper than
/// resolving against a mapping of unknown age.
fn cache_is_fresh(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < BIOC_CONFIG_TTL)
}

/// Load Bioconductor's `config.yaml`: the cached copy while it is fresh,
/// otherwise a refetch. Returns `None` only when there is no cache *and* the
/// fetch fails — offline with a stale cache still resolves against real data.
async fn load_bioc_config(client: &reqwest::Client) -> Option<BiocConfig> {
    let path = bioc_config_cache_path();
    if cache_is_fresh(&path) {
        if let Ok(text) = std::fs::read_to_string(&path) {
            return Some(parse_bioc_config(&text));
        }
    }

    match fetch_bioc_config(client).await {
        Ok(text) => {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(&path, &text) {
                debug!(
                    "Could not cache {BIOC_CONFIG_URL} at {}: {e}",
                    path.display()
                );
            }
            Some(parse_bioc_config(&text))
        }
        Err(e) => {
            // Offline, or Bioconductor is down. A stale cache still beats the
            // vendored table; no cache at all falls through to it.
            debug!("Could not fetch {BIOC_CONFIG_URL}: {e}");
            std::fs::read_to_string(&path)
                .ok()
                .map(|text| parse_bioc_config(&text))
        }
    }
}

async fn fetch_bioc_config(client: &reqwest::Client) -> Result<String> {
    let resp = client
        .get(BIOC_CONFIG_URL)
        .send()
        .await?
        .error_for_status()?;
    Ok(resp.text().await?)
}

/// Resolve `r_version` against `config` if it has an answer, else against the
/// vendored table.
fn resolve_release(config: Option<&BiocConfig>, r_version: &str) -> String {
    let parts: Vec<&str> = r_version.split('.').collect();
    let major: u64 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(4);
    // Unparseable/missing minor → a value outside the table so it falls through
    // to the newest-known release, matching the "unrecognized → newest" intent
    // (rather than silently landing on a specific old release).
    let minor: u64 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(99);

    if let Some(release) = config.and_then(|c| c.release_for_r(major, minor)) {
        return release;
    }
    let fallback = bioc_release_for_r(major, minor);
    debug!("R {r_version} → Bioconductor {fallback} (vendored table)");
    fallback.to_string()
}

/// Map an R version string (e.g. `"4.4.1"`) to its paired Bioconductor release
/// (e.g. `"3.20"`), consulting Bioconductor's authoritative `config.yaml`
/// (cached on disk, refetched weekly) and falling back to the vendored table
/// when there is no network and no cache (#120).
///
/// Use this wherever a client is at hand — it is what keeps the mapping from
/// going stale between uvr releases. [`default_release_for_r`] is the
/// no-client variant.
pub async fn release_for_r(client: &reqwest::Client, r_version: &str) -> String {
    let config = load_bioc_config(client).await;
    resolve_release(config.as_ref(), r_version)
}

/// Map an R version string to its paired Bioconductor release without any
/// network access: the cached `config.yaml` if one was ever written (by a
/// previous `uvr lock`/`sync`), else the vendored table.
///
/// For sync callers such as the `uvr add` not-found diagnostic. Resolution
/// itself should use [`release_for_r`].
pub fn default_release_for_r(r_version: &str) -> String {
    let config = std::fs::read_to_string(bioc_config_cache_path())
        .ok()
        .map(|text| parse_bioc_config(&text));
    resolve_release(config.as_ref(), r_version)
}

/// Bioconductor ships packages in four parallel indexes. A software package
/// like DESeq2 may depend on a data/annotation package like GenomeInfoDbData,
/// so we fetch and merge all four.
struct BiocEntry {
    entry: CranPackageEntry,
    /// Sub-repo path fragment (e.g. `"bioc"`, `"data/annotation"`).
    subrepo: &'static str,
}

pub struct BiocRegistry {
    packages: HashMap<String, BiocEntry>,
    bioc_release: String,
}

impl BiocRegistry {
    /// Fetch the Bioconductor package index for the release matching `r_version`.
    pub async fn fetch(client: &reqwest::Client, r_version: &str) -> Result<Self> {
        let release = release_for_r(client, r_version).await;
        Self::fetch_release(client, &release).await
    }

    /// Fetch the Bioconductor package index for a specific release (e.g. `"3.18"`).
    ///
    /// Fetches software, data/annotation, data/experiment, and workflows sub-repos
    /// in parallel and merges them. Software wins on name conflicts.
    pub async fn fetch_release(client: &reqwest::Client, bioc_release: &str) -> Result<Self> {
        let (software, annotation, experiment, workflows) = tokio::join!(
            fetch_subrepo(client, bioc_release, "bioc", "bioc"),
            fetch_subrepo(client, bioc_release, "data-annotation", "data/annotation"),
            fetch_subrepo(client, bioc_release, "data-experiment", "data/experiment"),
            fetch_subrepo(client, bioc_release, "workflows", "workflows"),
        );

        // Software (bioc) is mandatory — fail if it can't be fetched at all.
        let software = software.map_err(|e| {
            UvrError::Other(format!(
                "Failed to fetch Bioconductor {bioc_release} software index: {e}"
            ))
        })?;

        let mut packages: HashMap<String, BiocEntry> = HashMap::new();

        for (entry_map, subrepo) in [
            (Some(software), "bioc"),
            (annotation.ok(), "data/annotation"),
            (experiment.ok(), "data/experiment"),
            (workflows.ok(), "workflows"),
        ] {
            let Some(map) = entry_map else {
                warn!("Bioconductor {bioc_release} {subrepo}: fetch failed, skipping");
                continue;
            };
            for (name, entry) in map {
                packages.entry(name).or_insert(BiocEntry { entry, subrepo });
            }
        }

        debug!(
            "Bioconductor {bioc_release}: {} packages (software + data + workflows)",
            packages.len()
        );

        Ok(BiocRegistry {
            packages,
            bioc_release: bioc_release.to_string(),
        })
    }

    /// The Bioconductor release version this registry was fetched for (e.g. `"3.18"`).
    pub fn release(&self) -> &str {
        &self.bioc_release
    }

    /// Whether this Bioconductor release contains a package with the given name
    /// (across software, data, and workflow sub-repos).
    pub fn contains(&self, name: &str) -> bool {
        self.packages.contains_key(name)
    }
}

/// Fetch a single Bioconductor sub-repo index. Honors HTTP conditional GET
/// via cached ETag / Last-Modified headers when a local cache exists.
async fn fetch_subrepo(
    client: &reqwest::Client,
    bioc_release: &str,
    cache_key_suffix: &str,
    subrepo_path: &str,
) -> Result<HashMap<String, CranPackageEntry>> {
    let cache_key = format!("bioc-{bioc_release}-{cache_key_suffix}");
    let cache_path = crate::registry::cran::cache_path_for(&cache_key);
    let has_cache = cache_path.exists();

    let url = format!(
        "https://bioconductor.org/packages/{bioc_release}/{subrepo_path}/src/contrib/PACKAGES.gz"
    );

    if has_cache {
        if let Some((etag, last_modified)) = crate::registry::cran::read_cache_meta(&cache_key) {
            let mut req = client.get(&url);
            if let Some(ref e) = etag {
                req = req.header("If-None-Match", e.as_str());
            }
            if let Some(ref lm) = last_modified {
                req = req.header("If-Modified-Since", lm.as_str());
            }
            if let Ok(resp) = req.send().await {
                if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
                    debug!("Bioc {bioc_release}/{subrepo_path}: HTTP 304, using cache");
                    let raw = std::fs::read_to_string(&cache_path)?;
                    return Ok(parse_bioc_text(&raw));
                } else if resp.status().is_success() {
                    let new_etag = resp
                        .headers()
                        .get("etag")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    let new_lm = resp
                        .headers()
                        .get("last-modified")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    let bytes = resp.bytes().await?;
                    let mut gz = GzDecoder::new(bytes.as_ref());
                    let mut text = String::new();
                    gz.read_to_string(&mut text)?;
                    if let Some(parent) = cache_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    // Only record the new ETag/Last-Modified once the data write
                    // succeeds — otherwise a later conditional GET could 304
                    // against stale/absent cache content.
                    if let Err(e) = std::fs::write(&cache_path, &text) {
                        warn!(
                            "Bioc {bioc_release}/{subrepo_path}: failed to write cache data to {}: {e}; not updating cache meta",
                            cache_path.display()
                        );
                    } else {
                        crate::registry::cran::write_cache_meta(
                            &cache_key,
                            new_etag.as_deref(),
                            new_lm.as_deref(),
                        );
                    }
                    return Ok(parse_bioc_text(&text));
                }
            }
            debug!("Bioc {bioc_release}/{subrepo_path}: conditional request failed, using cache");
            let raw = std::fs::read_to_string(&cache_path)?;
            return Ok(parse_bioc_text(&raw));
        }
        let raw = std::fs::read_to_string(&cache_path)?;
        return Ok(parse_bioc_text(&raw));
    }

    debug!("Downloading Bioconductor {bioc_release}/{subrepo_path} PACKAGES.gz...");
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        return Err(UvrError::Other(format!(
            "Failed to fetch Bioconductor {subrepo_path} index (HTTP {})",
            resp.status()
        )));
    }
    let new_etag = resp
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let new_lm = resp
        .headers()
        .get("last-modified")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = resp.bytes().await?;
    let mut gz = GzDecoder::new(bytes.as_ref());
    let mut text = String::new();
    gz.read_to_string(&mut text)?;

    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Only record the new ETag/Last-Modified once the data write succeeds —
    // otherwise a later conditional GET could 304 against stale/absent cache
    // content.
    if let Err(e) = std::fs::write(&cache_path, &text) {
        warn!(
            "Bioc {bioc_release}/{subrepo_path}: failed to write cache data to {}: {e}; not updating cache meta",
            cache_path.display()
        );
    } else {
        crate::registry::cran::write_cache_meta(&cache_key, new_etag.as_deref(), new_lm.as_deref());
    }

    Ok(parse_bioc_text(&text))
}

impl PackageRegistry for BiocRegistry {
    fn resolve_package(&self, name: &str, constraint: Option<&str>) -> Result<PackageInfo> {
        let bioc = self
            .packages
            .get(name)
            .ok_or_else(|| UvrError::PackageNotFound(name.to_string()))?;
        let entry = &bioc.entry;

        if let Some(c) = constraint {
            if c != "*" && !c.is_empty() {
                let req = crate::resolver::parse_version_req(c)?;
                if !crate::resolver::version_matches_req(&entry.version, &req) {
                    return Err(UvrError::NoMatchingVersion {
                        package: name.to_string(),
                        constraint: c.to_string(),
                    });
                }
            }
        }

        let url = format!(
            "https://bioconductor.org/packages/{}/{}/src/contrib/{}_{}.tar.gz",
            self.bioc_release, bioc.subrepo, entry.name, entry.raw_version
        );

        Ok(PackageInfo {
            name: entry.name.clone(),
            version: entry.version.clone(),
            source: PackageSource::Bioconductor,
            checksum: if entry.md5sum.is_empty() {
                None
            } else {
                Some(format!("md5:{}", entry.md5sum))
            },
            requires: entry.requires_as_deps(),
            url,
            raw_version: Some(entry.raw_version.clone()),
            system_requirements: entry.system_requirements.clone(),
        })
    }
}

fn parse_bioc_text(text: &str) -> HashMap<String, CranPackageEntry> {
    let mut packages = HashMap::new();
    for block in text.split("\n\n") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        if let Some(entry) = parse_dcf_block(block) {
            packages.insert(entry.name.clone(), entry);
        }
    }
    packages
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(name: &str, raw_version: &str, md5: &str) -> CranPackageEntry {
        let parts: Vec<u64> = raw_version
            .split(['.', '-'])
            .filter_map(|s| s.parse().ok())
            .collect();
        let version = semver::Version::new(
            parts.first().copied().unwrap_or(0),
            parts.get(1).copied().unwrap_or(0),
            parts.get(2).copied().unwrap_or(0),
        );
        CranPackageEntry {
            name: name.to_string(),
            version,
            raw_version: raw_version.to_string(),
            depends: vec![],
            imports: vec![],
            linking_to: vec![],
            md5sum: md5.to_string(),
            system_requirements: None,
            path: None,
            built: None,
        }
    }

    /// A trimmed but structurally faithful sample of
    /// https://bioconductor.org/config.yaml — same quoting, same trailing
    /// comments, same `versions:` sequence right after the mapping block (the
    /// thing a naive indent-based parser trips over).
    const CONFIG_YAML_FIXTURE: &str = r#"---
## Lines that must be updated when we release a new software version are
## indicated in comments below

output_dir: output

## CHANGE THIS WHEN WE RELEASE A VERSION:
release_version: "3.23"
r_version_associated_with_release: "4.6.0"
r_version_associated_with_devel: "4.6.0"

## CHANGE THIS WHEN WE RELEASE A VERSION:
devel_version: "3.24"

versions:
- "3.23"
- "3.24"

r_ver_for_bioc_ver:
  "2.11": "2.15" # R switching to yearly releases, BioC remaining the same
  "3.17": "4.3"
  "3.18": "4.3"
  "3.19": "4.4"
  "3.20": "4.4"
  "3.21": "4.5"
  "3.22": "4.5"
  "3.23": "4.6"
  "3.24": "4.6"
# UPDATE THIS when we release a version

release_dates:
  "3.23": "April 2026"
"#;

    #[test]
    fn config_yaml_parses_the_two_fields_we_need() {
        let cfg = parse_bioc_config(CONFIG_YAML_FIXTURE);
        assert_eq!(cfg.devel.as_deref(), Some("3.24"));
        // Only the r_ver_for_bioc_ver block is collected — not the
        // `versions:` sequence items, not the `release_dates:` block that
        // follows the terminating comment.
        assert_eq!(cfg.pairs.len(), 9);
        assert_eq!(cfg.pairs.first().unwrap().0, "2.11");
        assert!(cfg.pairs.iter().any(|(b, r)| b == "3.23" && r == "4.6"));
        assert!(!cfg.pairs.iter().any(|(b, _)| b == "April 2026"));
    }

    #[test]
    fn config_yaml_picks_latest_released_bioc_per_r() {
        // #120: two Bioc releases share each R, and the newer one is the
        // answer. The vendored table says R 4.5 → 3.21; the live config says
        // 3.22, and that staleness is exactly what this fixes.
        let cfg = parse_bioc_config(CONFIG_YAML_FIXTURE);
        assert_eq!(cfg.release_for_r(4, 5).as_deref(), Some("3.22"));
        assert_eq!(cfg.release_for_r(4, 4).as_deref(), Some("3.20"));
        assert_eq!(cfg.release_for_r(4, 3).as_deref(), Some("3.18"));
    }

    #[test]
    fn config_yaml_excludes_the_devel_release() {
        // R 4.6 pairs with both 3.23 (released) and 3.24 (devel). Devel must
        // never win: its packages are built against an unreleased branch and
        // change under a lockfile that pins them.
        let cfg = parse_bioc_config(CONFIG_YAML_FIXTURE);
        assert_eq!(cfg.release_for_r(4, 6).as_deref(), Some("3.23"));
    }

    #[test]
    fn config_yaml_newer_r_gets_newest_released() {
        // An R the cached config predates still resolves to the newest
        // released Bioc (not devel), mirroring the vendored table's catch-all.
        let cfg = parse_bioc_config(CONFIG_YAML_FIXTURE);
        assert_eq!(cfg.release_for_r(4, 7).as_deref(), Some("3.23"));
        assert_eq!(cfg.release_for_r(5, 0).as_deref(), Some("3.23"));
    }

    #[test]
    fn config_yaml_older_r_defers_to_the_table() {
        // R 4.0 predates every pair in the fixture, so the config has no
        // opinion and the vendored table answers.
        let cfg = parse_bioc_config(CONFIG_YAML_FIXTURE);
        assert_eq!(cfg.release_for_r(4, 0), None);
        assert_eq!(resolve_release(Some(&cfg), "4.0.5"), "3.12");
    }

    #[test]
    fn unusable_config_falls_back_to_the_table() {
        // A missing, truncated, or restructured config.yaml must not break
        // resolution — the vendored table is the safety net.
        assert_eq!(resolve_release(None, "4.5.1"), "3.21");
        let empty = parse_bioc_config("output_dir: output\n");
        assert!(empty.pairs.is_empty());
        assert_eq!(resolve_release(Some(&empty), "4.5.1"), "3.21");
    }

    #[test]
    fn default_release_for_r_reads_the_disk_cache() {
        // The sync (no-client) path serves whatever a previous lock/sync
        // cached, so `uvr add`'s diagnostic names the same release resolution
        // will use — and falls back to the table when nothing is cached.
        let _env = crate::env_vars::env_lock();
        let previous = std::env::var("UVR_CACHE_DIR").ok();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("UVR_CACHE_DIR", tmp.path());

        assert_eq!(default_release_for_r("4.5.1"), "3.21", "no cache → table");

        std::fs::write(tmp.path().join("bioc-config.yaml"), CONFIG_YAML_FIXTURE).unwrap();
        assert_eq!(default_release_for_r("4.5.1"), "3.22", "cache → live map");
        assert_eq!(default_release_for_r("4.6.0"), "3.23");

        match previous {
            Some(v) => std::env::set_var("UVR_CACHE_DIR", v),
            None => std::env::remove_var("UVR_CACHE_DIR"),
        }
    }

    #[test]
    fn bioc_release_mapping() {
        assert_eq!(bioc_release_for_r(4, 5), "3.21");
        assert_eq!(bioc_release_for_r(4, 4), "3.20");
        assert_eq!(bioc_release_for_r(4, 3), "3.18");
        assert_eq!(bioc_release_for_r(4, 2), "3.16");
        assert_eq!(bioc_release_for_r(4, 1), "3.14");
        assert_eq!(bioc_release_for_r(4, 0), "3.12");
    }

    #[test]
    fn bioc_release_fallback() {
        // Unknown R versions fall back to the newest known release, not a
        // stale one — a future R should err toward the latest Bioc API.
        assert_eq!(bioc_release_for_r(5, 0), "3.23");
        assert_eq!(bioc_release_for_r(3, 6), "3.23");
        // R 4.6 is now mapped explicitly to its paired release.
        assert_eq!(bioc_release_for_r(4, 6), "3.23");
    }

    #[test]
    fn resolve_missing_package() {
        let registry = BiocRegistry {
            packages: HashMap::new(),
            bioc_release: "3.20".to_string(),
        };
        let result = registry.resolve_package("NonExistentPkg", None);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_software_package_uses_bioc_subrepo() {
        let mut packages = HashMap::new();
        packages.insert(
            "DESeq2".to_string(),
            BiocEntry {
                entry: make_entry("DESeq2", "1.42.0", "abc123"),
                subrepo: "bioc",
            },
        );
        let registry = BiocRegistry {
            packages,
            bioc_release: "3.20".to_string(),
        };
        let info = registry.resolve_package("DESeq2", None).unwrap();
        assert_eq!(info.source, PackageSource::Bioconductor);
        assert!(info
            .url
            .contains("/3.20/bioc/src/contrib/DESeq2_1.42.0.tar.gz"));
        assert_eq!(info.checksum, Some("md5:abc123".to_string()));
    }

    #[test]
    fn resolve_annotation_package_uses_data_annotation_subrepo() {
        let mut packages = HashMap::new();
        packages.insert(
            "GenomeInfoDbData".to_string(),
            BiocEntry {
                entry: make_entry("GenomeInfoDbData", "1.2.13", ""),
                subrepo: "data/annotation",
            },
        );
        let registry = BiocRegistry {
            packages,
            bioc_release: "3.21".to_string(),
        };
        let info = registry.resolve_package("GenomeInfoDbData", None).unwrap();
        assert!(info
            .url
            .contains("/3.21/data/annotation/src/contrib/GenomeInfoDbData_1.2.13.tar.gz"));
        assert_eq!(info.checksum, None);
    }

    #[test]
    fn resolve_package_constraint_mismatch() {
        let mut packages = HashMap::new();
        packages.insert(
            "SummarizedExperiment".to_string(),
            BiocEntry {
                entry: make_entry("SummarizedExperiment", "1.30.0", ""),
                subrepo: "bioc",
            },
        );
        let registry = BiocRegistry {
            packages,
            bioc_release: "3.18".to_string(),
        };
        let result = registry.resolve_package("SummarizedExperiment", Some(">=2.0.0"));
        assert!(result.is_err());
    }

    #[test]
    fn release_returns_bioc_version() {
        let registry = BiocRegistry {
            packages: HashMap::new(),
            bioc_release: "3.20".to_string(),
        };
        assert_eq!(registry.release(), "3.20");
    }

    #[test]
    fn offline_resolution_maps_r_to_bioc() {
        // The no-config path (offline, empty cache). R 4.6 must map to its own
        // Bioc release (3.23), not the 4.5 fallback — pulling 3.21 here ships
        // R-4.5-API package sources that fail to compile against R 4.6 headers
        // (the S4Vectors PRENV/Rf_findVar bug).
        assert_eq!(resolve_release(None, "4.6.0"), "3.23");
        assert_eq!(resolve_release(None, "4.5.1"), "3.21");
        assert_eq!(resolve_release(None, "4.4.0"), "3.20");
        assert_eq!(resolve_release(None, "4.3"), "3.18");
        // Unparseable / partial versions fall through to the newest known
        // release (the minor fallback is deliberately outside the table).
        assert_eq!(resolve_release(None, "garbage"), "3.23");
        assert_eq!(resolve_release(None, "4"), "3.23");
        assert_eq!(resolve_release(None, "9.9.9"), "3.23");
    }

    #[test]
    fn contains_reports_membership() {
        // Empty registry contains nothing; non-empty path is exercised live.
        let registry = BiocRegistry {
            packages: HashMap::new(),
            bioc_release: "3.20".to_string(),
        };
        assert!(!registry.contains("DESeq2"));
    }
}
