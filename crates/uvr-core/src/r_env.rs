//! The isolated R environment a uvr project runs against.
//!
//! `uvr run` spawns R with a specific set of environment variables that point
//! R at the project's library and shadow anything the machine would otherwise
//! contribute. Shell activation (`uvr activate`) has to export *the same* set
//! into the user's shell, and inline-script runs need it too. Keeping the
//! construction here means those paths cannot drift apart — a variable added
//! for `run` is automatically exported by `activate`.

use std::path::{Path, PathBuf};

/// The OS path-list separator (`;` on Windows, `:` elsewhere).
pub const PATH_SEP: &str = if cfg!(target_os = "windows") {
    ";"
} else {
    ":"
};

/// A resolved, isolated R environment.
///
/// Construct it directly — every input is explicit so the result is a pure
/// function of its fields and can be asserted on in tests without touching
/// the real environment.
#[derive(Debug, Clone, PartialEq)]
pub struct REnv {
    /// Absolute path to the R binary this environment runs.
    pub r_binary: PathBuf,
    /// The project (or fallback) library packages are installed into.
    pub library: PathBuf,
    /// Ephemeral library for `--with` packages, searched *before* `library`.
    pub with_library: Option<PathBuf>,
    /// Raw `UVR_EXTRA_LIBS` value, appended last. See [`REnv::r_libs_user`].
    pub extra_libs: Option<String>,
}

impl REnv {
    /// Directory holding the R binary — what activation prepends to `PATH`
    /// so a bare `R` resolves to this interpreter.
    pub fn r_bin_dir(&self) -> PathBuf {
        self.r_binary
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default()
    }

    /// R's own `lib/` directory, i.e. `<r_binary>/../../lib`.
    ///
    /// Exported as `DYLD_LIBRARY_PATH` / `LD_LIBRARY_PATH` so compiled
    /// packages can find `libR` at runtime regardless of the install-name
    /// baked into the shared object.
    pub fn r_lib_dir(&self) -> PathBuf {
        self.r_binary
            .parent()
            .and_then(Path::parent)
            .map(|p| p.join("lib"))
            .unwrap_or_default()
    }

    /// The `R_LIBS_USER` search path.
    ///
    /// Order is significant: the `--with` ephemeral library wins over the
    /// project library, and `UVR_EXTRA_LIBS` comes last. The extra-libs
    /// escape hatch exists because `R_LIBS_SITE` is blanked below to isolate
    /// the project — without it a controlled environment (a Docker image, a
    /// shared lab machine, a benchmark harness) could not expose a system
    /// library to uvr at all.
    pub fn r_libs_user(&self) -> String {
        let mut out = match &self.with_library {
            Some(with_lib) => {
                format!("{}{PATH_SEP}{}", with_lib.display(), self.library.display())
            }
            None => self.library.to_string_lossy().into_owned(),
        };
        if let Some(extra) = self.extra_libs.as_deref() {
            let extra = extra.trim();
            if !extra.is_empty() {
                out.push_str(PATH_SEP);
                out.push_str(extra);
            }
        }
        out
    }

