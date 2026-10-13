use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use super::*;

const BINARY: &str = "/usr/local/bin/open-ferry";
const MACHINE: &str = "/var/lib/open-ferry";
const ROOT_ACCOUNT: &str = "/root/.local/share/open-ferry";
const USER_ACCOUNT: &str = "/home/me/.local/share/open-ferry";
const WINDOWS_BINARY: &str = "C:/Program Files/open-ferry/open-ferry.exe";
const WINDOWS_ACCOUNT: &str = "C:/Users/me/AppData/Local/open-ferry";

/// What the fake has at a path.
#[derive(Clone, Copy, Debug)]
enum Node {
    Dir,
    File,
    Link,
    /// Something this process can't look at.
    Hidden,
}

/// A machine with the paths it is given; every path in it is cleaned for
/// its platform, so Windows paths can be written with `/`.
pub(crate) struct Fake {
    platform: Platform,
    privileged: Result<bool, String>,
    account: Result<String, String>,
    program_data: Result<String, String>,
    nodes: BTreeMap<String, Node>,
    /// Paths the trust check refuses, with everything below them.
    untrusted: BTreeSet<String>,
    /// The receipts, by their paths: the binary each names.
    receipts: BTreeMap<String, String>,
    /// What reading the receipts at these paths fails with.
    receipt_errors: BTreeMap<String, ReceiptError>,
    /// The receipts read, in order.
    reads: RefCell<Vec<String>>,
    listed: RefCell<Vec<String>>,
    /// What creating a directory fails with.
    create: Option<io::ErrorKind>,
    created: Vec<String>,
    /// Whether it is a container.
    pub(crate) container: bool,
    aliases: BTreeMap<String, String>,
}

impl Fake {
    /// The machine of a user, `privileged` or not, whose data directory is
    /// their home's.
    pub(crate) fn new(platform: Platform, privileged: bool) -> Fake {
        let account = match (platform, privileged) {
            (Platform::Windows, _) => WINDOWS_ACCOUNT,
            (Platform::MacOs, _) => "/Users/me/.local/share/open-ferry",
            (Platform::Linux, true) => ROOT_ACCOUNT,
            (Platform::Linux, false) => USER_ACCOUNT,
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
            listed: RefCell::new(Vec::new()),
            create: None,
            created: Vec::new(),
            container: false,
            aliases: BTreeMap::new(),
        }
    }

    fn path(&self, path: &str) -> String {
        self.platform.clean(path)
    }

    /// Adds the directory `path` and those above it.
    fn dir(&mut self, path: &str) -> &mut Fake {
        let mut next = Some(self.path(path));
        while let Some(path) = next {
            next = self.platform.parent(&path);
            self.nodes.entry(path).or_insert(Node::Dir);
        }
        self
    }

    /// Puts `node` at `path`, in directories made as needed.
    fn node(&mut self, path: &str, node: Node) -> &mut Fake {
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
    fn with_receipt(&mut self, dir: &str, binary: &str) -> &mut Fake {
        let path = self.platform.join(&self.path(dir), RECEIPT_FILE);
        let binary = self.path(binary);
        self.file(&path);
        self.receipts.insert(path, binary);
        self
    }

    /// Puts a receipt that fails to read with `error` in the data directory
    /// `dir`.
    fn with_receipt_error(&mut self, dir: &str, error: ReceiptError) -> &mut Fake {
        let path = self.platform.join(&self.path(dir), RECEIPT_FILE);
        self.file(&path);
        self.receipt_errors.insert(path, error);
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
        if self
            .nodes
            .iter()
            .any(|(top, node)| matches!(node, Node::Link) && below(top))
        {
            return Err(format!("{path} has a link in its path"));
        }
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
            Some(Node::File) => State::File,
            Some(Node::Link) => State::Other,
            Some(Node::Hidden) => State::Inaccessible("access is denied".to_owned()),
        }
    }

    fn entries(&self, dir: &str, limit: usize) -> io::Result<Vec<String>> {
        let dir = self.path(dir);
        self.listed.borrow_mut().push(dir.clone());
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
        self.trust(path)
    }

