//! Not upstream's: what the update tests share: a release server on
//! 127.0.0.1, minisign keys made for each test (never the real one, never
//! written to the repository), release archives, and a fake machine and
//! `--version` runner.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use tempfile::TempDir;
use tokio::task::JoinHandle;
use url::Url;

use crate::data_dir::DataDir;
use crate::fetch::{BoxFuture, HttpFetch};
use crate::install::System;
use crate::keys::ReleaseKeys;
use crate::release::{self, ArchiveKind};
use crate::runner::Runner;
use crate::switch::ReplaceOnDisk;
use crate::updater::{Limits, Updater};

pub const LINUX: &str = "x86_64-unknown-linux-gnu";
pub const MUSL: &str = "x86_64-unknown-linux-musl";
pub const MAC: &str = "aarch64-apple-darwin";
pub const WINDOWS: &str = "x86_64-pc-windows-msvc";

/// A minisign key pair made for one test.
pub struct TestKey {
    pair: minisign::KeyPair,
}

impl TestKey {
    pub fn new() -> Self {
        Self {
            pair: minisign::KeyPair::generate_unencrypted_keypair().expect("a key pair"),
        }
    }

    /// The public key's base64 line, as `release-keys.pub` holds it.
    pub fn public(&self) -> String {
        self.pair.pk.to_base64()
    }

    /// A `.minisig` of `data` with `comment` as its trusted comment.
    pub fn sign(&self, data: &[u8], comment: &str) -> String {
        minisign::sign(
            Some(&self.pair.pk),
            &self.pair.sk,
            Cursor::new(data),
            Some(comment),
            None,
        )
        .expect("a signature")
        .into_string()
    }
}

/// Keys trusting `keys`, as a keys file would list them.
pub fn trusting(keys: &[&TestKey]) -> ReleaseKeys {
    let text: String = keys
        .iter()
        .map(|key| format!("{}\n", key.public()))
        .collect();
    ReleaseKeys::parse(&text).expect("keys")
}

/// An archive entry.
pub enum Entry {
    File(String, Vec<u8>),
    Dir(String),
    Symlink(String, String),
    Hardlink(String, String),
    CharDevice(String),
}

pub fn file(name: &str, data: &[u8]) -> Entry {
    Entry::File(name.to_owned(), data.to_vec())
}

/// A `.tar.gz` of `entries`, their names written as they are (`..`
/// included, which `tar::Builder` would refuse).
pub fn tar_gz(entries: &[Entry]) -> Vec<u8> {
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut builder = tar::Builder::new(encoder);
    for entry in entries {
        let mut header = tar::Header::new_gnu();
        let (name, kind, data, link): (&str, _, &[u8], Option<&str>) = match entry {
            Entry::File(name, data) => (name, tar::EntryType::Regular, data, None),
            Entry::Dir(name) => (name, tar::EntryType::Directory, &[], None),
            Entry::Symlink(name, to) => (name, tar::EntryType::Symlink, &[], Some(to)),
            Entry::Hardlink(name, to) => (name, tar::EntryType::Link, &[], Some(to)),
            Entry::CharDevice(name) => (name, tar::EntryType::Char, &[], None),
        };
        raw(&mut header.as_old_mut().name, name);
        if let Some(link) = link {
            raw(&mut header.as_old_mut().linkname, link);
        }
        header.set_entry_type(kind);
        header.set_mode(0o755);
        header.set_size(data.len() as u64);
        header.set_cksum();
        builder.append(&header, data).expect("a tar entry");
    }
    builder
        .into_inner()
        .expect("a tar")
        .finish()
        .expect("a gzip stream")
}

fn raw(field: &mut [u8], text: &str) {
    field.fill(0);
    field[..text.len()].copy_from_slice(text.as_bytes());
}

/// A `.zip` of `entries`.
pub fn zip(entries: &[Entry]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for entry in entries {
        match entry {
            Entry::File(name, data) => {
                writer
                    .start_file(name.as_str(), options)
                    .expect("a zip entry");
                writer.write_all(data).expect("a zip entry's data");
            }
            Entry::Dir(name) => writer
                .add_directory(name.as_str(), options)
                .expect("a zip directory"),
            Entry::Symlink(name, to) => writer
                .add_symlink(name.as_str(), to.as_str(), options)
                .expect("a zip link"),
            Entry::Hardlink(..) | Entry::CharDevice(_) => panic!("zip has none"),
        }
    }
    writer.finish().expect("a zip").into_inner()
}

/// The archive name of `version` for `target`.
pub fn archive_name(version: &str, target: &str) -> String {
    format!(
        "open-ferry-{version}-{target}{}",
        ArchiveKind::of(target).extension()
    )
}