    /// Every environment variable that defines the isolated environment.
    ///
    /// This is the single source of truth: `uvr run` applies these to the R
    /// child process, `uvr activate` exports the same pairs into the shell.
    ///
    /// Note the deliberately-empty values — `R_LIBS_SITE` and `R_LIBS` are
    /// blanked so a system-wide library cannot leak into a project.
    ///
    /// **Both** Renviron variables must be blanked. `R_ENVIRON` covers only
    /// the *site* file; the per-user `~/.Renviron` is gated separately by
    /// `R_ENVIRON_USER`, and a user whose `~/.Renviron` sets `R_LIBS_USER`
    /// would otherwise have it silently override the project library. `uvr
    /// run` is additionally protected by its `--no-environ` process flag,
    /// but a sourced activation script has no such flag — so blanking the
    /// variables is what actually does the work here.
    pub fn vars(&self) -> Vec<(&'static str, String)> {
        let r_lib_dir = self.r_lib_dir().to_string_lossy().into_owned();
        vec![
            ("R_LIBS_USER", self.r_libs_user()),
            ("R_LIBS_SITE", String::new()),
            ("R_LIBS", String::new()),
            ("DYLD_LIBRARY_PATH", r_lib_dir.clone()),
            ("LD_LIBRARY_PATH", r_lib_dir),
            ("R_ENVIRON", String::new()),
            ("R_ENVIRON_USER", String::new()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn renv() -> REnv {
        REnv {
            r_binary: PathBuf::from("/opt/R/4.4.2/bin/R"),
            library: PathBuf::from("/proj/.uvr/library"),
            with_library: None,
            extra_libs: None,
        }
    }

    #[test]
    fn libs_user_is_the_library_when_nothing_else_is_set() {
        assert_eq!(renv().r_libs_user(), "/proj/.uvr/library");
    }

    #[test]
    fn with_library_takes_precedence_over_the_project_library() {
        let env = REnv {
            with_library: Some(PathBuf::from("/cache/with-envs/abc/.uvr/library")),
            ..renv()
        };
        assert_eq!(
            env.r_libs_user(),
            format!("/cache/with-envs/abc/.uvr/library{PATH_SEP}/proj/.uvr/library")
        );
    }

    #[test]
    fn extra_libs_are_appended_last() {
        let env = REnv {
            with_library: Some(PathBuf::from("/with")),
            extra_libs: Some("/site/lib".to_string()),
            ..renv()
        };
        assert_eq!(
            env.r_libs_user(),
            format!("/with{PATH_SEP}/proj/.uvr/library{PATH_SEP}/site/lib")
        );
    }

    #[test]
    fn whitespace_only_extra_libs_is_ignored() {
        // A shell exporting `UVR_EXTRA_LIBS=` must not produce a trailing
        // separator, which R would read as an empty (cwd) library entry.
        let env = REnv {
            extra_libs: Some("   ".to_string()),
            ..renv()
        };
        assert_eq!(env.r_libs_user(), "/proj/.uvr/library");
    }

    #[test]
    fn bin_and_lib_dirs_are_derived_from_the_binary() {
        let env = renv();
        assert_eq!(env.r_bin_dir(), PathBuf::from("/opt/R/4.4.2/bin"));
        assert_eq!(env.r_lib_dir(), PathBuf::from("/opt/R/4.4.2/lib"));
    }

    #[test]
    fn vars_isolate_the_project_from_system_libraries() {
        let vars = renv().vars();
        let get = |k: &str| {
            vars.iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("{k} not exported"))
        };
        assert_eq!(get("R_LIBS_USER"), "/proj/.uvr/library");
        // Blank, not absent: these actively shadow system-wide libraries.
        assert_eq!(get("R_LIBS_SITE"), "");
        assert_eq!(get("R_LIBS"), "");
        assert_eq!(get("R_ENVIRON"), "");
        // R_ENVIRON alone covers only the *site* Renviron. Without this, a
        // user whose ~/.Renviron sets R_LIBS_USER silently gets their own
        // library instead of the project's — verified against real R.
        assert_eq!(get("R_ENVIRON_USER"), "");
        assert_eq!(get("DYLD_LIBRARY_PATH"), "/opt/R/4.4.2/lib");
        assert_eq!(get("LD_LIBRARY_PATH"), "/opt/R/4.4.2/lib");
    }

    #[test]
    fn a_binary_without_parents_does_not_panic() {
        let env = REnv {
            r_binary: PathBuf::from("R"),
            ..renv()
        };
        assert_eq!(env.r_bin_dir(), PathBuf::from(""));
        assert_eq!(env.r_lib_dir(), PathBuf::from(""));
    }
}
