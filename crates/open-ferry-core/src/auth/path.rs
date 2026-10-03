//! Go's `path/filepath` behaviour that credential IDs, paths and indexes
//! depend on: `Clean`, `Rel`, `Join` and `Abs`, worked out on path
//! components.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

/// A path split into its prefix (a Windows drive or share), whether it is
/// rooted, and its cleaned parts.
struct Parts<'a> {
    prefix: Option<&'a OsStr>,
    rooted: bool,
    parts: Vec<&'a OsStr>,
}

fn split(path: &Path) -> Parts<'_> {
    let mut out = Parts {
        prefix: None,
        rooted: false,
        parts: Vec::new(),
    };
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.prefix = Some(prefix.as_os_str()),
            Component::RootDir => out.rooted = true,
            Component::CurDir => {}
            Component::ParentDir => {
                if out.parts.last().is_some_and(|last| *last != "..") {
                    out.parts.pop();
                } else if !out.rooted {
                    out.parts.push(OsStr::new(".."));
                }
            }
            Component::Normal(part) => out.parts.push(part),
        }
    }
    out
}

fn assemble(prefix: Option<&OsStr>, rooted: bool, parts: &[&OsStr]) -> PathBuf {
    let mut out = PathBuf::new();
    if let Some(prefix) = prefix {
        out.push(prefix);
    }
    if rooted {
        out.push(std::path::MAIN_SEPARATOR_STR);
    }
    for part in parts {
        out.push(part);
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// Go's `filepath.Clean`: the shortest path naming the same file, worked out
/// without touching the file system.
pub(crate) fn clean(path: &Path) -> PathBuf {
    let parts = split(path);
    assemble(parts.prefix, parts.rooted, &parts.parts)
}

/// Go's `filepath.Rel`: `target` relative to `base`, or `None` when it can't
/// be expressed that way.
pub(crate) fn rel(base: &Path, target: &Path) -> Option<PathBuf> {
    let base = split(base);
    let target = split(target);
    let same_prefix = match (base.prefix, target.prefix) {
        (None, None) => true,
        (Some(a), Some(b)) => same_word(a, b),
        _ => false,
    };
    if !same_prefix || base.rooted != target.rooted {
        return None;
    }
    let common = base
        .parts
        .iter()
        .zip(&target.parts)
        .take_while(|(a, b)| same_word(a, b))
        .count();
    let base_rest = base.parts.get(common..).unwrap_or_default();
    if base_rest.first().is_some_and(|part| *part == "..") {
        return None;
    }
    let mut parts: Vec<&OsStr> = base_rest.iter().map(|_| OsStr::new("..")).collect();
    parts.extend(target.parts.get(common..).unwrap_or_default());
    Some(assemble(None, false, &parts))
}

/// Go's `filepath.Join` of two elements: joined with a separator and
/// cleaned, an empty element ignored. Unlike [`Path::join`], an absolute
/// `name` is appended rather than taking over.
pub(crate) fn join(dir: &Path, name: &Path) -> PathBuf {
    if dir.as_os_str().is_empty() {
        if name.as_os_str().is_empty() {
            return PathBuf::new();
        }
        return clean(name);
    }
    if name.as_os_str().is_empty() {
        return clean(dir);
    }
    let mut joined = dir.as_os_str().to_owned();
    joined.push(std::path::MAIN_SEPARATOR_STR);
    joined.push(name.as_os_str());
    clean(Path::new(&joined))
}

/// Go's `filepath.Abs`: `path` made absolute against the working directory
/// and cleaned. A path that can't be made absolute is only cleaned.
pub(crate) fn absolute(path: &Path) -> PathBuf {
    clean(&std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()))
}

/// Compares two path parts as Go's `filepath.Rel` does: without regard to
/// case on Windows.
fn same_word(a: &OsStr, b: &OsStr) -> bool {
    if cfg!(windows) {
        match (a.to_str(), b.to_str()) {
            (Some(a), Some(b)) => super::go::equal_fold(a, b),
            _ => a == b,
        }
    } else {
        a == b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(parts: &[&str]) -> PathBuf {
        parts.iter().collect()
    }

    #[test]
    fn clean_matches_go() {
        assert_eq!(clean(Path::new("")), PathBuf::from("."));
        assert_eq!(clean(Path::new("./a/./b/../c")), p(&["a", "c"]));
        assert_eq!(clean(Path::new("../a/..")), PathBuf::from(".."));
        assert_eq!(clean(Path::new("a//b/")), p(&["a", "b"]));
        let root = std::path::MAIN_SEPARATOR_STR;
        assert_eq!(clean(&p(&[root, "..", "a"])), p(&[root, "a"]));
    }

    #[test]
    fn rel_matches_go() {
        assert_eq!(
            rel(Path::new("auth"), &p(&["auth", "sub", "x.json"])),
            Some(p(&["sub", "x.json"]))
        );
        assert_eq!(
            rel(Path::new("./auth/"), &p(&["auth", "x.json"])),
            Some(PathBuf::from("x.json"))
        );
        assert_eq!(
            rel(Path::new("auth"), &p(&["other", "x.json"])),
            Some(p(&["..", "other", "x.json"]))
        );
        assert_eq!(
            rel(Path::new("a"), Path::new("a")),
            Some(PathBuf::from("."))
        );
        assert_eq!(rel(Path::new(".."), Path::new("a")), None);
        let root = std::path::MAIN_SEPARATOR_STR;
        assert_eq!(rel(Path::new("a"), &p(&[root, "a"])), None);
    }

    #[test]
    fn join_matches_go() {
        assert_eq!(
            join(Path::new("a"), Path::new("b.json")),
            p(&["a", "b.json"])
        );
        assert_eq!(join(Path::new("a/"), Path::new("./b/../c")), p(&["a", "c"]));
        assert_eq!(join(Path::new(""), Path::new("b")), PathBuf::from("b"));
        assert_eq!(join(Path::new("a"), Path::new("")), PathBuf::from("a"));
        assert_eq!(join(Path::new(""), Path::new("")), PathBuf::new());
        let root = std::path::MAIN_SEPARATOR_STR;
        assert_eq!(join(Path::new("a"), &p(&[root, "b"])), p(&["a", "b"]));
    }

    #[cfg(windows)]
    #[test]
    fn rel_ignores_case_on_windows() {
        assert_eq!(
            rel(Path::new(r"C:\Auth"), Path::new(r"c:\auth\X.json")),
            Some(PathBuf::from("X.json"))
        );
    }
}
