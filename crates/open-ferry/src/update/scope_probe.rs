use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use crate::os_service::Platform;
use crate::scope::{Probe, State};
use open_ferry_update::data_dir::RECEIPT_FILE;
use open_ferry_update::receipt::{Receipt, ReceiptError};
use std::io;

/// What the fake has at a path.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Node {
    Dir,
    File,
    /// A regular file whose permissions deny a read-only open.
    Unreadable,
    /// Something this process can't look at.
    Hidden,
}

/// A machine with the paths it is given; every path in it is cleaned for
/// its platform, so Windows paths can be written with `/`.
pub(crate) struct Fake {
    pub(crate) platform: Platform,
    pub(crate) privileged: Result<bool, String>,
    pub(crate) account: Result<String, String>,
    pub(crate) program_data: Result<String, String>,
    pub(crate) nodes: BTreeMap<String, Node>,
    /// Paths the trust check refuses, with everything below them.
    pub(crate) untrusted: BTreeSet<String>,
    /// The receipts, by their paths: the binary each names.
    pub(crate) receipts: BTreeMap<String, String>,
    /// What reading the receipts at these paths fails with.
    pub(crate) receipt_errors: BTreeMap<String, ReceiptError>,
    /// The receipts read, in order.
    pub(crate) reads: RefCell<Vec<String>>,
    /// What creating a directory fails with.
    pub(crate) create: Option<io::ErrorKind>,
    pub(crate) created: Vec<String>,
}

impl Fake {
    /// The machine of a user, `privileged` or not, whose data directory is
    /// their home's.
    pub(crate) fn new(platform: Platform, privileged: bool) -> Fake {
        let account = match (platform, privileged) {
            (Platform::Windows, _) => "C:/Users/me/AppData/Local/open-ferry",
            (Platform::MacOs, _) => "/Users/me/.local/share/open-ferry",
            (Platform::Linux, true) => "/root/.local/share/open-ferry",
            (Platform::Linux, false) => "/home/me/.local/share/open-ferry",
        };
        Fake {
            platform,
            privileged: Ok(privileged),
            account: Ok(platform.clean(account)),
            program_data: Ok(platform.clean("C:/ProgramData")),
            nodes: BTreeMap::new(),
            untrusted: BTreeSet::new(),
            receipts: BTreeMap::new(),
            receipt_errors: BTreeMap::new(),
            reads: RefCell::new(Vec::new()),
            create: None,
            created: Vec::new(),
        }
    }

    fn path(&self, path: &str) -> String {
        self.platform.clean(path)
    }

    /// Adds the directory `path` and those above it.
    pub(crate) fn dir(&mut self, path: &str) -> &mut Fake {
        let mut next = Some(self.path(path));
        while let Some(path) = next {
            next = self.platform.parent(&path);
            self.nodes.entry(path).or_insert(Node::Dir);
        }
        self
    }

    /// Puts `node` at `path`, in directories made as needed.
    pub(crate) fn node(&mut self, path: &str, node: Node) -> &mut Fake {
        let path = self.path(path);
        if let Some(parent) = self.platform.parent(&path) {
            self.dir(&parent);
        }
        self.nodes.insert(path, node);
        self
    }

    pub(crate) fn file(&mut self, path: &str) -> &mut Fake {
        self.node(path, Node::File)
    }

    /// Puts a receipt naming `binary` in the data directory `dir`.
    pub(crate) fn with_receipt(&mut self, dir: &str, binary: &str) -> &mut Fake {
        let path = self.platform.join(&self.path(dir), RECEIPT_FILE);
        let binary = self.path(binary);
        self.file(&path);
        self.receipts.insert(path, binary);
        self
    }

    /// Makes the trust check refuse `path` and everything below it.
    pub(crate) fn untrust(&mut self, path: &str) -> &mut Fake {
        let path = self.path(path);
        self.untrusted.insert(path);
        self
    }

    fn trust(&self, path: &str) -> Result<(), String> {
        let path = self.path(path);
        let below = |top: &str| {
            path.strip_prefix(top).is_some_and(|rest| {
                rest.is_empty() || rest.starts_with(|c| self.platform.is_separator(c))
            })
        };
        match self.untrusted.iter().find(|top| below(top)) {
            Some(top) => Err(format!("{path} is not safe: {top} is untrusted")),
            None => Ok(()),
        }
    }
}

impl Probe for Fake {
    fn platform(&self) -> Platform {
        self.platform
    }

    fn privileged(&self) -> Result<bool, String> {
        self.privileged.clone()
    }

    fn account_dir(&self) -> Result<String, String> {
        self.account.clone()
    }

    fn program_data(&self) -> Result<String, String> {
        self.program_data.clone()
    }

    fn state(&self, path: &str) -> State {
        match self.nodes.get(&self.path(path)) {
            None => State::Missing,
            Some(Node::Dir) => State::Directory,
            Some(Node::File | Node::Unreadable) => State::File,
            Some(Node::Hidden) => State::Inaccessible("access is denied".to_owned()),
        }
    }

    fn entries(&self, dir: &str, limit: usize) -> io::Result<Vec<String>> {
        let dir = self.path(dir);
        Ok(self
            .nodes
            .keys()
            .filter(|path| self.platform.parent(path).as_deref() == Some(dir.as_str()))
            .filter_map(|path| path.rsplit(self.platform.separator()).next())
            .map(str::to_owned)
            .take(limit)
            .collect())
    }

    fn trusted_dir(&self, path: &str) -> Result<(), String> {
        self.trust(path)
    }

    fn trusted_file(&self, path: &str) -> Result<(), String> {
        if matches!(self.nodes.get(&self.path(path)), Some(Node::Unreadable)) {
            Err(format!("{path} can't be examined: access is denied"))
        } else {
            self.trust(path)
        }
    }

    fn readable_file(&self, path: &str) -> io::Result<()> {
        if matches!(self.nodes.get(&self.path(path)), Some(Node::Unreadable)) {
            Err(io::ErrorKind::PermissionDenied.into())
        } else {
            Ok(())
        }
    }

    fn receipt(&self, path: &str) -> Result<Option<Receipt>, ReceiptError> {
        let path = self.path(path);
        self.reads.borrow_mut().push(path.clone());
        if let Some(error) = self.receipt_errors.get(&path) {
            return Err(error.clone());
        }
        match (self.receipts.get(&path), self.nodes.contains_key(&path)) {
            (Some(binary), _) => Ok(Some(Receipt {
                binary: binary.clone(),
                ..Receipt::default()
            })),
            (None, true) => Err(ReceiptError::Malformed("not a receipt".to_owned())),
            (None, false) => Ok(None),
        }
    }

    fn canonical(&self, path: &str) -> io::Result<String> {
        let path = self.path(path);
        if self.nodes.contains_key(&path) {
            Ok(path)
        } else {
            Err(io::ErrorKind::NotFound.into())
        }
    }

    fn create_machine_dir(&mut self, path: &str) -> io::Result<()> {
        if let Some(kind) = self.create {
            return Err(kind.into());
        }
        let path = self.path(path);
        if self.nodes.contains_key(&path) {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        self.created.push(path.clone());
        self.dir(&path);
        Ok(())
    }
}
