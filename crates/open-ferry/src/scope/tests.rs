use std::collections::{BTreeMap, BTreeSet};

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
struct Fake {
    platform: Platform,
    privileged: Result<bool, String>,
    account: Result<String, String>,
    program_data: Result<String, String>,
    nodes: BTreeMap<String, Node>,
    /// Paths the trust check refuses, with everything below them.
    untrusted: BTreeSet<String>,
    /// The receipts, by their paths: the binary each names.
    receipts: BTreeMap<String, String>,
    /// What creating a directory fails with.
    create: Option<io::ErrorKind>,
    created: Vec<String>,
}

impl Fake {
    /// The machine of a user, `privileged` or not, whose data directory is
    /// their home's.
    fn new(platform: Platform, privileged: bool) -> Fake {
        let account = match (platform, privileged) {
            (Platform::Windows, _) => WINDOWS_ACCOUNT,
            (Platform::MacOs, _) => "/Users/me/.local/share/open-ferry",
            (Platform::Linux, true) => ROOT_ACCOUNT,
            (Platform::Linux, false) => USER_ACCOUNT,
        };
        Fake {
            platform,
            privileged: Ok(privileged),
            account: Ok(account.to_owned()),
            program_data: Ok(platform.clean("C:/ProgramData")),
            nodes: BTreeMap::new(),
            untrusted: BTreeSet::new(),
            receipts: BTreeMap::new(),
            create: None,
            created: Vec::new(),
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

    fn file(&mut self, path: &str) -> &mut Fake {
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

    /// Makes the trust check refuse `path` and everything below it.
    fn untrust(&mut self, path: &str) -> &mut Fake {
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
            Some(Node::File) => State::File,
            Some(Node::Link) => State::Other,
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
        self.trust(path)
    }

    fn receipt(&self, path: &str) -> Result<Option<Receipt>, ReceiptError> {
        let path = self.path(path);
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
const MACHINE_REMEDY: &str =
    "Remove /var/lib/open-ferry as root and install again with `install.sh --system`.";

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
            "Remove {machine} as an administrator and install again with `install.ps1 -System`."
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
        machine_config(Platform::Linux, BINARY).as_deref(),
        Some("/etc/open-ferry/config.yaml")
    );
    assert_eq!(
        machine_config(Platform::Windows, &Platform::Windows.clean(WINDOWS_BINARY)),
        Some(Platform::Windows.clean("C:/Program Files/open-ferry/config.yaml"))
    );
    assert_eq!(machine_config(Platform::MacOs, BINARY), None);
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
