//! Package names reach manifest keys, lockfile entries, and library paths, so only the R package-name charset is accepted, never a path-special `.` or `..`.

pub fn is_valid(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '.' || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_r_package_names() {
        for ok in ["dplyr", "data.table", "my-pkg_1", "R6", "A", "ggplot2"] {
            assert!(is_valid(ok), "should accept {ok:?}");
        }
    }

    #[test]
    fn rejects_empty_space_plus_and_path_like_names() {
        for bad in [
            "",
            ".",
            "..",
            "my pkg",
            " pkg",
            "pkg ",
            "a+b",
            "a/b",
            "pkgs/my pkg",
            "../../outside",
            "/abs",
            "a\\b",
            "a:b",
            "a\nb",
            "pkg\0",
        ] {
            assert!(!is_valid(bad), "should reject {bad:?}");
        }
    }
}
