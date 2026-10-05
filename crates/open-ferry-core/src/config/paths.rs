// Ported from CLIProxyAPI internal/util/util.go (ResolveAuthDir) (v8.0.15, MIT),
// and from Go's path/filepath (path.go, path_windows.go, path_unix.go;
// go1.27, BSD-3-Clause) and os.UserHomeDir, which it calls.
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/golang/go

//! Paths as upstream spells them.
//!
//! Upstream resolves `auth-dir` and compares watched paths as strings run
//! through Go's `filepath.Clean`, which differs from Rust's `Path`
//! normalization (it folds `..`, and on Windows rewrites `/` as `\` and
//! guards drive-relative names). [`clean`], [`dir`] and [`join`] repeat
//! Go's rules for both platform families so either can be tested anywhere;
//! [`Os::HOST`] picks the one this build runs on.
//!
//! Deviations from upstream:
//! - A home directory that isn't valid UTF-8 is converted lossily.

use std::env;

use super::types::DEFAULT_AUTH_DIR;

/// Which of Go's path rules apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Os {
    Unix,
    Windows,
}

impl Os {
    /// The rules of the platform this build runs on.
    pub(crate) const HOST: Os = if cfg!(windows) { Os::Windows } else { Os::Unix };

    fn is_separator(self, byte: u8) -> bool {
        byte == b'/' || (self == Os::Windows && byte == b'\\')
    }

    fn separator(self) -> u8 {
        match self {
            Os::Unix => b'/',
            Os::Windows => b'\\',
        }
    }

    /// Go's `filepath.FromSlash`.
    fn native_separators(self, path: &str) -> String {
        match self {
            Os::Unix => path.to_owned(),
            Os::Windows => path.replace('/', "\\"),
        }
    }
}

/// Go's `lazybuf`: output that is only copied once it differs from the
/// input.
struct LazyBuf<'a> {
    path: &'a [u8],
    buf: Option<Vec<u8>>,
    w: usize,
}

impl LazyBuf<'_> {
    fn index(&self, i: usize) -> u8 {
        let source = self.buf.as_deref().unwrap_or(self.path);
        source.get(i).copied().unwrap_or_default()
    }

    fn append(&mut self, byte: u8) {
        if self.buf.is_none() {
            if self.path.get(self.w) == Some(&byte) {
                self.w += 1;
                return;
            }
            let mut buf = vec![0; self.path.len().max(self.w + 1)];
            if let (Some(target), Some(source)) = (buf.get_mut(..self.w), self.path.get(..self.w)) {
                target.copy_from_slice(source);
            }
            self.buf = Some(buf);
        }
        if let Some(buf) = &mut self.buf {
            if buf.len() <= self.w {
                buf.resize(self.w + 1, 0);
            }
            if let Some(slot) = buf.get_mut(self.w) {
                *slot = byte;
            }
        }
        self.w += 1;
    }

    fn prepend(&mut self, prefix: &[u8]) {
        let buf = self.buf.get_or_insert_with(|| self.path.to_vec());
        buf.splice(0..0, prefix.iter().copied());
        self.w += prefix.len();
    }

    fn output(&self) -> &[u8] {
        let source = self.buf.as_deref().unwrap_or(self.path);
        source.get(..self.w).unwrap_or(source)
    }
}

/// Go's `volumeNameLen`: the length of a leading drive or UNC volume.
fn volume_name_len(os: Os, path: &[u8]) -> usize {
    if os == Os::Unix {
        return 0;
    }
    let is_sep = |i: usize| path.get(i).is_some_and(|&byte| os.is_separator(byte));
    if path.get(1) == Some(&b':') {
        return 2;
    }
    if !is_sep(0) {
        return 0;
    }
    if has_prefix_fold(os, path, b"\\\\.\\UNC") {
        return unc_len(os, path, b"\\\\.\\UNC\\".len());
    }
    if has_prefix_fold(os, path, b"\\\\.")
        || has_prefix_fold(os, path, b"\\\\?")
        || has_prefix_fold(os, path, b"\\??")
    {
        if path.len() == 3 {
            return 3;
        }
        let rest = path.get(4..).unwrap_or_default();
        return match rest.iter().position(|&byte| os.is_separator(byte)) {
            Some(index) => 4 + index,
            None => path.len(),
        };
    }
    if is_sep(1) {
        return unc_len(os, path, 2);
    }
    0
}

