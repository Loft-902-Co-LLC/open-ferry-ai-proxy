//! Shared fake for runtime tests.
use super::*;
use crate::update::scope_probe::Fake;
use std::sync::Mutex;

pub(crate) struct RuntimeFake {
    fake: Mutex<Fake>,
}
impl RuntimeFake {
    pub(crate) fn new(fake: Fake) -> Self {
        Self {
            fake: Mutex::new(fake),
        }
    }
    pub(crate) fn current() -> Self {
        let platform = if cfg!(windows) {
            Platform::Windows
        } else if cfg!(target_os = "macos") {
            Platform::MacOs
        } else {
            Platform::Linux
        };
        let mut fake = Fake::new(platform, false);
        let scratch = std::env::var_os("OFP_TEST_SCRATCH")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/ofp-scratch")
            });
        fake.account = Ok(scratch
            .join("runtime-account")
            .to_string_lossy()
            .into_owned());
        Self {
            fake: Mutex::new(fake),
        }
    }
}
impl Probe for RuntimeFake {
    fn platform(&self) -> Platform {
        self.fake.lock().unwrap().platform()
    }
    fn privileged(&self) -> Result<bool, String> {
        self.fake.lock().unwrap().privileged()
    }
    fn account_dir(&self) -> Result<String, String> {
        self.fake.lock().unwrap().account_dir()
    }
    fn program_data(&self) -> Result<String, String> {
        self.fake.lock().unwrap().program_data()
    }
    fn program_files(&self) -> Result<String, String> {
        self.fake.lock().unwrap().program_files()
    }
    fn entries(&self, dir: &str, limit: usize) -> io::Result<Vec<String>> {
        self.fake.lock().unwrap().entries(dir, limit)
    }
    fn trusted_dir(&self, path: &str) -> Result<(), String> {
        self.fake.lock().unwrap().trusted_dir(path)
    }
    fn trusted_file(&self, path: &str) -> Result<(), String> {
        self.fake.lock().unwrap().trusted_file(path)
    }
    fn program_head(&self, path: &str) -> Result<Vec<u8>, String> {
        self.fake.lock().unwrap().program_head(path)
    }
    fn readable_file(&self, path: &str) -> io::Result<()> {
        self.fake.lock().unwrap().readable_file(path)
    }
    fn receipt(&self, path: &str) -> Result<Option<Receipt>, ReceiptError> {
        self.fake.lock().unwrap().receipt(path)
    }
    fn canonical(&self, path: &str) -> io::Result<String> {
        self.fake.lock().unwrap().canonical(path)
    }
    fn state(&self, path: &str) -> State {
        self.fake.lock().unwrap().state(path)
    }
    fn create_machine_dir(&mut self, _: &str) -> io::Result<()> {
        panic!("runtime tests never provision")
    }
}

/// Where tests keep scratch files: `OFP_TEST_SCRATCH` when set, else the
/// workspace's `target/ofp-scratch`. It is cleaned, since privileged checks
/// refuse a path with `..` in it, as the fallback would otherwise have.
pub(crate) fn scratch_root() -> std::path::PathBuf {
    let root = std::env::var_os("OFP_TEST_SCRATCH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ofp-scratch")
        });
    open_ferry_core::auth::path::clean(&root)
}

/// Scratch storage for tests that exercise runtime filesystem reads.
pub(crate) fn scratch_dir() -> tempfile::TempDir {
    let root = scratch_root();
    fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}
