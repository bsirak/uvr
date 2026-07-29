//! One parse of `/etc/os-release`, shared by everything that needs to know
//! which Linux this is.
//!
//! uvr asks that question for two unrelated reasons:
//!
//! - **Which P3M binaries to install** — a wrong answer installs packages
//!   linked against shared libraries that do not exist here, which fails at
//!   `library()` rather than at install time (#175).
//! - **Which system packages to check for** — a wrong answer produces a
//!   misleading hint, but nothing breaks.
//!
//! Those were answered by two separate parsers that disagreed: one required
//! `VERSION_ID` and returned `Option`, the other did not and fell back to a
//! hardcoded distro. Parsing lives here now so the *identity* is decided once
//! and each consumer only decides what to do about it.

/// The fields of `/etc/os-release` uvr cares about, unquoted.
///
/// Either field may be empty. Rolling releases ship no `VERSION_ID` at all —
/// Arch and CachyOS are the cases that matter here — so callers that need a
/// version must handle its absence rather than assume a default.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OsRelease {
    /// `ID`, lowercased (e.g. `ubuntu`, `debian`, `arch`).
    pub id: String,
    /// `VERSION_ID` verbatim (e.g. `22.04`, `12`, `3.21`). Empty on rolling
    /// releases.
    pub version_id: String,
}

impl OsRelease {
    /// Parse os-release content. Unknown/missing keys yield empty strings.
    pub fn parse(content: &str) -> Self {
        let mut out = OsRelease::default();
        for line in content.lines() {
            if let Some(val) = line.strip_prefix("ID=") {
                out.id = val.trim_matches('"').to_lowercase();
            } else if let Some(val) = line.strip_prefix("VERSION_ID=") {
                out.version_id = val.trim_matches('"').to_string();
            }
        }
        out
    }

    /// `true` when the file gave us nothing usable to identify the system.
    pub fn is_empty(&self) -> bool {
        self.id.is_empty()
    }
}

/// Read and parse `/etc/os-release`. `None` when it is absent or unreadable
/// (a scratch container, a non-Linux host).
pub fn detect() -> Option<OsRelease> {
    let content = std::fs::read_to_string("/etc/os-release").ok()?;
    Some(OsRelease::parse(&content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_id_and_version() {
        let os = OsRelease::parse("NAME=\"Ubuntu\"\nID=ubuntu\nVERSION_ID=\"22.04\"\n");
        assert_eq!(os.id, "ubuntu");
        assert_eq!(os.version_id, "22.04");
    }

    #[test]
    fn lowercases_the_id() {
        // os-release says IDs are lowercase, but not every distro complies;
        // both consumers used to normalize differently.
        assert_eq!(OsRelease::parse("ID=Debian\n").id, "debian");
    }

    #[test]
    fn rolling_release_has_no_version() {
        // Arch / CachyOS: the case that broke both consumers, differently.
        let os = OsRelease::parse("NAME=\"Arch Linux\"\nID=arch\n");
        assert_eq!(os.id, "arch");
        assert_eq!(os.version_id, "");
        assert!(!os.is_empty(), "an id with no version still identifies it");
    }

    #[test]
    fn id_like_is_not_mistaken_for_id() {
        // `ID_LIKE=arch` must not be read as `ID`, or a derivative would be
        // matched against rules meant for its parent.
        let os = OsRelease::parse("ID=cachyos\nID_LIKE=arch\n");
        assert_eq!(os.id, "cachyos");
    }

    #[test]
    fn empty_content_identifies_nothing() {
        assert!(OsRelease::parse("").is_empty());
    }
}
