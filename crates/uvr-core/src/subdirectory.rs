//! Subdirectory paths reach URL paths and archive extraction, so only portable repository-relative paths are accepted.

use crate::error::{Result, UvrError};

pub fn is_valid(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') || path.contains('\\') || path.contains(':') {
        return false;
    }
    if path.chars().any(char::is_control) {
        return false;
    }
    path.split('/')
        .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

pub fn validate(path: &str) -> Result<()> {
    if is_valid(path) {
        return Ok(());
    }
    Err(UvrError::Other(format!(
        "Invalid subdirectory '{path}'. Expected a repository-relative path such as \
         'pkg' or 'r/pkg': no leading '/', no '\\', no ':', no '.' or '..' segments, \
         and no empty segments."
    )))
}

pub fn encode_segments(path: &str) -> String {
    path.split('/')
        .map(|seg| urlencoding::encode(seg).into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_portable_relative_paths() {
        for ok in ["pkg", "r/pkg", "a/b/c", "my-pkg_1.0", "pkg dir"] {
            assert!(is_valid(ok), "should accept {ok:?}");
            assert!(validate(ok).is_ok());
        }
    }

    #[test]
    fn rejects_unsafe_or_non_portable_paths() {
        for bad in [
            "",
            "/abs",
            "a\\b",
            "C:/pkg",
            "https://example.com/pkg",
            "a//b",
            ".",
            "..",
            "a/../b",
            "./a",
            "a/.",
            "a/",
            "a\nb",
            "a\tb",
        ] {
            assert!(!is_valid(bad), "should reject {bad:?}");
            assert!(validate(bad).is_err(), "should error on {bad:?}");
        }
    }

    #[test]
    fn encode_segments_preserves_separators() {
        assert_eq!(encode_segments("r/pkg"), "r/pkg");
        assert_eq!(encode_segments("my pkg/sub"), "my%20pkg/sub");
        assert_eq!(encode_segments("a+b/c"), "a%2Bb/c");
    }
}
