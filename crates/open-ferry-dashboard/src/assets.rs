//! The app's files, as `build.rs` embedded them, and what each one is.

include!(concat!(env!("OUT_DIR"), "/assets.rs"));

/// A set of the app's files: the embedded build, or a test's.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Assets {
    /// Each file by its path below `dist`, `/`-separated.
    files: &'static [(&'static str, &'static [u8])],
    /// Whether there is an app; without one, the placeholder page is
    /// served.
    built: bool,
}

impl Assets {
    /// The files built into this binary.
    pub(crate) fn embedded() -> Self {
        Self {
            files: FILES,
            built: BUILT,
        }
    }

    /// `files` as an app, for tests.
    #[cfg(test)]
    pub(crate) fn fixture(files: &'static [(&'static str, &'static [u8])]) -> Self {
        Self { files, built: true }
    }

    /// No app, as in a binary built without one.
    #[cfg(test)]
    pub(crate) fn missing() -> Self {
        Self {
            files: &[],
            built: false,
        }
    }

    /// Whether there is an app.
    pub(crate) fn built(self) -> bool {
        self.built
    }

    /// The file at `path` below `dist`, if there is one.
    pub(crate) fn find(self, path: &str) -> Option<&'static [u8]> {
        self.files
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| *bytes)
    }
}

/// The `Content-Type` of a file named `path`, by its extension.
pub(crate) fn content_type(path: &str) -> &'static str {
    let extension = path
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "txt" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not upstream's: each kind of file the app builds gets its type, and
    /// anything else is served as bytes.
    #[test]
    fn files_get_their_types() {
        for (path, expected) in [
            ("index.html", "text/html; charset=utf-8"),
            ("assets/index-B0x1.js", "text/javascript; charset=utf-8"),
            ("assets/index-C2y3.CSS", "text/css; charset=utf-8"),
            ("favicon.svg", "image/svg+xml"),
            ("third-party-licenses.txt", "text/plain; charset=utf-8"),
            ("assets/inter-D4.woff2", "font/woff2"),
            ("assets/index-B0x1.js.map", "application/json"),
            ("README", "application/octet-stream"),
            ("assets/blob.bin", "application/octet-stream"),
        ] {
            assert_eq!(content_type(path), expected, "{path}");
        }
    }

    /// Not upstream's: a fixture's files are found by their exact path.
    #[test]
    fn files_are_found_by_path() {
        let assets = Assets::fixture(&[("index.html", b"<p>app</p>"), ("assets/a.js", b"a")]);
        assert!(assets.built());
        assert_eq!(assets.find("assets/a.js"), Some(&b"a"[..]));
        assert_eq!(assets.find("assets/A.js"), None);
        assert_eq!(assets.find("/assets/a.js"), None);
        assert!(!Assets::missing().built());
        assert_eq!(Assets::missing().find("index.html"), None);
    }
}