fn unc_len(os: Os, path: &[u8], prefix_len: usize) -> usize {
    let mut count = 0;
    for (i, &byte) in path.iter().enumerate().skip(prefix_len) {
        if os.is_separator(byte) {
            count += 1;
            if count == 2 {
                return i;
            }
        }
    }
    path.len()
}

/// Go's `pathHasPrefixFold`: a case-insensitive prefix where any separator
/// matches any other, followed by a separator or the end.
fn has_prefix_fold(os: Os, path: &[u8], prefix: &[u8]) -> bool {
    if path.len() < prefix.len() {
        return false;
    }
    for (&want, &have) in prefix.iter().zip(path) {
        if os.is_separator(want) {
            if !os.is_separator(have) {
                return false;
            }
        } else if !want.eq_ignore_ascii_case(&have) {
            return false;
        }
    }
    path.get(prefix.len())
        .is_none_or(|&byte| os.is_separator(byte))
}

/// Go's `filepath.Clean`.
pub(crate) fn clean(os: Os, original: &str) -> String {
    let bytes = original.as_bytes();
    let vol_len = volume_name_len(os, bytes);
    let (volume, path) = bytes.split_at(vol_len.min(bytes.len()));
    if path.is_empty() {
        let unc = vol_len > 1
            && bytes.first().is_some_and(|&b| os.is_separator(b))
            && bytes.get(1).is_some_and(|&b| os.is_separator(b));
        if unc {
            return os.native_separators(original);
        }
        return format!("{original}.");
    }
    let rooted = path.first().is_some_and(|&byte| os.is_separator(byte));
    let n = path.len();
    let mut out = LazyBuf {
        path,
        buf: None,
        w: 0,
    };
    let (mut r, mut dotdot) = (0, 0);
    if rooted {
        out.append(os.separator());
        (r, dotdot) = (1, 1);
    }
    let at = |i: usize| path.get(i).copied();
    let sep_or_end = |i: usize| at(i).is_none_or(|byte| os.is_separator(byte));
    while r < n {
        if at(r).is_some_and(|byte| os.is_separator(byte))
            || (at(r) == Some(b'.') && sep_or_end(r + 1))
        {
            // An empty element or `.`.
            r += 1;
        } else if at(r) == Some(b'.') && at(r + 1) == Some(b'.') && sep_or_end(r + 2) {
            r += 2;
            if out.w > dotdot {
                out.w -= 1;
                while out.w > dotdot && !os.is_separator(out.index(out.w)) {
                    out.w -= 1;
                }
            } else if !rooted {
                if out.w > 0 {
                    out.append(os.separator());
                }
                out.append(b'.');
                out.append(b'.');
                dotdot = out.w;
            }
        } else {
            if (rooted && out.w != 1) || (!rooted && out.w != 0) {
                out.append(os.separator());
            }
            while let Some(byte) = at(r).filter(|&byte| !os.is_separator(byte)) {
                out.append(byte);
                r += 1;
            }
        }
    }
    if out.w == 0 {
        out.append(b'.');
    }
    if os == Os::Windows {
        post_clean(&mut out, vol_len);
    }
    let mut result = Vec::with_capacity(volume.len() + out.w);
    result.extend_from_slice(volume);
    result.extend_from_slice(out.output());
    os.native_separators(&String::from_utf8_lossy(&result))
}

/// Go's Windows `postClean`: keeps a cleaned relative path from turning
/// into a drive or device path. It scans the whole buffer, as Go does.
fn post_clean(out: &mut LazyBuf<'_>, vol_len: usize) {
    if vol_len != 0 {
        return;
    }
    let Some(buf) = &out.buf else {
        return;
    };
    let colon = buf
        .iter()
        .take_while(|&&byte| !Os::Windows.is_separator(byte))
        .any(|&byte| byte == b':');
    let device = buf.len() >= 3
        && buf
            .first()
            .is_some_and(|&byte| Os::Windows.is_separator(byte))
        && buf.get(1) == Some(&b'?')
        && buf.get(2) == Some(&b'?');
    if colon {
        out.prepend(b".\\");
    } else if device {
        out.prepend(b"\\.");
    }
}

