//! Inline dependency headers — a script that carries its own environment.
//!
//! An `.R` file can declare the packages it needs in a fenced comment block
//! near the top, so `uvr run script.R` provisions them and runs the file in
//! **any** directory, with no project and no setup. Send someone the script
//! and it runs.
//!
//! ```r
//! # /// script
//! # dependencies = [
//! #   "ggplot2",
//! #   "dplyr",
//! # ]
//! # ///
//!
//! library(ggplot2)
//! ```
//!
//! The shape mirrors Python's PEP 723, which solved the same problem: an
//! opening `# /// script` line, `#`-prefixed TOML, and a closing `# ///`.
//! R has no published standard here, so uvr defines the format rather than
//! inventing a second one later.
//!
//! Both fence lines must sit at column 0 with no leading whitespace — the
//! same rule PEP 723 sets. An indented `# /// script` is *not* a header, so
//! that a line inside a string or a nested comment cannot open a block.
//!
//! This is the first slice (#181): plain package names only. Version
//! constraints, Bioconductor and git sources arrive with #182, and the `r`
//! version pin with #183 — both are rejected or warned about here rather
//! than accepted and quietly misread.

use serde::Deserialize;

use crate::error::{Result, UvrError};

/// Opens the block. Matched exactly, ignoring only trailing whitespace.
const FENCE_OPEN: &str = "# /// script";
/// Closes the block.
const FENCE_CLOSE: &str = "# ///";

/// The declarations parsed out of a script's inline header.
///
/// Unknown keys are ignored rather than rejected, so a script written for a
/// newer uvr still runs on an older one instead of failing on a key it hasn't
/// learned yet.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct ScriptHeader {
    /// Package names, in the order written.
    #[serde(default)]
    pub dependencies: Vec<String>,

    /// R version constraint. Parsed but not yet acted on — see [`parse`].
    #[serde(default)]
    pub r: Option<String>,
}

/// True for a bare R package name — a letter, then letters, digits and dots.
///
/// The rule *Writing R Extensions* sets, and the whole grammar this slice
/// accepts. Anything else (`ggplot2>=3.4`, `DESeq2 (bioc)`, `user/repo@ref`)
/// is a spec kind #182 introduces; accepting one now would send it to the
/// resolver as a literal package name and produce a baffling
/// "Package not found: ggplot2>=3.4" instead of naming the real problem.
fn is_plain_package_name(spec: &str) -> bool {
    let mut chars = spec.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '.')
        && !spec.ends_with('.')
}