    fn readable_file(&self, _path: &str) -> io::Result<()> {
        Ok(())
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
            Ok(self.aliases.get(&path).cloned().unwrap_or(path))
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

    fn in_container(&self) -> bool {
        self.container
    }
}

fn found(result: Result<Home, Refusal>) -> Home {
    match result {
        Ok(home) => home,
        Err(refusal) => panic!("refused: {refusal}"),
    }
}

fn refused<T: fmt::Debug>(result: Result<T, Refusal>) -> Refusal {
    match result {
        Ok(value) => panic!("not refused: {value:?}"),
        Err(refusal) => refusal,
    }
}

fn root_of(home: &Home) -> String {
    home.data.root().to_string_lossy().into_owned()
}

const ACCOUNT_REMEDY: &str =
    "Run it without sudo, or install open-ferry for the whole machine with `install.sh --system`.";
const MACHINE_REMEDY: &str = "Stop the open-ferry service, then as root fix what is named, or move /var/lib/open-ferry aside and install again with `install.sh --system`; keep its auth directory private, since it holds the machine's credentials.";

#[test]
fn an_unprivileged_process_uses_its_account_records_as_they_are() {
    let mut fake = Fake::new(Platform::Linux, false);
    fake.with_receipt(USER_ACCOUNT, BINARY)
        .file(BINARY)
        .untrust("/home/me");
    let home = found(lookup(&fake, BINARY));
    assert_eq!(home.scope, Scope::Account);
    assert_eq!(root_of(&home), USER_ACCOUNT);
    assert_eq!(home.kind, ServiceKind::User);
    assert!(!home.privileged);
    assert_eq!(home.machine_hint, None);
    assert_eq!(home.require_privilege(), Ok(()));
    assert_eq!(authorize(&fake, USER_ACCOUNT), Ok(()));
}

#[test]
fn root_uses_the_machine_records_that_name_its_binary() {
    let mut fake = Fake::new(Platform::Linux, true);
    fake.with_receipt(MACHINE, BINARY)
        .file(BINARY)
        .file("/var/lib/open-ferry/versions/1.2.3/open-ferry")
        .file("/var/lib/open-ferry/update-state.json")
        .untrust("/root");
    let home = found(lookup(&fake, BINARY));
    assert_eq!(home.scope, Scope::Machine);
    assert_eq!(root_of(&home), MACHINE);
    assert_eq!(home.kind, ServiceKind::System);
    assert!(home.privileged);
    assert_eq!(home.require_privilege(), Ok(()));
}

#[test]
fn root_uses_its_own_records_when_the_machine_names_another_binary() {
    let mut fake = Fake::new(Platform::Linux, true);
    fake.with_receipt(MACHINE, "/opt/other/open-ferry")
        .file("/opt/other/open-ferry")
        .file(BINARY)
        .dir(ROOT_ACCOUNT);
    let home = found(lookup(&fake, BINARY));
    assert_eq!(home.scope, Scope::Account);
    assert_eq!(root_of(&home), ROOT_ACCOUNT);
    assert_eq!(home.kind, ServiceKind::User);
}

#[test]
fn root_refuses_account_records_another_user_can_change() {
    // sudo kept the user's HOME.
    let mut fake = Fake::new(Platform::Linux, true);
    fake.account = Ok(USER_ACCOUNT.to_owned());
    fake.with_receipt(USER_ACCOUNT, BINARY)
        .file(BINARY)
        .untrust("/home/me");
    let refusal = refused(lookup(&fake, BINARY));
    assert!(refusal.reason.contains("/home/me"), "{refusal}");
    assert_eq!(refusal.remedy.as_deref(), Some(ACCOUNT_REMEDY));
    assert_eq!(refused(authorize(&fake, USER_ACCOUNT)), refusal);
}

#[test]
fn an_untrusted_machine_directory_refuses_root_unless_its_account_names_the_binary() {
    let mut fake = Fake::new(Platform::Linux, true);
    fake.with_receipt(MACHINE, BINARY)
        .file(BINARY)
        .dir(ROOT_ACCOUNT)
        .untrust(MACHINE);
    let refusal = refused(lookup(&fake, BINARY));
    assert!(refusal.reason.contains(MACHINE), "{refusal}");
    assert_eq!(refusal.remedy.as_deref(), Some(MACHINE_REMEDY));

    fake.with_receipt(ROOT_ACCOUNT, BINARY);
    let home = found(lookup(&fake, BINARY));
    assert_eq!(home.scope, Scope::Account);
    assert_eq!(root_of(&home), ROOT_ACCOUNT);

    // Not when that account is untrusted too: the machine's refusal stands.
    fake.untrust("/root");
    assert_eq!(refused(lookup(&fake, BINARY)), refusal);
}

#[test]
fn a_machine_receipt_the_trust_check_refuses_is_not_read() {
    let mut fake = Fake::new(Platform::Linux, true);
    fake.with_receipt(MACHINE, BINARY)
        .file(BINARY)
        .dir(ROOT_ACCOUNT)
        .untrust("/var/lib/open-ferry/install-receipt.json");
    let refusal = refused(lookup(&fake, BINARY));
    assert!(refusal.reason.contains("install-receipt.json"), "{refusal}");
    assert_eq!(refusal.remedy.as_deref(), Some(MACHINE_REMEDY));
    let reads = fake.reads.borrow().clone();
    assert!(
        !reads.iter().any(|path| path.starts_with(MACHINE)),
        "{reads:?}"
    );

    // An unprivileged process just uses its own records.
    fake.privileged = Ok(false);
    let home = found(lookup(&fake, BINARY));
    assert_eq!(home.scope, Scope::Account);
    assert_eq!(home.machine_hint, None);
}

#[test]
fn a_machine_directory_that_is_not_a_directory_refuses_root() {
    let mut fake = Fake::new(Platform::Linux, true);
    fake.node(MACHINE, Node::Link)
        .file(BINARY)
        .dir(ROOT_ACCOUNT);
    let refusal = refused(lookup(&fake, BINARY));
    assert_eq!(
        refusal.reason,
        "/var/lib/open-ferry is not a real directory"
    );
}

#[test]
fn a_user_who_cannot_read_the_machine_directory_gets_a_hint() {
    let mut fake = Fake::new(Platform::Linux, false);
    fake.node(MACHINE, Node::Hidden)
        .file(BINARY)
        .dir(USER_ACCOUNT);
    let home = found(lookup(&fake, BINARY));
    assert_eq!(home.scope, Scope::Account);
    assert_eq!(root_of(&home), USER_ACCOUNT);
    assert_eq!(
        home.machine_hint.as_deref(),
        Some("This open-ferry looks installed for the whole machine: run it with sudo.")
    );

    // Not for a binary a user could change.
    fake.untrust("/usr/local/bin");
    assert_eq!(found(lookup(&fake, BINARY)).machine_hint, None);

    // Not when the account's own receipt names the binary.
    let mut fake = Fake::new(Platform::Linux, false);
    fake.node(MACHINE, Node::Hidden)
        .file(BINARY)
        .with_receipt(USER_ACCOUNT, BINARY);
    assert_eq!(found(lookup(&fake, BINARY)).machine_hint, None);
}

#[test]
fn root_that_cannot_read_the_machine_directory_needs_a_trusted_account_naming_the_binary() {
    let mut fake = Fake::new(Platform::Linux, true);
    fake.node(MACHINE, Node::Hidden)
        .file(BINARY)
        .with_receipt(ROOT_ACCOUNT, BINARY)
        .untrust("/root");
    let refusal = refused(lookup(&fake, BINARY));
    assert_eq!(
        refusal.reason,
        "/var/lib/open-ferry can't be looked at: access is denied"
    );
    assert_eq!(refusal.remedy.as_deref(), Some(MACHINE_REMEDY));
    // The untrusted account's receipt isn't read.
    assert!(fake.reads.borrow().is_empty(), "{:?}", fake.reads.borrow());

    // A trusted account whose receipt names the binary is used.
    fake.untrusted.clear();
    let home = found(lookup(&fake, BINARY));
    assert_eq!(home.scope, Scope::Account);
    assert_eq!(root_of(&home), ROOT_ACCOUNT);
    assert_eq!(home.machine_hint, None);

    // One whose receipt names nothing isn't.
    let mut fake = Fake::new(Platform::Linux, true);
    fake.node(MACHINE, Node::Hidden)
        .file(BINARY)
        .dir(ROOT_ACCOUNT);
    assert_eq!(refused(lookup(&fake, BINARY)), refusal);

    // Nor is an account without a data directory.
    fake.account = Err("HOME isn't set to an absolute path".to_owned());
    assert_eq!(refused(lookup(&fake, BINARY)), refusal);
}

#[test]
fn a_machine_receipt_that_cannot_be_read_or_is_not_valid_refuses_root() {
    let receipt = "/var/lib/open-ferry/install-receipt.json";
    let mut fake = Fake::new(Platform::Linux, true);
    fake.node(receipt, Node::Hidden)
        .file(BINARY)
        .dir(ROOT_ACCOUNT);
    let refusal = refused(lookup(&fake, BINARY));
    assert_eq!(
        refusal.reason,
        format!("{receipt} can't be looked at: access is denied")
    );
    assert_eq!(refusal.remedy.as_deref(), Some(MACHINE_REMEDY));

    let mut fake = Fake::new(Platform::Linux, true);
    fake.with_receipt_error(MACHINE, ReceiptError::Unreadable("I/O error".to_owned()))
        .file(BINARY)
        .dir(ROOT_ACCOUNT);
    let refusal = refused(lookup(&fake, BINARY));
    assert_eq!(
        refusal.reason,
        format!("{receipt} can't be read: I/O error")
    );
    assert_eq!(refusal.remedy.as_deref(), Some(MACHINE_REMEDY));
    // A user gets the hint, as for a machine directory it can't read.
    fake.privileged = Ok(false);
    fake.account = Ok(USER_ACCOUNT.to_owned());
    assert!(found(lookup(&fake, BINARY)).machine_hint.is_some());

    let mut fake = Fake::new(Platform::Linux, true);
    fake.with_receipt_error(
        MACHINE,
        ReceiptError::Malformed("format 2 isn't 1".to_owned()),
    )
    .file(BINARY)
    .dir(ROOT_ACCOUNT);
    let refusal = refused(lookup(&fake, BINARY));
    assert_eq!(
        refusal.reason,
        format!("{receipt} isn't a valid install receipt: format 2 isn't 1")
    );
    assert_eq!(refusal.remedy.as_deref(), Some(MACHINE_REMEDY));
    // Unless the account's own receipt names the binary.
    fake.with_receipt(ROOT_ACCOUNT, BINARY);
    assert_eq!(found(lookup(&fake, BINARY)).scope, Scope::Account);
    // A user just uses its own records.
    fake.privileged = Ok(false);
    let home = found(lookup(&fake, BINARY));
    assert_eq!(home.scope, Scope::Account);
    assert_eq!(home.machine_hint, None);
}

#[test]
fn the_account_data_directory_is_used_as_it_is_given() {
    // Unprivileged, as `DataDir::for_this_user` gives it.
    let mut fake = Fake::new(Platform::Windows, false);
    fake.account = Ok(WINDOWS_ACCOUNT.to_owned());
    fake.file(WINDOWS_BINARY).dir("C:/ProgramData");
    assert_eq!(
        root_of(&found(lookup(&fake, WINDOWS_BINARY))),
        WINDOWS_ACCOUNT
    );

    // Privileged, a `..` in it refuses rather than being folded away.
    let account = "/root/.local/share/../share/open-ferry";
    let mut fake = Fake::new(Platform::Linux, true);
    fake.account = Ok(account.to_owned());
    fake.file(BINARY).dir(ROOT_ACCOUNT);
    let refusal = refused(lookup(&fake, BINARY));
    assert_eq!(
        refusal.reason,
        format!("{account} is not a full path without `.` or `..`")
    );
    assert_eq!(refusal.remedy.as_deref(), Some(ACCOUNT_REMEDY));
}

#[test]
fn anyone_may_read_a_readable_machine_install_but_only_privilege_changes_it() {
    let mut fake = Fake::new(Platform::Linux, false);
    fake.with_receipt(MACHINE, BINARY)
        .file(BINARY)
        .untrust("/home");
    let home = found(lookup(&fake, BINARY));
    assert_eq!(home.scope, Scope::Machine);
    assert_eq!(home.kind, ServiceKind::System);
    assert!(!home.privileged);
    let refusal = refused(home.require_privilege());
    assert_eq!(
        refusal.reason,
        "open-ferry is installed for the whole machine, so changing it needs root"
    );
    assert_eq!(refusal.remedy.as_deref(), Some("Run it with sudo."));
}

#[test]
fn macos_refuses_every_privileged_process() {
    let account = "/Users/me/.local/share/open-ferry";
    let mut fake = Fake::new(Platform::MacOs, true);
    fake.with_receipt(account, BINARY).file(BINARY);
    let refusal = refused(lookup(&fake, BINARY));
    assert_eq!(refusal, macos_refusal());
    assert_eq!(refused(authorize(&fake, account)), refusal);
    assert_eq!(refused(provision(&mut fake)), refusal);
    assert_eq!(refused(machine_auth_dir(&fake)), refusal);

    fake.privileged = Ok(false);
    let home = found(lookup(&fake, BINARY));
    assert_eq!(home.scope, Scope::Account);
    assert_eq!(root_of(&home), account);
    assert_eq!(authorize(&fake, account), Ok(()));
    assert_eq!(machine_root(&fake), Ok(None));
}

#[test]
fn an_elevated_windows_process_refuses_records_its_user_can_change() {
    let mut fake = Fake::new(Platform::Windows, true);
    let binary = "C:/Users/me/AppData/Local/Programs/open-ferry/open-ferry.exe";
    fake.with_receipt(WINDOWS_ACCOUNT, binary)
        .file(binary)
        .dir("C:/ProgramData")
        .untrust("C:/Users/me");
    let refusal = refused(lookup(&fake, binary));
    assert_eq!(
        refusal.remedy.as_deref(),
        Some(
            "Run it in a terminal not opened as administrator, or install open-ferry for the whole machine with `install.ps1 -System`."
        )
    );

    // Not elevated, it uses them.
    fake.privileged = Ok(false);
    let home = found(lookup(&fake, binary));
    assert_eq!(home.scope, Scope::Account);
    assert_eq!(root_of(&home), Platform::Windows.clean(WINDOWS_ACCOUNT));
}

#[test]
fn windows_machine_records_are_in_program_data() {
    let mut fake = Fake::new(Platform::Windows, true);
    let machine = Platform::Windows.clean("C:/ProgramData/open-ferry");
    fake.with_receipt(&machine, WINDOWS_BINARY)
        .file(WINDOWS_BINARY)
        .untrust("C:/Users/me");
    assert_eq!(machine_root(&fake), Ok(Some(machine.clone())));
    let home = found(lookup(&fake, WINDOWS_BINARY));
    assert_eq!(home.scope, Scope::Machine);
    assert_eq!(root_of(&home), machine);
    assert_eq!(home.kind, ServiceKind::System);

    // The machine's remedy names the directory and the Windows script.
    fake.untrust(&machine);
    let refusal = refused(lookup(&fake, WINDOWS_BINARY));
    assert_eq!(
        refusal.remedy,
        Some(format!(
            "Stop the open-ferry service, then as an administrator fix what is named, or move {machine} aside and install again with `install.ps1 -System`; keep its auth directory private, since it holds the machine's credentials."
        ))
    );

    // ProgramData must be a full path.
    fake.program_data = Ok("ProgramData".to_owned());
    assert!(machine_root(&fake).is_err());
}

#[test]
fn a_missing_data_directory_passes_when_the_directory_above_it_does() {
    let mut fake = Fake::new(Platform::Linux, true);
    fake.dir("/root").file(BINARY);
    let home = found(lookup(&fake, BINARY));
    assert_eq!(root_of(&home), ROOT_ACCOUNT);

    fake.dir("/root/.local").untrust("/root/.local");
    let refusal = refused(lookup(&fake, BINARY));
    assert!(refusal.reason.contains("/root/.local"), "{refusal}");
}

#[test]
fn authorize_checks_everything_below_the_directory() {
    let base = || {
        let mut fake = Fake::new(Platform::Linux, true);
        fake.file("/var/lib/open-ferry/versions/1.2.3/open-ferry")
            .file("/var/lib/open-ferry/update-state.json")
            .file("/var/lib/open-ferry/auth/codex-me.json");
        fake
    };
    assert_eq!(authorize(&base(), MACHINE), Ok(()));

    let mut fake = base();
    fake.untrust("/var/lib/open-ferry/versions/1.2.3/open-ferry");
    let refusal = refused(authorize(&fake, MACHINE));
    assert!(refusal.reason.contains("1.2.3/open-ferry"), "{refusal}");
    assert_eq!(refusal.remedy.as_deref(), Some(MACHINE_REMEDY));

    let mut fake = base();
    fake.untrust("/var/lib/open-ferry/auth");
    assert!(refused(authorize(&fake, MACHINE)).reason.contains("/auth"));

    let mut fake = base();
    fake.node("/var/lib/open-ferry/versions/1.2.4", Node::Link);
    assert_eq!(
        refused(authorize(&fake, MACHINE)).reason,
        "/var/lib/open-ferry/versions/1.2.4 is a link or a special file"
    );

    let mut fake = base();
    fake.node("/var/lib/open-ferry/update.lock", Node::Hidden);
    assert!(
        refused(authorize(&fake, MACHINE))
            .reason
            .contains("update.lock can't be looked at")
    );

    // An unprivileged process checks nothing.
    let mut fake = base();
    fake.privileged = Ok(false);
    fake.untrust("/var");
    assert_eq!(authorize(&fake, MACHINE), Ok(()));
}

#[test]
fn authorize_gives_up_on_a_directory_too_big_or_too_deep() {
    let mut fake = Fake::new(Platform::Linux, true);
    for index in 0..MAX_ENTRIES {
        fake.file(&format!("/var/lib/open-ferry/{index}"));
    }
    assert_eq!(authorize(&fake, MACHINE), Ok(()));
    fake.file("/var/lib/open-ferry/one-more");
    assert_eq!(
        refused(authorize(&fake, MACHINE)).reason,
        format!("/var/lib/open-ferry holds more than {MAX_ENTRIES} entries")
    );

    let deep = |levels: usize| {
        let mut fake = Fake::new(Platform::Linux, true);
        fake.dir(&format!("{MACHINE}{}", "/d".repeat(levels)));
        fake
    };
    assert_eq!(authorize(&deep(MAX_DEPTH), MACHINE), Ok(()));
    let refusal = refused(authorize(&deep(MAX_DEPTH + 1), MACHINE));
    assert!(
        refusal
            .reason
            .ends_with(&format!("is more than {MAX_DEPTH} levels below {MACHINE}")),
        "{refusal}"
    );
}

#[test]
fn authorize_takes_only_a_plain_full_path() {
    let mut fake = Fake::new(Platform::Linux, true);
    fake.dir(ROOT_ACCOUNT);
    for path in [
        "root/.local/share/open-ferry",
        "/root/.local/share/../share/open-ferry",
        "/root/./.local/share/open-ferry",
    ] {
        let refusal = refused(authorize(&fake, path));
        assert!(
            refusal
                .reason
                .ends_with("is not a full path without `.` or `..`"),
            "{path}: {refusal}"
        );
    }
    assert_eq!(authorize(&fake, "/root/.local/share/open-ferry/"), Ok(()));
}

#[test]
fn a_privilege_check_that_fails_refuses() {
    let mut fake = Fake::new(Platform::Linux, true);
    fake.dir(ROOT_ACCOUNT).file(BINARY);
    fake.privileged = Err("no token".to_owned());
    let refusal = refused(lookup(&fake, BINARY));
    assert_eq!(
        refusal.reason,
        "failed to tell whether this process is privileged: no token"
    );
    assert_eq!(refused(authorize(&fake, ROOT_ACCOUNT)), refusal);
    assert_eq!(refused(provision(&mut fake)), refusal);
}

#[test]
fn an_account_without_a_data_directory_refuses_unless_the_machine_is_used() {
    let mut fake = Fake::new(Platform::Linux, true);
    fake.account = Err("HOME isn't set to an absolute path".to_owned());
    fake.file(BINARY).dir("/var/lib");
    assert_eq!(
        refused(lookup(&fake, BINARY)).reason,
        "HOME isn't set to an absolute path"
    );
    fake.with_receipt(MACHINE, BINARY);
    assert_eq!(found(lookup(&fake, BINARY)).scope, Scope::Machine);
}

#[test]
fn provisioning_needs_privilege() {
    let mut fake = Fake::new(Platform::Linux, false);
    fake.dir("/var/lib");
    let refusal = refused(provision(&mut fake));
    assert_eq!(
        refusal.reason,
        "installing open-ferry for the whole machine needs root"
    );
    assert_eq!(refusal.remedy.as_deref(), Some("Run it with sudo."));

    let mut fake = Fake::new(Platform::Windows, false);
    let refusal = refused(provision(&mut fake));
    assert_eq!(
        refusal.remedy.as_deref(),
        Some("Run it in a terminal opened as administrator.")
    );
    assert!(fake.created.is_empty());
}

#[test]
fn provisioning_creates_the_machine_directory_or_finds_a_trusted_one() {
    let mut fake = Fake::new(Platform::Linux, true);
    fake.dir("/var/lib");
    let home = found(provision(&mut fake));
    assert_eq!(home.scope, Scope::Machine);
    assert_eq!(root_of(&home), MACHINE);
    assert_eq!(home.kind, ServiceKind::System);
    assert_eq!(fake.created, [MACHINE]);

    // Again, it is found, not made.
    fake.file("/var/lib/open-ferry/install-receipt.json");
    assert_eq!(found(provision(&mut fake)), home);
    assert_eq!(fake.created, [MACHINE]);

    // Made by a concurrent install between the check and the creation.
    let mut fake = Fake::new(Platform::Linux, true);
    fake.dir(MACHINE);
    fake.create = Some(io::ErrorKind::AlreadyExists);
    assert_eq!(found(provision(&mut fake)), home);

    // Windows: in ProgramData, from the known folder.
    let mut fake = Fake::new(Platform::Windows, true);
    fake.dir("C:/ProgramData");
    let machine = Platform::Windows.clean("C:/ProgramData/open-ferry");
    assert_eq!(root_of(&found(provision(&mut fake))), machine);
    assert_eq!(fake.created, [machine]);
}

#[test]
fn provisioning_never_adopts_a_directory_it_cannot_trust() {
    let mut fake = Fake::new(Platform::Linux, true);
    fake.file("/var/lib/open-ferry/install-receipt.json")
        .untrust(MACHINE);
    let refusal = refused(provision(&mut fake));
    assert_eq!(refusal.remedy.as_deref(), Some(MACHINE_REMEDY));
    assert!(fake.created.is_empty());

    // Made under a parent that is untrusted: the empty directory stays, refused.
    let mut fake = Fake::new(Platform::Linux, true);
    fake.dir("/var/lib").untrust("/var/lib");
    refused(provision(&mut fake));
    assert_eq!(fake.created, [MACHINE]);

    let mut fake = Fake::new(Platform::Linux, true);
    fake.create = Some(io::ErrorKind::PermissionDenied);
    let refusal = refused(provision(&mut fake));
    assert!(
        refusal
            .reason
            .starts_with("failed to create /var/lib/open-ferry: "),
        "{refusal}"
    );
}

#[test]
fn the_machine_auth_directory_is_in_the_machine_data_directory() {
    let mut fake = Fake::new(Platform::Linux, true);
    let refusal = refused(machine_auth_dir(&fake));
    assert_eq!(refusal.reason, "/var/lib/open-ferry doesn't exist yet");
    assert_eq!(
        refusal.remedy.as_deref(),
        Some("Install open-ferry for the whole machine with `install.sh --system` first.")
    );

    fake.dir(MACHINE);
    assert_eq!(
        machine_auth_dir(&fake),
        Ok("/var/lib/open-ferry/auth".to_owned())
    );

    fake.untrust(MACHINE);
    assert_eq!(
        refused(machine_auth_dir(&fake)).remedy.as_deref(),
        Some(MACHINE_REMEDY)
    );

    fake.privileged = Ok(false);
    assert_eq!(
        refused(machine_auth_dir(&fake)).reason,
        "writing a config for the whole machine needs root"
    );

    let mut fake = Fake::new(Platform::Windows, true);
    fake.dir("C:/ProgramData/open-ferry");
    assert_eq!(
        machine_auth_dir(&fake),
        Ok(Platform::Windows.clean("C:/ProgramData/open-ferry/auth"))
    );
}

#[test]
fn machine_configs_are_where_the_install_scripts_put_them() {
    assert_eq!(
        machine_config(&Fake::new(Platform::Linux, true))
            .unwrap()
            .as_deref(),
        Some("/etc/open-ferry/config.yaml")
    );
    assert_eq!(
        machine_config(&Fake::new(Platform::Windows, true)).unwrap(),
        Some(Platform::Windows.clean("C:/ProgramData/open-ferry/config.yaml"))
    );
    assert_eq!(
        machine_config(&Fake::new(Platform::MacOs, true)).unwrap(),
        None
    );
    let mut probe = Fake::new(Platform::Windows, true);
    probe.program_data = Ok("D:/MachineData".to_owned());
    assert_eq!(
        machine_config(&probe).unwrap(),
        Some(Platform::Windows.clean("D:/MachineData/open-ferry/config.yaml"))
    );
    probe.program_data = Err("known folder unavailable".to_owned());
    assert_eq!(
        machine_config(&probe),
        Err("known folder unavailable".to_owned())
    );
}

#[test]
fn refusals_read_as_a_reason_then_a_remedy() {
    let refusal = Refusal {
        reason: "/home/me is not safe".to_owned(),
        remedy: Some("Run it without sudo.".to_owned()),
    };
    assert_eq!(
        refusal.to_string(),
        "/home/me is not safe. Run it without sudo."
    );
    let refusal = Refusal {
        reason: "no ProgramData".to_owned(),
        remedy: None,
    };
    assert_eq!(refusal.to_string(), "no ProgramData");
}

#[test]
fn the_host_tells_directories_files_and_nothing_apart() {
    let dir = env!("CARGO_MANIFEST_DIR");
    assert_eq!(Host.state(dir), State::Directory);
    let manifest = Path::new(dir).join("Cargo.toml");
    assert_eq!(Host.state(&manifest.to_string_lossy()), State::File);
    let missing = Path::new(dir).join("no-such-file");
    assert_eq!(Host.state(&missing.to_string_lossy()), State::Missing);
    let names = Host.entries(dir, 1000).unwrap();
    assert!(names.iter().any(|name| name == "Cargo.toml"), "{names:?}");
    assert_eq!(Host.entries(dir, 1).unwrap().len(), 1);
    assert_eq!(Some(Host.platform()), Platform::HOST);
}

#[test]
fn a_privileged_process_takes_settings_only_from_a_trusted_file() {
    for (platform, path) in [
        (Platform::Linux, "/work/config.yaml"),
        (Platform::Windows, "C:/work/config.yaml"),
    ] {
        let mut probe = Fake::new(platform, true);
        let file = Path::new(path);
        let dir = platform.parent(&platform.clean(path)).unwrap();
        // Nothing there: nothing to read, whatever the directory.
        probe.untrust(&dir);
        assert_eq!(authorize_file(&probe, file), Ok(false));
        probe.file(path);
        let refusal = refused(authorize_file(&probe, file));
        assert!(refusal.reason.starts_with("Refusing to load"), "{refusal}");
        assert!(refusal.reason.contains(path), "{refusal}");
        assert!(refusal.remedy.is_some());
        probe.untrusted.clear();
        assert_eq!(authorize_file(&probe, file), Ok(true));
        probe.untrust(path);
        refused(authorize_file(&probe, file));
        probe.untrusted.clear();
        probe.privileged = Err("no token".to_owned());
        refused(authorize_file(&probe, file));
        probe.privileged = Ok(false);
        probe.untrust(path);
        assert_eq!(authorize_file(&probe, file), Ok(true));
    }
}

#[test]
fn a_privileged_process_takes_settings_only_from_a_full_plain_path() {
    let mut probe = Fake::new(Platform::Linux, true);
    probe.file("/work/config.yaml");
    for path in [
        "config.yaml",
        "/work/./config.yaml",
        "/work/x/../config.yaml",
    ] {
        let refusal = refused(authorize_file(&probe, Path::new(path)));
        assert!(refusal.reason.contains("full path"), "{refusal}");
    }
}

#[test]
fn a_container_takes_settings_from_any_file() {
    let mut probe = Fake::new(Platform::Linux, true);
    probe.file("/work/.env").untrust("/work");
    probe.container = true;
    assert_eq!(authorize_file(&probe, Path::new("/work/.env")), Ok(true));
}

#[test]
fn macos_refuses_every_settings_file_a_privileged_process_finds() {
    let mut probe = Fake::new(Platform::MacOs, true);
    let file = Path::new("/work/.env");
    assert_eq!(authorize_file(&probe, file), Ok(false));
    probe.file("/work/.env");
    let refusal = refused(authorize_file(&probe, file));
    assert!(refusal.reason.contains("/work/.env"), "{refusal}");
    assert!(refusal.reason.contains("macOS"), "{refusal}");
    probe.privileged = Ok(false);
    assert_eq!(authorize_file(&probe, file), Ok(true));
}

#[test]
fn the_auth_directory_stays_out_of_the_machine_records() {
    let linux = Platform::Linux;
    for auth in [
        "/",
        "/var/lib",
        MACHINE,
        "/var/lib/open-ferry/",
        "/var/lib/open-ferry/versions",
    ] {
        let refusal = refused(check_auth_dir_placement(linux, MACHINE, auth));
        assert!(refusal.reason.contains(MACHINE), "{refusal}");
        assert!(
            refusal.remedy.unwrap().contains("/var/lib/open-ferry/auth"),
            "{auth}"
        );
    }
    for auth in [
        "/var/lib/open-ferry/auth",
        "/var/lib/open-ferry/auth/team",
        "/var/lib/open-ferry-two",
        "/srv/open-ferry/auth",
    ] {
        assert_eq!(
            check_auth_dir_placement(linux, MACHINE, auth),
            Ok(()),
            "{auth}"
        );
    }
    let windows = Platform::Windows;
    let machine = "C:/ProgramData/open-ferry";
    for auth in [
        "C:/ProgramData",
        "c:/programdata/OPEN-FERRY",
        "C:/ProgramData/open-ferry/logs",
    ] {
        refused(check_auth_dir_placement(windows, machine, auth));
    }
    for auth in ["C:/ProgramData/Open-Ferry/AUTH", "D:/open-ferry/auth"] {
        assert_eq!(
            check_auth_dir_placement(windows, machine, auth),
            Ok(()),
            "{auth}"
        );
    }
}

#[test]
fn the_host_is_a_container_only_by_its_files() {
    #[cfg(windows)]
    assert!(!Host.in_container());
    #[cfg(unix)]
    assert_eq!(
        Host.in_container(),
        CONTAINER_MARKERS
            .iter()
            .any(|path| Path::new(path).symlink_metadata().is_ok())
    );
}

#[test]
fn request_logs_do_not_count_toward_the_record_tree_limit() {
    for (platform, root) in [
        (Platform::Linux, MACHINE),
        (Platform::Windows, "C:/ProgramData/open-ferry"),
    ] {
        for name in ["logs", "auth/logs", "auth/team/logs"] {
            let mut probe = Fake::new(platform, true);
            let logs = platform.join(root, name);
            for index in 0..=MAX_ENTRIES {
                probe.file(&platform.join(&logs, &format!("request-{index}.log")));
            }
            assert_eq!(authorize(&probe, root), Ok(()), "{logs}");
            assert!(
                !probe.listed.borrow().contains(&platform.clean(&logs)),
                "{logs}"
            );
            probe.untrust(&logs);
            let refusal = refused(authorize(&probe, root));
            assert!(refusal.reason.contains("untrusted"), "{refusal}");
        }
    }
}

#[test]
fn a_log_directory_among_versions_is_still_walked() {
    let mut probe = Fake::new(Platform::Linux, true);
    let logs = format!("{MACHINE}/versions/logs");
    for index in 0..=MAX_ENTRIES {
        probe.file(&format!("{logs}/request-{index}.log"));
    }
    let refusal = refused(authorize(&probe, MACHINE));
    assert!(
        refusal.reason.contains("more than 10000 entries"),
        "{refusal}"
    );
    let mut probe = Fake::new(Platform::Linux, true);
    probe
        .file(&format!("{logs}/record"))
        .untrust(&format!("{logs}/record"));
    let refusal = refused(authorize(&probe, MACHINE));
    assert!(refusal.reason.contains("untrusted"), "{refusal}");
}

#[test]
fn runtime_auth_placement_checks_privilege_and_the_known_machine_root() {
    for (platform, root) in [
        (Platform::Linux, MACHINE),
        (Platform::Windows, "C:/ProgramData/open-ferry"),
    ] {
        let mut probe = Fake::new(platform, true);
        probe.dir(root);
        refused(authorize_auth_dir(&probe, Path::new(root)));
        assert_eq!(
            authorize_auth_dir(&probe, Path::new(&platform.join(root, "auth/team"))),
            Ok(())
        );
        probe.container = true;
        refused(authorize_auth_dir(&probe, Path::new(root)));
        probe.privileged = Ok(false);
        assert_eq!(authorize_auth_dir(&probe, Path::new(root)), Ok(()));
        probe.privileged = Err("no token".to_owned());
        refused(authorize_auth_dir(&probe, Path::new(root)));
    }
    let mut probe = Fake::new(Platform::Windows, true);
    probe.program_data = Err("no known folder".to_owned());
    refused(authorize_auth_dir(&probe, Path::new("D:/auth")));
    let mut probe = Fake::new(Platform::MacOs, true);
    probe.dir("/srv/auth");
    assert_eq!(authorize_auth_dir(&probe, Path::new("/srv/auth")), Ok(()));
    probe.untrust("/srv");
    assert!(authorize_auth_dir(&probe, Path::new("/srv/auth")).is_err());
}

#[test]
fn a_container_has_no_exemption_for_record_trees_or_machine_installs() {
    let mut probe = Fake::new(Platform::Linux, true);
    probe.container = true;
    probe.dir(ROOT_ACCOUNT).untrust(ROOT_ACCOUNT);
    refused(authorize(&probe, ROOT_ACCOUNT));
    probe.dir(MACHINE).untrust(MACHINE);
    refused(provision(&mut probe));
}

#[test]
fn privileged_auth_directories_require_trusted_ancestry_and_resolved_placement() {
    let mut probe = Fake::new(Platform::Linux, true);
    probe.dir("/srv/auth").untrust("/srv");
    for path in [
        "/srv/auth",
        "/srv/missing/auth",
        "/srv/./auth",
        "/srv/../auth",
    ] {
        assert!(
            authorize_auth_dir(&probe, Path::new(path)).is_err(),
            "{path}"
        );
    }
    probe.container = true;
    assert!(authorize_auth_dir(&probe, Path::new("/srv/auth")).is_ok());
    probe.container = false;
    probe.untrusted.clear();
    probe.node("/srv/auth", Node::Link);
    assert!(authorize_auth_dir(&probe, Path::new("/srv/auth/team")).is_err());
    probe.node("/srv/auth", Node::File);
    assert!(authorize_auth_dir(&probe, Path::new("/srv/auth")).is_err());
    probe.node("/srv/auth", Node::Dir);
    assert!(authorize_auth_dir(&probe, Path::new("/srv/auth")).is_ok());
    probe.dir(MACHINE);
    assert!(authorize_auth_dir(&probe, Path::new("/var/lib/open-ferry/auth")).is_ok());
    probe.privileged = Ok(false);
    probe.untrust("/srv");
    assert!(authorize_auth_dir(&probe, Path::new("/srv/auth")).is_ok());

    let mut probe = Fake::new(Platform::Windows, true);
    probe.dir("C:/PROGRA~3");
    probe
        .aliases
        .insert(probe.path("C:/PROGRA~3"), probe.path("C:/ProgramData"));
    assert!(
        authorize_auth_dir(
            &probe,
            Path::new("C:/PROGRA~3/open-ferry/versions/credentials")
        )
        .is_err()
    );
    probe
        .aliases
        .insert(probe.path("C:/PROGRA~3"), r"\\?\C:\ProgramData".to_owned());
    assert!(
        authorize_auth_dir(
            &probe,
            Path::new("C:/PROGRA~3/open-ferry/versions/credentials")
        )
        .is_err()
    );
    probe.dir("c:/programdata/OPEN-FERRY/versions");
    assert!(
        authorize_auth_dir(&probe, Path::new("c:/programdata/OPEN-FERRY/versions/auth")).is_err()
    );
    probe.program_data = Ok("C:/PROGRA~3".to_owned());
    probe.dir("C:/ProgramData/open-ferry/versions");
    assert!(
        authorize_auth_dir(&probe, Path::new("C:/ProgramData/open-ferry/versions/auth")).is_err()
    );
    probe.dir("D:/trusted/auth");
    assert!(authorize_auth_dir(&probe, Path::new("D:/trusted/auth")).is_ok());
}

#[test]
fn privileged_claude_spawns_check_every_runtime_path() {
    use open_ferry_providers::claude_cli::Entry;
    let entry = Entry {
        config_dir: "/srv/account".into(),
        ..Entry::from_auth(&open_ferry_core::auth::Auth::default())
    };
    let program = Path::new("/srv/bin/claude");
    let root = Path::new("/srv/entries");
    let dir = PathBuf::from(format!("/srv/entries/{}", entry.dir_name()));
    let work = dir.to_string_lossy().to_string() + "/work";
    let work = PathBuf::from(work);
    let mut probe = Fake::new(Platform::Linux, true);
    probe
        .file("/srv/bin/claude")
        .dir("/srv/account")
        .dir(work.to_str().unwrap());
    assert!(authorize_claude_spawn(&probe, &entry, program, root, None, None, None).is_ok());
    for path in [
        program,
        Path::new("/srv/account"),
        root,
        dir.as_path(),
        work.as_path(),
    ] {
        probe.untrust(path.to_str().unwrap());
        assert!(
            authorize_claude_spawn(&probe, &entry, program, root, None, None, None).is_err(),
            "{}",
            path.display()
        );
        probe.untrusted.clear();
    }
    probe.untrust("/srv/account");
    let missing_account = Entry {
        config_dir: "/srv/account/missing".into(),
        ..entry.clone()
    };
    assert!(
        authorize_claude_spawn(&probe, &missing_account, program, root, None, None, None).is_err()
    );
    probe.untrusted.clear();
    probe.node("/srv/account", Node::Link);
    assert!(authorize_claude_spawn(&probe, &entry, program, root, None, None, None).is_err());
    probe.node("/srv/account", Node::Dir);
    let default = Entry {
        config_dir: String::new(),
        ..entry.clone()
    };
    assert!(
        authorize_claude_spawn(&probe, &default, program, root, None, None, None)
            .unwrap_err()
            .contains("set config-dir")
    );
    probe.dir("/root");
    assert!(
        authorize_claude_spawn(
            &probe,
            &default,
            program,
            root,
            Some(Path::new("/root")),
            None,
            None
        )
        .is_ok()
    );
    probe.untrust("/root");
    assert!(
        authorize_claude_spawn(
            &probe,
            &default,
            program,
            root,
            Some(Path::new("/root")),
            None,
            None
        )
        .is_err()
    );
    probe.untrusted.clear();
    probe.dir("/srv/inherited").untrust("/srv/inherited");
    assert!(
        authorize_claude_spawn(
            &probe,
            &default,
            program,
            root,
            Some(Path::new("/root")),
            Some(Path::new("/srv/inherited")),
            None
        )
        .is_err()
    );
    let temp = std::env::temp_dir().join("open-ferry-claude-cli");
    assert!(
        authorize_claude_spawn(&probe, &entry, program, &temp, None, None, None)
            .unwrap_err()
            .contains("temporary directory")
    );
    probe.platform = Platform::MacOs;
    assert!(
        authorize_claude_spawn(&probe, &entry, program, root, None, None, None)
            .unwrap_err()
            .contains("macOS")
    );
    probe.privileged = Ok(false);
    assert!(authorize_claude_spawn(&probe, &default, program, &temp, None, None, None).is_ok());
    probe.privileged = Ok(true);
    probe.container = true;
    assert!(authorize_claude_spawn(&probe, &default, program, &temp, None, None, None).is_ok());
}

#[test]
fn unrecognised_containers_get_an_actionable_settings_remedy() {
    let probe = Fake::new(Platform::Linux, true);
    let refusal = authorize_file(&probe, Path::new("relative.yaml")).unwrap_err();
    let remedy = refusal.remedy.unwrap();
    assert!(remedy.contains("container open-ferry doesn't recognise"));
    assert!(remedy.contains("user other than root"));
    assert!(remedy.contains("root's"));
}

#[test]
fn windows_default_claude_accounts_require_a_home_and_a_trusted_profile() {
    use open_ferry_providers::claude_cli::Entry;
    let entry = Entry::from_auth(&open_ferry_core::auth::Auth::default());
    let program = Path::new("C:/trusted/bin/claude.exe");
    let root = Path::new("C:/trusted/entries");
    let home = Path::new("C:/trusted/home");
    let profile = Path::new("D:/account");
    let mut probe = Fake::new(Platform::Windows, true);
    probe
        .file("C:/trusted/bin/claude.exe")
        .dir("C:/trusted/entries")
        .dir("C:/trusted/home")
        .dir("D:/account")
        .untrust("D:/account");
    assert!(
        authorize_claude_spawn(
            &probe,
            &entry,
            program,
            root,
            Some(home),
            None,
            Some(profile)
        )
        .is_err()
    );
    probe.untrusted.clear();
    assert!(
        authorize_claude_spawn(
            &probe,
            &entry,
            program,
            root,
            Some(home),
            None,
            Some(profile)
        )
        .is_ok()
    );
    assert!(
        authorize_claude_spawn(&probe, &entry, program, root, None, None, Some(profile))
            .unwrap_err()
            .contains("set config-dir")
    );
    assert!(
        authorize_claude_spawn(&probe, &entry, program, root, Some(home), None, None)
            .unwrap_err()
            .contains("set config-dir")
    );
    probe.untrust("D:/account").dir("C:/trusted/account");
    let explicit = Entry {
        config_dir: "C:/trusted/account".into(),
        ..entry
    };
    assert!(
        authorize_claude_spawn(&probe, &explicit, program, root, None, None, Some(profile)).is_ok()
    );
}