/// Go's `filepath.Dir`.
pub(crate) fn dir(os: Os, path: &str) -> String {
    let bytes = path.as_bytes();
    let vol_len = volume_name_len(os, bytes);
    let mut end = bytes.len();
    while end > vol_len
        && bytes
            .get(end - 1)
            .is_some_and(|&byte| !os.is_separator(byte))
    {
        end -= 1;
    }
    let volume = os.native_separators(&String::from_utf8_lossy(
        bytes.get(..vol_len).unwrap_or_default(),
    ));
    let rest = String::from_utf8_lossy(bytes.get(vol_len..end).unwrap_or_default());
    let cleaned = clean(os, &rest);
    if cleaned == "." && vol_len > 2 {
        return volume;
    }
    volume + &cleaned
}

/// Go's `filepath.Join` of two non-empty elements.
pub(crate) fn join(os: Os, first: &str, second: &str) -> String {
    let mut joined = first.to_owned();
    match (os, first.as_bytes().last()) {
        (Os::Unix, _) => {
            joined.push('/');
            joined.push_str(second);
        }
        (Os::Windows, Some(&last)) if os.is_separator(last) => {
            let second = second.trim_start_matches(['/', '\\']);
            let device = second
                .strip_prefix("??")
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', '\\']));
            if joined.len() == 1 && device {
                joined.push_str(".\\");
            }
            joined.push_str(second);
        }
        (Os::Windows, Some(b':')) => joined.push_str(second),
        (Os::Windows, _) => {
            joined.push('\\');
            joined.push_str(second);
        }
    }
    clean(os, &joined)
}

/// Go's `os.UserHomeDir`.
pub(crate) fn user_home_dir() -> Result<String, String> {
    let (name, shown) = match Os::HOST {
        Os::Windows => ("USERPROFILE", "%userprofile%"),
        Os::Unix => ("HOME", "$HOME"),
    };
    match env::var_os(name) {
        Some(value) if !value.is_empty() => Ok(value.to_string_lossy().into_owned()),
        _ => Err(format!("{shown} is not defined")),
    }
}