/// Parse a script's inline dependency header.
///
/// Returns `Ok(None)` when the file has no header at all — the overwhelming
/// majority of scripts, which must keep running exactly as they do today.
/// A header that is present but broken is an error, never a silent `None`:
/// a typo in the fence would otherwise drop every declared dependency and
/// fail later as a confusing missing-package error.
///
/// `r` is parsed but not applied — R-version pinning lands in #183. Callers
/// should tell the user it was ignored rather than silently running against
/// whichever R they happen to have.
pub fn parse(source: &str) -> Result<Option<ScriptHeader>> {
    let mut lines = source.lines();

    // Scan for the opening fence. Anything before it is ordinary script
    // content — a shebang, a licence banner, a roxygen block.
    if !lines.any(|line| line.trim_end() == FENCE_OPEN) {
        return Ok(None);
    }

    let mut body = String::new();
    for line in lines {
        // Checked before the comment strip below, which would otherwise
        // consume the closing fence as a body line: `# ///` less its `# `
        // prefix is `///`, which TOML would then reject as a syntax error
        // rather than the block ending cleanly.
        if line.trim_end() == FENCE_CLOSE {
            let header: ScriptHeader =
                toml::from_str(&body).map_err(|e| UvrError::ScriptHeaderParse(e.to_string()))?;

            if let Some(bad) = header
                .dependencies
                .iter()
                .find(|spec| !is_plain_package_name(spec))
            {
                return Err(UvrError::ScriptHeaderParse(format!(
                    "`{bad}` is not a plain package name. Version constraints, \
                     Bioconductor and git sources in script headers are not \
                     supported yet (#182)"
                )));
            }

            return Ok(Some(header));
        }

        let content = if line.trim_end() == "#" {
            ""
        } else if let Some(rest) = line.strip_prefix("# ") {
            rest
        } else {
            // Not "unterminated" — the block may well be closed further down.
            // This line simply cannot be part of it, which is almost always a
            // missing `# ///` above it.
            return Err(UvrError::ScriptHeaderParse(format!(
                "`{}` is not a `#` comment, so it cannot be inside the \
                 `{FENCE_OPEN}` block — is the closing `{FENCE_CLOSE}` missing?",
                line.trim()
            )));
        };

        body.push_str(content);
        body.push('\n');
    }

    Err(UvrError::ScriptHeaderParse(format!(
        "unterminated `{FENCE_OPEN}` block: no closing `{FENCE_CLOSE}` line"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deps(source: &str) -> Vec<String> {
        parse(source)
            .expect("should parse")
            .expect("should have a header")
            .dependencies
    }

    #[test]
    fn a_header_yields_its_dependencies_in_order() {
        let source = "\
# /// script
# dependencies = [
#   \"ggplot2\",
#   \"dplyr\",
# ]
# ///

library(ggplot2)
";
        assert_eq!(deps(source), vec!["ggplot2", "dplyr"]);
    }

    #[test]
    fn a_single_line_array_works_too() {
        let source = "# /// script\n# dependencies = [\"jsonlite\"]\n# ///\n";
        assert_eq!(deps(source), vec!["jsonlite"]);
    }

    #[test]
    fn a_file_with_no_header_has_none() {
        assert_eq!(parse("library(ggplot2)\nprint(1)\n").unwrap(), None);
    }

    #[test]
    fn an_empty_file_has_none() {
        assert_eq!(parse("").unwrap(), None);
    }

    #[test]
    fn a_decorative_comment_divider_does_not_open_a_block() {
        // `# ///` on its own is a plausible section divider in a real script.
        // Only the full `# /// script` opens a header, so a divider must not
        // turn the rest of the file into a parse error.
        let source = "# ///\n# a divider, not a header\nprint(1)\n";
        assert_eq!(parse(source).unwrap(), None);
    }

    #[test]
    fn the_fence_must_sit_at_column_zero() {
        // Indented, so not a header — matching PEP 723's rule.
        let source = "  # /// script\n  # dependencies = [\"x\"]\n  # ///\n";
        assert_eq!(parse(source).unwrap(), None);
    }

    #[test]
    fn trailing_whitespace_on_a_fence_is_tolerated() {
        let source = "# /// script  \n# dependencies = []\n# ///\t\n";
        assert_eq!(deps(source), Vec::<String>::new());
    }

    #[test]
    fn an_empty_dependency_list_is_a_header_not_an_absence() {
        // Distinct from `Ok(None)`: this script asked for an isolated
        // environment with nothing in it, and must get one.
        let source = "# /// script\n# dependencies = []\n# ///\n";
        assert_eq!(parse(source).unwrap(), Some(ScriptHeader::default()));
    }

    #[test]
    fn a_header_with_no_keys_at_all_is_valid() {
        let source = "# /// script\n# ///\n";
        assert_eq!(parse(source).unwrap(), Some(ScriptHeader::default()));
    }

    #[test]
    fn a_bare_hash_is_a_blank_body_line() {
        let source = "# /// script\n#\n# dependencies = [\"withr\"]\n#\n# ///\n";
        assert_eq!(deps(source), vec!["withr"]);
    }

    #[test]
    fn an_unterminated_block_is_an_error() {
        let source = "# /// script\n# dependencies = [\"ggplot2\"]\n";
        let err = parse(source).unwrap_err().to_string();
        assert!(err.contains("unterminated"), "got: {err}");
        assert!(err.contains("no closing"), "got: {err}");
    }

    #[test]
    fn script_code_before_the_closing_fence_is_an_error() {
        // The likeliest real typo: the author forgot the closing line, so
        // the first line of actual code lands inside the block. The block
        // *is* closed further down, so the message must point at the stray
        // line rather than claim the block was never terminated.
        let source = "# /// script\n# dependencies = [\"ggplot2\"]\nlibrary(ggplot2)\n# ///\n";
        let err = parse(source).unwrap_err().to_string();
        assert!(err.contains("library(ggplot2)"), "got: {err}");
        assert!(err.contains(FENCE_CLOSE), "got: {err}");
        assert!(!err.contains("unterminated"), "misleading wording: {err}");
    }

    #[test]
    fn malformed_toml_in_the_body_is_an_error() {
        let source = "# /// script\n# dependencies = [\n# ///\n";
        assert!(parse(source).is_err());
    }

    #[test]
    fn a_wrongly_typed_dependencies_key_is_an_error() {
        let source = "# /// script\n# dependencies = \"ggplot2\"\n# ///\n";
        assert!(parse(source).is_err());
    }

    #[test]
    fn an_unknown_key_is_ignored_for_forward_compatibility() {
        // A script written against a newer uvr must still run here rather
        // than failing on a key this version has never heard of.
        let source = "# /// script\n# future-knob = 7\n# dependencies = [\"cli\"]\n# ///\n";
        assert_eq!(deps(source), vec!["cli"]);
    }

    #[test]
    fn an_r_pin_is_captured_even_though_it_is_not_applied_yet() {
        // Captured so the caller can say it was ignored (#183). Dropping it
        // silently would run the script against the wrong R with no signal.
        let source = "# /// script\n# r = \">=4.3\"\n# dependencies = [\"cli\"]\n# ///\n";
        let header = parse(source).unwrap().unwrap();
        assert_eq!(header.r.as_deref(), Some(">=4.3"));
        assert_eq!(header.dependencies, vec!["cli"]);
    }

    #[test]
    fn dotted_package_names_are_plain_names() {
        // `data.table`, `org.Hs.eg.db` — dots are legal in R package names,
        // so the plain-name check must not mistake them for a spec grammar.
        let source = "# /// script\n# dependencies = [\"data.table\", \"org.Hs.eg.db\"]\n# ///\n";
        assert_eq!(deps(source), vec!["data.table", "org.Hs.eg.db"]);
    }

    #[test]
    fn specs_this_slice_cannot_honour_are_rejected_by_name() {
        // Passing these through would reach the resolver as literal package
        // names and fail with "Package not found: ggplot2>=3.4" — a message
        // that describes neither the cause nor the fix.
        for spec in [
            "ggplot2>=3.4",
            "DESeq2 (bioc)",
            "tidyverse/ggplot2@main",
            "forgejo::codeberg.org/o/r",
            "",
        ] {
            let source = format!("# /// script\n# dependencies = [\"{spec}\"]\n# ///\n");
            let err = match parse(&source) {
                Err(e) => e.to_string(),
                Ok(ok) => panic!("`{spec}` should be rejected, got {ok:?}"),
            };
            assert!(err.contains("not a plain package name"), "got: {err}");
        }
    }

    #[test]
    fn the_header_may_follow_a_shebang_or_banner() {
        let source = "\
#!/usr/bin/env Rscript
# Copyright someone
# /// script
# dependencies = [\"cli\"]
# ///
";
        assert_eq!(deps(source), vec!["cli"]);
    }

    #[test]
    fn only_the_first_block_is_read() {
        // A second fence pair later in the file is ordinary script content;
        // the first block wins and its close ends the header.
        let source = "\
# /// script
# dependencies = [\"cli\"]
# ///
print(1)
# /// script
# dependencies = [\"nope\"]
# ///
";
        assert_eq!(deps(source), vec!["cli"]);
    }
}