/// A release archive of `version` for `target`, as release.yml packs it,
/// its binary holding `binary`.
pub fn release_archive(version: &str, target: &str, binary: &[u8]) -> Vec<u8> {
    let dir = format!("open-ferry-{version}-{target}");
    let entries = [
        Entry::Dir(format!("{dir}/")),
        file(&format!("{dir}/{}", release::binary_name(target)), binary),
        file(&format!("{dir}/LICENSE"), b"MIT"),
        file(&format!("{dir}/config.example.yaml"), b"port: 8317\n"),
    ];
    match ArchiveKind::of(target) {
        ArchiveKind::TarGz => tar_gz(&entries),
        ArchiveKind::Zip => zip(&entries),
    }
}

/// What the fake `--version` prints for a good binary of `version`; a
/// binary holding `#fail` fails.
pub fn good_binary(version: &str) -> Vec<u8> {
    format!("open-ferry {version}").into_bytes()
}

/// A SHA256SUMS line.
pub fn sums_line(data: &[u8], name: &str) -> String {
    format!("{}  {name}\n", release::sha256_hex(data))
}

/// How the release server answers a path.
#[derive(Clone)]
pub enum Reply {
    Body(Vec<u8>),
    Status(u16),
    Redirect(String),
    Slow(Duration, Vec<u8>),
}

#[derive(Default)]
struct Served {
    replies: HashMap<String, Reply>,
    seen: Vec<(String, String)>,
}

/// A release server on 127.0.0.1 that records every request.
pub struct ReleaseServer {
    pub base: Url,
    pub address: std::net::SocketAddr,
    served: Arc<Mutex<Served>>,
    task: JoinHandle<()>,
}

impl ReleaseServer {
    pub async fn start() -> Self {
        let served = Arc::new(Mutex::new(Served::default()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback listener");
        let address = listener.local_addr().expect("its address");
        let app = Router::new()
            .fallback(serve)
            .with_state(Arc::clone(&served));
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            base: Url::parse(&format!("http://{address}/releases")).expect("a URL"),
            address,
            served,
            task,
        }
    }

    /// Answers `path` (from the root) with `reply`.
    pub fn reply(&self, path: &str, reply: Reply) {
        self.served
            .lock()
            .unwrap()
            .replies
            .insert(path.to_owned(), reply);
    }

    /// Stops answering `path`.
    pub fn forget(&self, path: &str) {
        self.served.lock().unwrap().replies.remove(path);
    }

    /// The paths requested, in order.
    pub fn paths(&self) -> Vec<String> {
        let served = self.served.lock().unwrap();
        served.seen.iter().map(|(path, _)| path.clone()).collect()
    }

    /// How many requests came.
    pub fn requests(&self) -> usize {
        self.served.lock().unwrap().seen.len()
    }

    /// The User-Agents requests came with.
    pub fn user_agents(&self) -> Vec<String> {
        let served = self.served.lock().unwrap();
        served.seen.iter().map(|(_, agent)| agent.clone()).collect()
    }

    /// Publishes a release as the latest: `sums` and `signature` (none for
    /// an unsigned release) at `latest/download/`, and `archives` at
    /// `download/v<version>/`.
    pub fn publish_raw(
        &self,
        version: &str,
        sums: &[u8],
        signature: Option<&str>,
        archives: &[(String, Vec<u8>)],
    ) {
        self.reply(
            "/releases/latest/download/SHA256SUMS",
            Reply::Body(sums.to_vec()),
        );
        let signature_path = "/releases/latest/download/SHA256SUMS.minisig";
        match signature {
            Some(signature) => {
                self.reply(signature_path, Reply::Body(signature.as_bytes().to_vec()))
            }
            None => self.forget(signature_path),
        }
        for (name, data) in archives {
            self.reply(
                &format!("/releases/download/v{version}/{name}"),
                Reply::Body(data.clone()),
            );
        }
    }

    /// Publishes `archives` as release `version`, signed by `key`.
    pub fn publish(&self, key: &TestKey, version: &str, archives: &[(String, Vec<u8>)]) {
        let sums: String = archives
            .iter()
            .map(|(name, data)| sums_line(data, name))
            .collect();
        let signature = key.sign(sums.as_bytes(), &format!("open-ferry {version} SHA256SUMS"));
        self.publish_raw(version, sums.as_bytes(), Some(&signature), archives);
    }
}

impl Drop for ReleaseServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(State(served): State<Arc<Mutex<Served>>>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    let agent = request
        .headers()
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let reply = {
        let mut served = served.lock().unwrap();
        served.seen.push((path.clone(), agent));
        served.replies.get(&path).cloned()
    };
    let status = |code: u16| {
        Response::builder()
            .status(StatusCode::from_u16(code).unwrap())
            .body(Body::empty())
            .unwrap()
    };
    match reply {
        None => status(404),
        Some(Reply::Body(data)) => Response::new(Body::from(data)),
        Some(Reply::Status(code)) => status(code),
        Some(Reply::Redirect(location)) => Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, location)
            .body(Body::empty())
            .unwrap(),
        Some(Reply::Slow(wait, data)) => {
            tokio::time::sleep(wait).await;
            Response::new(Body::from(data))
        }
    }
}