/// Upstream's `ResolveAuthDir`: `auth_dir`, or the default, with a leading
/// `~` replaced by the home directory, cleaned.
pub(crate) fn resolve_auth_dir(
    os: Os,
    auth_dir: &str,
    home: impl FnOnce() -> Result<String, String>,
) -> Result<String, String> {
    let auth_dir = if auth_dir.is_empty() {
        DEFAULT_AUTH_DIR
    } else {
        auth_dir
    };
    let Some(remainder) = auth_dir.strip_prefix('~') else {
        return Ok(clean(os, auth_dir));
    };
    let home = home().map_err(|error| format!("resolve auth dir: {error}"))?;
    let remainder = remainder.trim_start_matches(['/', '\\']);
    if remainder.is_empty() {
        return Ok(clean(os, &home));
    }
    let normalized = remainder.replace('\\', "/");
    Ok(clean(
        os,
        &join(os, &home, &os.native_separators(&normalized)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_matches_go_on_unix() {
        for (input, want) in [
            ("", "."),
            ("abc", "abc"),
            ("abc/def/", "abc/def"),
            ("a//b", "a/b"),
            ("./a/./b/", "a/b"),
            ("a/b/../c", "a/c"),
            ("../../a", "../../a"),
            ("/../a", "/a"),
            ("/", "/"),
            ("a/../..", ".."),
            ("a\\b", "a\\b"),
        ] {
            assert_eq!(clean(Os::Unix, input), want, "{input:?}");
        }
    }

    // Expected values come from Go 1.27's filepath.Clean, Dir and Join on
    // Windows.
    #[test]
    fn clean_matches_go_on_windows() {
        for (input, want) in [
            ("", "."),
            ("c:", "c:."),
            ("c:\\", "c:\\"),
            ("c:/a/b/../c", "c:\\a\\c"),
            (
                "C:\\Users\\me\\..\\you\\.cli-proxy-api",
                "C:\\Users\\you\\.cli-proxy-api",
            ),
            ("\\\\host\\share", "\\\\host\\share"),
            ("\\\\host\\share\\a\\..\\..", "\\\\host\\share\\"),
            ("//host/share/x", "\\\\host\\share\\x"),
            ("\\\\?\\c:\\a\\..", "\\\\?\\c:\\"),
            ("a/../c:", ".\\c:"),
            ("ab:c", "ab:c"),
            ("./b:c/../d", ".\\d"),
            ("\\a\\..\\??\\c:\\x", "\\.\\??\\c:\\x"),
            ("a/b", "a\\b"),
            ("../../a", "..\\..\\a"),
            (
                "\\\\.\\UNC\\host\\share\\x\\..",
                "\\\\.\\UNC\\host\\share\\",
            ),
            ("\\\\.\\c:\\x", "\\\\.\\c:\\x"),
            ("\\??\\c:\\x\\..\\y", "\\??\\c:\\y"),
            ("\\\\host", "\\\\host"),
            ("\\\\", "\\\\"),
            ("C:.", "C:."),
            ("x/y/../../..", ".."),
        ] {
            assert_eq!(clean(Os::Windows, input), want, "{input:?}");
        }
        assert_eq!(dir(Os::Windows, "C:\\a\\b.json"), "C:\\a");
        assert_eq!(dir(Os::Windows, "c:/a/b.json"), "c:\\a");
        assert_eq!(
            dir(Os::Windows, "\\\\host\\share\\b.json"),
            "\\\\host\\share\\"
        );
        assert_eq!(dir(Os::Windows, "\\\\host\\share"), "\\\\host\\share");
        assert_eq!(dir(Os::Windows, "/a/b.json"), "\\a");
        assert_eq!(dir(Os::Windows, "C:\\b.json"), "C:\\");
        assert_eq!(dir(Os::Windows, "C:b.json"), "C:.");
        assert_eq!(dir(Os::Unix, "/a/b.json"), "/a");
        assert_eq!(dir(Os::Unix, "b.json"), ".");
        assert_eq!(
            join(Os::Windows, "C:\\Users\\me", "x\\y"),
            "C:\\Users\\me\\x\\y"
        );
        assert_eq!(join(Os::Windows, "C:", "x"), "C:x");
        assert_eq!(join(Os::Windows, "C:\\", "x"), "C:\\x");
        assert_eq!(join(Os::Windows, "\\", "??\\x"), "\\.\\??\\x");
    }

    #[test]
    fn resolve_auth_dir_like_upstream() {
        let home = || Ok::<_, String>("/home/me".to_owned());
        let resolve = |dir: &str| resolve_auth_dir(Os::Unix, dir, home);
        assert_eq!(resolve(""), Ok("/home/me/.cli-proxy-api".to_owned()));
        assert_eq!(resolve("~"), Ok("/home/me".to_owned()));
        assert_eq!(resolve("~/"), Ok("/home/me".to_owned()));
        assert_eq!(resolve("~/auths/../x"), Ok("/home/me/x".to_owned()));
        assert_eq!(resolve("~\\a\\b"), Ok("/home/me/a/b".to_owned()));
        assert_eq!(resolve("~user"), Ok("/home/me/user".to_owned()));
        assert_eq!(resolve("/srv//auth/"), Ok("/srv/auth".to_owned()));
        assert_eq!(resolve("rel/./dir"), Ok("rel/dir".to_owned()));

        let windows_home = || Ok::<_, String>("C:\\Users\\me".to_owned());
        let resolve = |dir: &str| resolve_auth_dir(Os::Windows, dir, windows_home);
        assert_eq!(resolve(""), Ok("C:\\Users\\me\\.cli-proxy-api".to_owned()));
        assert_eq!(resolve("~/a/b"), Ok("C:\\Users\\me\\a\\b".to_owned()));
        assert_eq!(resolve("D:/auth"), Ok("D:\\auth".to_owned()));

        let missing = || Err::<String, _>("$HOME is not defined".to_owned());
        assert_eq!(
            resolve_auth_dir(Os::Unix, "~/x", missing),
            Err("resolve auth dir: $HOME is not defined".to_owned())
        );
        assert_eq!(
            resolve_auth_dir(Os::Unix, "/x", missing),
            Ok("/x".to_owned())
        );
    }
}
