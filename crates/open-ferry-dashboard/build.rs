//! Embeds the dashboard app, `dashboard/dist` at the repository root, when
//! it has been built; else the crate serves a page saying it wasn't.
//!
//! Writes `$OUT_DIR/assets.rs`: `FILES`, each file of `dist` by its path
//! below `dist` (`/`-separated) with its bytes through `include_bytes!`,
//! sorted by path, and `BUILT`, whether there was an app to embed.
//!
//! With `OPEN_FERRY_REQUIRE_DASHBOARD=1` the build fails when
//! `dist/index.html` is missing, so a release can't ship without the app.
//!
//! The script runs again when `dist` changes, or, while there is none,
//! when anything in `dashboard/` does, which is when the app is built.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The variable that makes a missing app fail the build.
const REQUIRE: &str = "OPEN_FERRY_REQUIRE_DASHBOARD";

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: embedding the dashboard: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> io::Result<ExitCode> {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed={REQUIRE}");
    let manifest = env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("CARGO_MANIFEST_DIR isn't set"))?;
    let out_dir = env::var_os("OUT_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("OUT_DIR isn't set"))?;
    let app = manifest.join("..").join("..").join("dashboard");
    let dist = app.join("dist");

    let mut files = Vec::new();
    let built = dist.join("index.html").is_file();
    if built {
        println!("cargo::rerun-if-changed={}", dist.display());
        collect(&dist, "", &mut files)?;
        files.sort();
    } else {
        if env::var(REQUIRE).is_ok_and(|value| value == "1") {
            eprintln!(
                "error: {REQUIRE}=1, but the dashboard isn't built: {} is missing. \
                 Run `npm ci` and `npm run build` in dashboard/ first.",
                dist.join("index.html").display()
            );
            return Ok(ExitCode::FAILURE);
        }
        // Cargo runs a script on every build while a path it watches is
        // missing, so watch only one that exists.
        if dist.exists() {
            println!("cargo::rerun-if-changed={}", dist.display());
        } else if app.exists() {
            println!("cargo::rerun-if-changed={}", app.display());
        }
    }

    let mut code = String::new();
    code.push_str("/// The app's files, by their path below `dist`, sorted.\n");
    code.push_str("static FILES: &[(&str, &[u8])] = &[\n");
    for (name, path) in &files {
        let _ = writeln!(code, "    ({name:?}, include_bytes!({path:?})),");
    }
    code.push_str("];\n\n");
    code.push_str("/// Whether the app was built into this binary.\n");
    let _ = writeln!(code, "const BUILT: bool = {built};");
    fs::write(out_dir.join("assets.rs"), code)?;
    Ok(ExitCode::SUCCESS)
}

/// Adds each plain file below `dir` to `files`, as its path below `dist`
/// (`prefix` is `dir`'s) and its full path. A link, or a name that isn't
/// UTF-8, is left out with a warning.
fn collect(dir: &Path, prefix: &str, files: &mut Vec<(String, String)>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            println!(
                "cargo::warning=dashboard: skipped {}: the name isn't UTF-8",
                path.display()
            );
            continue;
        };
        let file_type = entry.file_type()?;
        let relative = format!("{prefix}{name}");
        if file_type.is_dir() {
            collect(&path, &format!("{relative}/"), files)?;
        } else if file_type.is_file() {
            let Some(full) = path.to_str().map(str::to_owned) else {
                println!(
                    "cargo::warning=dashboard: skipped {}: the path isn't UTF-8",
                    path.display()
                );
                continue;
            };
            files.push((relative, full));
        } else {
            println!(
                "cargo::warning=dashboard: skipped {}: not a plain file",
                path.display()
            );
        }
    }
    Ok(())
}