/// A machine as a test says it is.
pub struct FakeSystem {
    pub exe: PathBuf,
    pub container: bool,
    pub writable: bool,
}

impl System for FakeSystem {
    fn current_exe(&self) -> io::Result<PathBuf> {
        Ok(self.exe.clone())
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        fs::canonicalize(path)
    }

    fn in_container(&self) -> bool {
        self.container
    }

    fn dir_writable(&self, _dir: &Path) -> bool {
        self.writable
    }
}

/// A `--version` run that prints the binary's contents, or fails for one
/// that holds `#fail`; nothing is executed.
#[derive(Default)]
pub struct FakeRunner {
    pub runs: AtomicUsize,
}

impl Runner for FakeRunner {
    fn version<'a>(
        &'a self,
        binary: &'a Path,
        _timeout: Duration,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            self.runs.fetch_add(1, Ordering::SeqCst);
            let data = fs::read(binary).map_err(|error| error.to_string())?;
            if data.starts_with(b"#fail") {
                return Err("it exited with exit code: 1".into());
            }
            Ok(String::from_utf8_lossy(&data).trim().to_owned())
        })
    }
}

/// Writes an install receipt naming `binary`.
pub fn write_receipt(data: &DataDir, binary: &Path) {
    let receipt = serde_json::json!({
        "format": 1,
        "installer": "install.sh",
        "version": "0.1.0",
        "binary": binary.display().to_string(),
        "target": LINUX,
        "installed_at": "2026-10-08T12:00:00Z",
    });
    fs::create_dir_all(data.root()).unwrap();
    fs::write(data.receipt_file(), receipt.to_string()).unwrap();
}

/// An installed open-ferry 0.1.0 in a temporary directory, its receipt,
/// a release server and a key it trusts.
pub struct Fixture {
    pub temp: TempDir,
    pub server: ReleaseServer,
    pub key: TestKey,
    pub installed: PathBuf,
    pub data: DataDir,
    pub runner: Arc<FakeRunner>,
    pub updater: Updater,
}

impl Fixture {
    pub async fn new() -> Self {
        Self::for_target(LINUX).await
    }

    pub async fn for_target(target: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let server = ReleaseServer::start().await;
        let key = TestKey::new();
        let bin = temp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let installed = bin.join(release::binary_name(target));
        fs::write(&installed, good_binary("0.1.0")).unwrap();
        let data = DataDir::at(temp.path().join("data"));
        write_receipt(&data, &installed);
        let runner = Arc::new(FakeRunner::default());
        let updater = Updater {
            fetch: Arc::new(HttpFetch::new("").unwrap()),
            base: server.base.clone(),
            keys: trusting(&[&key]),
            data: data.clone(),
            system: Arc::new(FakeSystem {
                exe: installed.clone(),
                container: false,
                writable: true,
            }),
            runner: Arc::clone(&runner) as Arc<dyn Runner>,
            switch: Arc::new(ReplaceOnDisk),
            target: target.to_owned(),
            running: "0.1.0".to_owned(),
            limits: Limits::default(),
        };
        Self {
            temp,
            server,
            key,
            installed,
            data,
            runner,
            updater,
        }
    }

    /// Publishes `version` as the latest release, signed, with a good
    /// binary for this target and an archive for another.
    pub fn release(&self, version: &str) {
        self.release_binary(version, &good_binary(version));
    }

    /// Publishes `version` with `binary` as this target's binary.
    pub fn release_binary(&self, version: &str, binary: &[u8]) {
        let target = self.updater.target.as_str();
        let other = if target == MAC { LINUX } else { MAC };
        let archives = [
            (
                archive_name(version, other),
                release_archive(version, other, &good_binary(version)),
            ),
            (
                archive_name(version, target),
                release_archive(version, target, binary),
            ),
        ];
        self.server.publish(&self.key, version, &archives);
    }

    /// The installed binary's contents.
    pub fn installed_text(&self) -> String {
        fs::read_to_string(&self.installed).unwrap()
    }

    /// The path of an archive download of `version` for this target.
    pub fn archive_path(&self, version: &str) -> String {
        format!(
            "/releases/download/v{version}/{}",
            archive_name(version, &self.updater.target)
        )
    }
}

/// The two list downloads every check makes first.
pub const LIST_PATHS: [&str; 2] = [
    "/releases/latest/download/SHA256SUMS",
    "/releases/latest/download/SHA256SUMS.minisig",
];
