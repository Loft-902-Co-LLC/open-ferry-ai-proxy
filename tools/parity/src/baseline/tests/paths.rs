use super::*;

const BASELINE: &str = "tools/parity/baseline/config-save.json";

fn checkout() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_owned()
}

#[test]
fn relative_repository_path() {
    // Cargo runs tests from the package directory, without changing global cwd.
    assert_eq!(
        repository_path(Path::new("baseline/config-save.json")),
        BASELINE
    );
    assert_eq!(
        repository_path(Path::new(r"baseline\config-save.json")),
        BASELINE
    );
}

#[test]
fn absolute_repository_path_with_either_separator() {
    let absolute = checkout().join(BASELINE).to_string_lossy().into_owned();
    for separator in ['/', '\\'] {
        assert_eq!(
            repository_path(Path::new(
                &absolute.replace(['/', '\\'], &separator.to_string())
            )),
            BASELINE
        );
    }
}

#[test]
fn parent_components_resolve_inside_repository() {
    assert_eq!(
        repository_path(&checkout().join("tools/parity/src/../baseline/config-save.json")),
        BASELINE
    );
}

#[test]
fn external_path_uses_only_filename() {
    let directory = std::env::temp_dir().join(format!("parity-path-{}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("external.json");
    fs::write(&path, "{}\n").unwrap();
    assert_eq!(repository_path(&path), "external.json");
    fs::remove_dir_all(directory).unwrap();
    assert_eq!(
        repository_path(Path::new(r"C:\Users\someone\missing.json")),
        "missing.json"
    );
    assert_eq!(
        repository_path(Path::new("/Users/someone/missing.json")),
        "missing.json"
    );
}

#[test]
fn baseline_markdown_contains_no_local_path() {
    for path in [
        checkout().join(BASELINE),
        PathBuf::from(r"C:\Users\someone\missing.json"),
        PathBuf::from("/Users/someone/missing.json"),
    ] {
        let markdown = baseline().assess(&observation(), &path).unwrap().markdown;
        assert!(!markdown.contains(":\\"), "{markdown}");
        assert!(!markdown.contains("`/"), "{markdown}");
        assert!(!markdown.contains("Users"), "{markdown}");
        let expected = if path == checkout().join(BASELINE) {
            BASELINE
        } else {
            "missing.json"
        };
        assert!(
            markdown.contains(&format!("Baseline: `{expected}`.")),
            "{markdown}"
        );
    }
}
