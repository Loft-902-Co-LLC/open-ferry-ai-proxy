use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::*;

/// A system that records what is done to it, and answers commands as the
/// test says; a command it wasn't told of succeeds and prints nothing.
#[derive(Clone, Default)]
struct Fake {
    privileged: bool,
    files: BTreeMap<String, Vec<u8>>,
    dirs: BTreeSet<String>,
    owners: BTreeMap<String, Owner>,
    links: BTreeMap<String, String>,
    resolved_paths: BTreeMap<String, String>,
    answers: BTreeMap<String, Output>,
    queued: BTreeMap<String, VecDeque<Output>>,
    reads: RefCell<Vec<String>>,
    events: Vec<String>,
    /// What `stop_supervisor` answers; `NotRunning` when not set.
    stop: Option<StopOutcome>,
    /// Paths the trust check refuses, with everything below them.
    untrusted: BTreeSet<String>,
}

impl Fake {
    fn file(mut self, path: &str, data: &str) -> Fake {
        self.files.insert(path.to_owned(), data.as_bytes().to_vec());
        self
    }

    fn dir(mut self, path: &str) -> Fake {
        self.dirs.insert(path.to_owned());
        self
    }

    fn answer(mut self, cmd: &str, code: i32, stdout: &str) -> Fake {
        if cmd == "whoami.exe /groups /fo csv /nh" {
            self.privileged = stdout.contains("S-1-16-12288") || stdout.contains("S-1-16-16384");
        }
        self.answers.insert(
            cmd.to_owned(),
            Output {
                code: Some(code),
                stdout: stdout.to_owned(),
                stderr: String::new(),
            },
        );
        self
    }

    fn fail(mut self, cmd: &str, code: i32, stderr: &str) -> Fake {
        self.answers.insert(
            cmd.to_owned(),
            Output {
                code: Some(code),
                stdout: String::new(),
                stderr: stderr.to_owned(),
            },
        );
        self
    }

    fn owner(mut self, path: &str, uid: u32, gid: u32, mode: u32, dir: bool) -> Fake {
        self.owners.insert(
            path.to_owned(),
            Owner {
                uid,
                gid,
                mode,
                dir,
            },
        );
        self
    }

    fn link(mut self, path: &str, target: &str) -> Fake {
        self.links.insert(path.to_owned(), target.to_owned());
        self
    }

    fn uid(mut self, uid: u32) -> Fake {
        self.privileged = uid == 0;
        self.answer("id -u", 0, &format!("{uid}\n"))
    }

    fn answer_for(&self, cmd: &Cmd) -> Output {
        self.answers
            .get(&cmd.to_string())
            .cloned()
            .unwrap_or(Output {
                code: Some(0),
                ..Output::default()
            })
    }

    /// What the trust check says of `path`: refused when it, or a directory
    /// above it, was marked untrusted.
    fn trust(&self, path: &str) -> Result<(), String> {
        let below = |top: &str| {
            path.strip_prefix(top)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', '\\']))
        };
        if let Some(top) = self.links.keys().find(|top| below(top)) {
            return Err(format!("{path} is not safe: {top} is a link"));
        }
        if let Some((top, _)) = self
            .owners
            .iter()
            .find(|(top, owner)| below(top) && (owner.uid != 0 || owner.mode & 0o022 != 0))
        {
            return Err(format!(
                "{path} is not safe: {top} is writable by another account or not owned by root"
            ));
        }
        match self.untrusted.iter().find(|top| below(top)) {
            Some(top) => Err(format!("{path} is not safe: {top} is untrusted")),
            None => Ok(()),
        }
    }
}

impl System for Fake {
    fn privileged(&self) -> Result<bool, String> {
        Ok(self.privileged)
    }
    fn run(&mut self, cmd: &Cmd) -> io::Result<Output> {
        self.events.push(format!("run {cmd}"));
        if let Some(answer) = self
            .queued
            .get_mut(&cmd.to_string())
            .and_then(VecDeque::pop_front)
        {
            return Ok(answer);
        }
        Ok(self.answer_for(cmd))
    }

    fn show(&mut self, cmd: &Cmd) -> io::Result<Option<i32>> {
        self.events.push(format!("show {cmd}"));
        Ok(self.answer_for(cmd).code)
    }

    fn exists(&self, path: &str) -> bool {
        self.files.contains_key(path)
            || self.dirs.contains(path)
            || path == "/"
            || path.ends_with(":\\")
    }

    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        self.reads.borrow_mut().push(path.to_owned());
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }

    fn real_path(&self, path: &str) -> io::Result<String> {
        Ok(self
            .links
            .get(path)
            .or_else(|| self.resolved_paths.get(path))
            .cloned()
            .unwrap_or_else(|| path.to_owned()))
    }

    fn owner(&self, path: &str) -> io::Result<Owner> {
        Ok(self.owners.get(path).copied().unwrap_or(Owner {
            uid: 0,
            gid: 0,
            mode: 0o755,
            dir: !self.files.contains_key(path),
        }))
    }

    fn create_dir_all(&mut self, path: &str) -> io::Result<()> {
        self.events.push(format!("mkdir {path}"));
        self.dirs.insert(path.to_owned());
        Ok(())
    }

    fn write(&mut self, path: &str, data: &[u8]) -> io::Result<()> {
        self.events.push(format!("write {path}"));
        self.files.insert(path.to_owned(), data.to_vec());
        Ok(())
    }

    fn remove(&mut self, path: &str) -> io::Result<()> {
        self.events.push(format!("remove {path}"));
        self.files
            .remove(path)
            .map(drop)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }

    fn write_atomic(&mut self, path: &str, data: &[u8]) -> io::Result<()> {
        self.events.push(format!("write_atomic {path}"));
        self.files.insert(path.to_owned(), data.to_vec());
        Ok(())
    }

    fn stop_supervisor(
        &mut self,
        data_dir: &str,
        budget: Duration,
        kind: ServiceKind,
    ) -> StopOutcome {
        // A user supervisor's stop is recorded as before; a system one says so.
        let system = if kind == ServiceKind::System {
            " system"
        } else {
            ""
        };
        self.events.push(format!(
            "stop_supervisor {data_dir} {}s{system}",
            budget.as_secs()
        ));
        self.stop.clone().unwrap_or(StopOutcome::NotRunning)
    }

    fn trusted_dir(&self, path: &str) -> Result<(), String> {
        self.trust(path)
    }

    fn trusted_file(&self, path: &str) -> Result<(), String> {
        self.trust(path)
    }

    fn machine_root(&self, platform: Platform) -> Result<Option<String>, String> {
        scope::machine_root(&crate::update::scope_probe::Fake::new(platform, true))
    }

    fn pause(&mut self, time: Duration) {
        self.events.push(format!("pause {}s", time.as_secs()));
    }
}

/// A context on `platform`, whose installed config path is the one
/// `installed::config_path` would find with `env`.
fn context(platform: Platform, exe: &str, cwd: &str, env: &[(&str, &str)]) -> Context {
    let env: BTreeMap<String, String> = env
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    let base = match platform {
        Platform::Windows => env.get("APPDATA").cloned(),
        Platform::Linux | Platform::MacOs => {
            env.get("HOME").map(|home| platform.join(home, ".config"))
        }
    };
    let installed_config = base
        .map(|base| platform.join(&platform.join(&base, "open-ferry"), "config.yaml"))
        .ok_or_else(|| "no installed config path".to_owned());
    Context {
        platform,
        exe: exe.to_owned(),
        cwd: cwd.to_owned(),
        env,
        installed_config,
        machine_config: scope::machine_config(&crate::update::scope_probe::Fake::new(
            platform, true,
        )),
    }
}

fn linux() -> Context {
    context(
        Platform::Linux,
        "/home/me/.local/bin/open-ferry",
        "/home/me/work",
        &[("HOME", "/home/me")],
    )
}

fn macos() -> Context {
    context(
        Platform::MacOs,
        "/Users/me/.local/bin/open-ferry",
        "/Users/me",
        &[("HOME", "/Users/me")],
    )
}

fn windows() -> Context {
    context(
        Platform::Windows,
        r"C:\Users\me\AppData\Local\Programs\open-ferry\open-ferry.exe",
        r"C:\Users\me",
        &[
            ("APPDATA", r"C:\Users\me\AppData\Roaming"),
            ("TEMP", r"C:\Users\me\AppData\Local\Temp"),
            ("ProgramFiles", r"C:\Program Files"),
            ("ProgramFiles(x86)", r"C:\Program Files (x86)"),
            ("SystemRoot", r"C:\Windows"),
        ],
    )
}

/// A config for the user's service.
const CONFIG: &str = "port: 8317\nauth-dir: \"~/.cli-proxy-api\"\n";

/// A config for a system service: its auth directory is a full path.
const SYSTEM_CONFIG: &str = "port: 8317\nauth-dir: /var/lib/open-ferry/auth\n";

const LINUX_CONFIG: &str = "/home/me/.config/open-ferry/config.yaml";
const LINUX_UNIT: &str = "/home/me/.config/systemd/user/open-ferry.service";
const MACOS_CONFIG: &str = "/Users/me/.config/open-ferry/config.yaml";
const MACOS_AGENT: &str =
    "/Users/me/Library/LaunchAgents/io.github.loft-902-co-llc.open-ferry.plist";
const WINDOWS_CONFIG: &str = r"C:\Users\me\AppData\Roaming\open-ferry\config.yaml";
const TASK_FILE: &str = r"C:\Users\me\AppData\Local\Temp\open-ferry-task.xml";
const SID: &str = "S-1-5-21-1004336348-1177238915-682003330-1001";

fn args(args: &[&str]) -> impl Iterator<Item = String> {
    args.iter()
        .map(|arg| (*arg).to_owned())
        .collect::<Vec<_>>()
        .into_iter()
}

/// Runs `service` with `command_line` against `fake`, and gives what it
/// printed on standard output.
fn execute_with(
    fake: &mut Fake,
    context: &Context,
    command_line: &[&str],
) -> (Result<(), String>, String) {
    let request = parse(args(command_line)).unwrap();
    let mut out = Vec::new();
    let updating = Updating::plain(data_dir(context), "the test asks for the plain form");
    let result = execute(fake, context, &request, &updating, &mut out);
    (result, String::from_utf8(out).unwrap())
}

fn events(fake: &Fake) -> Vec<&str> {
    fake.events.iter().map(String::as_str).collect()
}

fn text(fake: &Fake, path: &str) -> String {
    String::from_utf8(fake.files[path].clone()).unwrap()
}

#[test]
fn parses_an_action_and_its_flags_as_go_does() {
    // Not upstream's: `service`'s command line.
    assert_eq!(
        parse(args(&[
            "install",
            "--config",
            "c.yaml",
            "-system",
            "-dry-run=true"
        ])),
        Ok(Request {
            supervise: false,
            data_dir: None,
            action: Action::Install,
            config: Some("c.yaml".to_owned()),
            system: true,
            dry_run: true,
            dir: None,
        })
    );
    assert_eq!(
        parse(args(&["install", "-config=", "--"])),
        Ok(Request {
            supervise: false,
            data_dir: None,
            action: Action::Install,
            config: None,
            system: false,
            dry_run: false,
            dir: None,
        })
    );
    assert_eq!(
        parse(args(&["uninstall", "-system=1", "-dry-run=false"])),
        Ok(Request {
            supervise: false,
            data_dir: None,
            action: Action::Uninstall,
            config: None,
            system: true,
            dry_run: false,
            dir: None,
        })
    );
    assert_eq!(
        parse(args(&["status"])).map(|request| request.action),
        Ok(Action::Status)
    );
    assert_eq!(
        parse(args(&["run", "-system", "-config", r"C:\x\config.yaml"])),
        Ok(Request {
            supervise: false,
            data_dir: None,
            action: Action::Run,
            config: Some(r"C:\x\config.yaml".to_owned()),
            system: true,
            dry_run: false,
            dir: None,
        })
    );
}

#[test]
fn refuses_a_command_line_it_does_not_know() {
    // Not upstream's: `service`'s command line errors.
    let invalid = |command_line: &[&str]| match parse(args(command_line)) {
        Err(FlagError::Invalid(message)) => message,
        other => panic!("{command_line:?} gave {other:?}"),
    };
    assert_eq!(
        invalid(&[]),
        "service needs a command: install, uninstall or status"
    );
    assert_eq!(
        invalid(&["-config", "x", "install"]),
        "service needs a command before its flags: install, uninstall or status"
    );
    assert_eq!(invalid(&["start"]), "unknown service command: start");
    assert_eq!(
        invalid(&["status", "-dry-run"]),
        "flag provided but not defined: -dry-run"
    );
    assert_eq!(
        invalid(&["uninstall", "-config", "x"]),
        "flag provided but not defined: -config"
    );
    assert_eq!(
        invalid(&["install", "-config"]),
        "flag needs an argument: -config"
    );
    assert_eq!(
        invalid(&["install", "-system=maybe"]),
        "invalid boolean value \"maybe\" for -system: parse error"
    );
    assert_eq!(invalid(&["install", "now"]), "unexpected argument: now");
    assert_eq!(
        invalid(&["install", "--", "now"]),
        "unexpected argument: now"
    );
    assert_eq!(invalid(&["install", "---x"]), "bad flag syntax: ---x");
    for help in [
        &["-h"][..],
        &["--help"],
        &["install", "-h"],
        &["status", "-help"],
    ] {
        assert_eq!(parse(args(help)), Err(FlagError::Help), "{help:?}");
    }
}

#[test]
fn usage_shows_the_actions_and_the_installed_config_path() {
    // Not upstream's: `service -h`.
    assert_eq!(
        usage("open-ferry", &Ok(LINUX_CONFIG.to_owned())),
        "Usage: open-ferry service install [-config PATH] [-system] [-dry-run]
       open-ferry service uninstall [-system] [-dry-run]
       open-ferry service status [-system]

Runs open-ferry in the background with the system's service manager: as a
systemd unit on Linux, a launchd job on macOS, and a scheduled task at logon
on Windows, as you; with -system, as a service started at boot.

Flags:
  -config string
    \tThe config the service runs with (default: the installed config path, shown below)
  -dry-run
    \tPrint what would be written and run, and change nothing
  -system
    \tFor the whole machine, started at boot as root (LocalSystem on Windows); needs root or an administrator

The installed config path is /home/me/.config/open-ferry/config.yaml
"
    );
    assert!(
        usage("open-ferry", &Err("HOME isn't set".to_owned()))
            .ends_with("\nThere is no installed config path: HOME isn't set\n")
    );
}

#[test]
fn makes_paths_full_as_each_platform_reads_them() {
    // Not upstream's: the config's path is made absolute at install.
    let unix = |path: &str| Platform::Linux.absolute("/home/me/work", path);
    assert_eq!(unix("c.yaml"), Ok("/home/me/work/c.yaml".to_owned()));
    assert_eq!(unix("../x/./c.yaml"), Ok("/home/me/x/c.yaml".to_owned()));
    assert_eq!(unix("/etc//open-ferry/"), Ok("/etc/open-ferry".to_owned()));
    assert_eq!(unix("/../.."), Ok("/".to_owned()));

    let windows = |path: &str| Platform::Windows.absolute(r"C:\Users\me", path);
    assert_eq!(windows("c.yaml"), Ok(r"C:\Users\me\c.yaml".to_owned()));
    assert_eq!(windows("D:/cfg/../c.yaml"), Ok(r"D:\c.yaml".to_owned()));
    assert_eq!(windows(r"\cfg\c.yaml"), Ok(r"C:\cfg\c.yaml".to_owned()));
    assert_eq!(
        windows(r"\\server\share\a\..\c.yaml"),
        Ok(r"\\server\share\c.yaml".to_owned())
    );
    assert!(windows("D:c.yaml").is_err());
}

#[test]
fn finds_parents_and_what_is_within_a_directory() {
    // Not upstream's: path helpers.
    let linux = Platform::Linux;
    assert_eq!(linux.parent("/a/b"), Some("/a".to_owned()));
    assert_eq!(linux.parent("/a"), Some("/".to_owned()));
    assert_eq!(linux.parent("/"), None);
    let windows = Platform::Windows;
    assert_eq!(windows.parent(r"C:\a\b"), Some(r"C:\a".to_owned()));
    assert_eq!(windows.parent(r"C:\a"), Some(r"C:\".to_owned()));
    assert_eq!(windows.parent(r"C:\"), None);
    assert_eq!(
        windows.parent(r"\\server\share\a"),
        Some(r"\\server\share\".to_owned())
    );
    assert_eq!(windows.parent(r"\\server\share\"), None);

    assert!(windows.is_within(r"c:\program files\open-ferry\x.exe", r"C:\Program Files"));
    assert!(windows.is_within(r"C:\Program Files", r"C:\Program Files\"));
    assert!(!windows.is_within(r"C:\Program Files2\x.exe", r"C:\Program Files"));
    assert!(linux.is_within("/etc/open-ferry", "/etc"));
    assert!(!linux.is_within("/etcetera", "/etc"));
    assert!(!linux.is_within("/Etc/x", "/etc"));
}

#[test]
fn installs_with_the_installed_config_unless_told_otherwise() {
    // Not upstream's: without -config the service runs with the installed
    // config path, and with none `install` says why and does nothing.
    let mut context = linux();
    context.installed_config = Ok("/xdg/open-ferry/../open-ferry/config.yaml".to_owned());
    let mut fake = Fake::default()
        .uid(1000)
        .file("/xdg/open-ferry/config.yaml", CONFIG);
    let (result, out) = execute_with(&mut fake, &context, &["install", "-dry-run"]);
    assert_eq!(result, Ok(()));
    assert!(
        out.contains(r#"-config "/xdg/open-ferry/config.yaml""#),
        "{out}"
    );

    context.installed_config =
        Err("HOME isn't set, so there is no default config path; pass -config".to_owned());
    let mut fake = Fake::default().uid(1000);
    let (result, _) = execute_with(&mut fake, &context, &["install"]);
    assert_eq!(
        result,
        Err("HOME isn't set, so there is no default config path; pass -config".to_owned())
    );
    assert!(events(&fake).is_empty(), "{:?}", events(&fake));
}

fn user_definition() -> Definition {
    Definition {
        data_dir: None,
        exe: "/home/me/.local/bin/open-ferry".to_owned(),
        config: LINUX_CONFIG.to_owned(),
        dir: "/home/me/.config/open-ferry".to_owned(),
    }
}

#[test]
fn writes_a_systemd_user_unit() {
    // Not upstream's: the user unit's text.
    assert_eq!(
        systemd::unit(&user_definition(), false),
        "# Installed by `open-ferry service install`; `open-ferry service uninstall` removes it.
[Unit]
Description=open-ferry AI proxy
Documentation=https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy

[Service]
Type=simple
ExecStart=\"/home/me/.local/bin/open-ferry\" -config \"/home/me/.config/open-ferry/config.yaml\"
WorkingDirectory=/home/me/.config/open-ferry
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
"
    );
}

#[test]
fn writes_a_systemd_system_unit() {
    // Not upstream's: the system unit waits for the network, and starts at
    // boot.
    let definition = Definition {
        data_dir: None,
        exe: "/usr/local/bin/open-ferry".to_owned(),
        config: "/etc/open-ferry/config.yaml".to_owned(),
        dir: "/etc/open-ferry".to_owned(),
    };
    assert_eq!(
        systemd::unit(&definition, true),
        "# Installed by `open-ferry service install -system`; `open-ferry service uninstall -system` removes it.
[Unit]
Description=open-ferry AI proxy
Documentation=https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
ExecStart=\"/usr/local/bin/open-ferry\" -config \"/etc/open-ferry/config.yaml\"
WorkingDirectory=/etc/open-ferry
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
"
    );
}

#[test]
fn escapes_what_systemd_would_expand() {
    // Not upstream's: `%` is a specifier and `$` a variable to systemd.
    let definition = Definition {
        data_dir: None,
        exe: "/opt/my apps/open-ferry".to_owned(),
        config: "/srv/100% \"$HOME\"/config.yaml".to_owned(),
        dir: "/srv/100% \"$HOME\"".to_owned(),
    };
    let unit = systemd::unit(&definition, false);
    assert!(unit.contains(
        "ExecStart=\"/opt/my apps/open-ferry\" -config \"/srv/100%% \\\"$$HOME\\\"/config.yaml\"\n"
    ));
    assert!(unit.contains("WorkingDirectory=/srv/100%% \"$HOME\"\n"));
}

#[test]
fn writes_a_launchd_plist() {
    // Not upstream's: the agent's property list.
    let definition = Definition {
        data_dir: None,
        exe: "/Users/me/.local/bin/open-ferry".to_owned(),
        config: MACOS_CONFIG.to_owned(),
        dir: "/Users/me/.config/open-ferry".to_owned(),
    };
    assert_eq!(
        launchd::plist(&definition, "/Users/me/Library/Logs/open-ferry.log", false),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Installed by `open-ferry service install`; `open-ferry service uninstall` removes it. -->
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>io.github.loft-902-co-llc.open-ferry</string>
	<key>ProgramArguments</key>
	<array>
		<string>/Users/me/.local/bin/open-ferry</string>
		<string>-config</string>
		<string>/Users/me/.config/open-ferry/config.yaml</string>
	</array>
	<key>WorkingDirectory</key>
	<string>/Users/me/.config/open-ferry</string>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>StandardOutPath</key>
	<string>/Users/me/Library/Logs/open-ferry.log</string>
	<key>StandardErrorPath</key>
	<string>/Users/me/Library/Logs/open-ferry.log</string>
</dict>
</plist>
"#
    );
}

#[test]
fn escapes_paths_in_xml() {
    // Not upstream's: a path is text in the plist and the task's XML.
    let definition = Definition {
        data_dir: None,
        exe: "/Applications/A&B <x>/open-ferry".to_owned(),
        config: "/Users/me/it's/config.yaml".to_owned(),
        dir: "/Users/me/it's".to_owned(),
    };
    let plist = launchd::plist(&definition, "/Library/Logs/open-ferry.log", true);
    assert!(plist.contains("<string>/Applications/A&amp;B &lt;x&gt;/open-ferry</string>"));
    assert!(plist.contains("<string>/Users/me/it&apos;s</string>"));
    assert!(plist.contains("`open-ferry service install -system`"));
}

#[test]
fn writes_a_scheduled_task() {
    // Not upstream's: the task's definition.
    let definition = Definition {
        data_dir: None,
        exe: r"C:\Users\me\AppData\Local\Programs\open-ferry\open-ferry.exe".to_owned(),
        config: WINDOWS_CONFIG.to_owned(),
        dir: r"C:\Users\me\AppData\Roaming\open-ferry".to_owned(),
    };
    assert_eq!(
        windows::task_xml(&definition, SID),
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>open-ferry AI proxy. Installed by `open-ferry service install`; `open-ferry service uninstall` removes it.</Description>
    <URI>\open-ferry</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>S-1-5-21-1004336348-1177238915-682003330-1001</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>S-1-5-21-1004336348-1177238915-682003330-1001</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>10</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>&quot;C:\Users\me\AppData\Local\Programs\open-ferry\open-ferry.exe&quot;</Command>
      <Arguments>service run -config &quot;C:\Users\me\AppData\Roaming\open-ferry\config.yaml&quot;</Arguments>
      <WorkingDirectory>C:\Users\me\AppData\Roaming\open-ferry</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#
    );
}

#[test]
fn a_task_runs_in_the_directory_it_was_given() {
    // Not upstream's: CLIProxyAPI ran in a directory of its own, and
    // `service run` goes there as the Windows service and task do not.
    let definition = Definition {
        data_dir: None,
        exe: r"C:\Users\me\AppData\Local\Programs\open-ferry\open-ferry.exe".to_owned(),
        config: WINDOWS_CONFIG.to_owned(),
        dir: r"C:\CLIProxyAPI".to_owned(),
    };
    let xml = windows::task_xml(&definition, SID);
    assert!(
        xml.contains(r"service run -config &quot;C:\Users\me\AppData\Roaming\open-ferry\config.yaml&quot; -dir &quot;C:\CLIProxyAPI&quot;</Arguments>"),
        "{xml}"
    );
    assert!(
        xml.contains(r"<WorkingDirectory>C:\CLIProxyAPI</WorkingDirectory>"),
        "{xml}"
    );
}

#[test]
fn run_takes_a_directory() {
    // Not upstream's: `service run -dir`, and only `run` takes it.
    assert_eq!(
        parse(args(&[
            "run",
            "-config",
            r"C:\x\config.yaml",
            "-dir",
            r"C:\cpa"
        ])),
        Ok(Request {
            supervise: false,
            data_dir: None,
            action: Action::Run,
            config: Some(r"C:\x\config.yaml".to_owned()),
            system: false,
            dry_run: false,
            dir: Some(r"C:\cpa".to_owned()),
        })
    );
    assert!(matches!(
        parse(args(&["install", "-dir", r"C:\cpa"])),
        Err(FlagError::Invalid(message)) if message == "flag provided but not defined: -dir"
    ));
}

#[test]
fn writes_utf16_with_a_byte_order_mark() {
    // Not upstream's: Task Scheduler reads its XML as UTF-16.
    assert_eq!(
        encode("<é/>", true),
        [0xFF, 0xFE, b'<', 0, 0xE9, 0, b'/', 0, b'>', 0]
    );
    assert_eq!(encode("<é/>", false), "<é/>".as_bytes());
}

#[test]
fn quotes_commands_for_people_to_read_and_type() {
    // Not upstream's: how commands are shown and suggested.
    let cmd = Cmd::new("sc.exe", &["create", "open-ferry", "binPath="])
        .arg(r#""C:\Program Files\open-ferry\open-ferry.exe" service run"#)
        .arg("");
    assert_eq!(
        cmd.to_string(),
        r#"sc.exe create open-ferry binPath= "\"C:\Program Files\open-ferry\open-ferry.exe\" service run" """#
    );
    assert_eq!(
        Platform::Linux.quote("/usr/local/bin/open-ferry"),
        "/usr/local/bin/open-ferry"
    );
    assert_eq!(
        Platform::Linux.quote("/home/me/my config's"),
        r"'/home/me/my config'\''s'"
    );
    assert_eq!(
        Platform::Windows.quote(r"C:\a b\c.yaml"),
        r#""C:\a b\c.yaml""#
    );
    assert_eq!(Platform::Windows.quote(r"C:\ab\c.yaml"), r"C:\ab\c.yaml");
}

#[test]
fn installs_a_systemd_user_unit() {
    // Not upstream's: `install` on Linux.
    let mut fake = Fake::default()
        .uid(1000)
        .file(LINUX_CONFIG, CONFIG)
        .dir("/home/me/.config");
    let (result, out) = execute_with(&mut fake, &linux(), &["install"]);
    assert_eq!(result, Ok(()));
    assert_eq!(
        events(&fake),
        [
            "run id -u",
            "mkdir /home/me/.config/systemd/user",
            "write /home/me/.config/systemd/user/open-ferry.service",
            "run systemctl --user daemon-reload",
            "run systemctl --user enable --now open-ferry.service",
            "write_atomic /home/me/.local/share/open-ferry/service.json",
        ]
    );
    assert_eq!(
        text(&fake, LINUX_UNIT),
        systemd::unit(&user_definition(), false)
    );
    assert_eq!(
        out,
        "Installing the plain form, without the supervisor: the test asks for the plain form.
Created /home/me/.config/systemd/user
Wrote /home/me/.config/systemd/user/open-ferry.service
Ran: systemctl --user daemon-reload
Ran: systemctl --user enable --now open-ferry.service
Wrote /home/me/.local/share/open-ferry/service.json
open-ferry is installed and started, as a systemd user service: it starts when you log in, and again when it fails.
Its logs: journalctl --user -u open-ferry
To start it at boot and keep it running when you are logged out, run: loginctl enable-linger
`open-ferry service status` shows it; `open-ferry service uninstall` removes it.
"
    );
}

#[test]
fn makes_a_relative_config_full() {
    // Not upstream's: `-config` is made absolute from the working directory.
    let mut fake = Fake::default()
        .uid(1000)
        .file("/home/me/cfg/c.yaml", CONFIG)
        .dir("/home/me/.config/systemd/user");
    let (result, _) = execute_with(
        &mut fake,
        &linux(),
        &["install", "-config", "../cfg/c.yaml"],
    );
    assert_eq!(result, Ok(()));
    let unit = text(&fake, LINUX_UNIT);
    assert!(unit.contains("-config \"/home/me/cfg/c.yaml\"\nWorkingDirectory=/home/me/cfg\n"));
}

#[test]
fn a_dry_run_changes_nothing() {
    // Not upstream's: `-dry-run` only reads.
    let mut fake = Fake::default().uid(1000).file(LINUX_CONFIG, CONFIG);
    let (result, out) = execute_with(&mut fake, &linux(), &["install", "-dry-run"]);
    assert_eq!(result, Ok(()));
    assert_eq!(events(&fake), ["run id -u"]);
    let unit = systemd::unit(&user_definition(), false)
        .lines()
        .map(|line| format!("    {line}\n"))
        .collect::<String>();
    assert_eq!(
        out,
        format!(
            "Installing the plain form, without the supervisor: the test asks for the plain form.
Would create /home/me/.config/systemd/user
Would write /home/me/.config/systemd/user/open-ferry.service:
{unit}Would run: systemctl --user daemon-reload
Would run: systemctl --user enable --now open-ferry.service
Would write /home/me/.local/share/open-ferry/service.json:
    {{
      \"format\": 1,
      \"kind\": \"user\",
      \"manager\": \"systemd\",
      \"supervised\": false,
      \"data_dir\": \"/home/me/.local/share/open-ferry\",
      \"exe\": \"/home/me/.local/bin/open-ferry\",
      \"config\": \"/home/me/.config/open-ferry/config.yaml\"
    }}
Dry run: nothing was changed.
"
        )
    );

    let mut fake = Fake::default().uid(1000).file(LINUX_UNIT, "unit");
    let (result, out) = execute_with(&mut fake, &linux(), &["uninstall", "-dry-run"]);
    assert_eq!(result, Ok(()));
    assert_eq!(events(&fake), ["run id -u"]);
    assert_eq!(
        out,
        "The service's data directory is /home/me/.local/share/open-ferry (worked out from the environment, as nothing else names it).
Would run: systemctl --user disable --now open-ferry.service
Would remove /home/me/.config/systemd/user/open-ferry.service
Would run: systemctl --user daemon-reload
Dry run: nothing was changed.
"
    );
}

#[test]
fn refuses_a_config_that_is_missing_or_does_not_load() {
    // Not upstream's: `install` needs a config that loads.
    let mut fake = Fake::default().uid(1000);
    let (result, _) = execute_with(&mut fake, &linux(), &["install"]);
    assert_eq!(
        result,
        Err(format!(
            "There is no config at {LINUX_CONFIG}. Make one with `open-ferry init`, or name another with -config."
        ))
    );
    let mut fake = Fake::default()
        .uid(1000)
        .file(LINUX_CONFIG, "port: [8317\n");
    let (result, _) = execute_with(&mut fake, &linux(), &["install", "-dry-run"]);
    let message = result.unwrap_err();
    assert!(
        message.starts_with(&format!("The config at {LINUX_CONFIG} doesn't load: ")),
        "{message}"
    );
    assert_eq!(events(&fake), ["run id -u"]);
}

#[test]
fn refuses_to_install_twice() {
    // Not upstream's: an installed service is uninstalled first.
    let mut fake = Fake::default()
        .uid(1000)
        .file(LINUX_CONFIG, CONFIG)
        .file(LINUX_UNIT, "unit");
    let (result, _) = execute_with(&mut fake, &linux(), &["install"]);
    assert_eq!(
        result,
        Err(format!(
            "open-ferry is already installed as a systemd user service ({LINUX_UNIT}). To install it again, run `open-ferry service uninstall` first."
        ))
    );
    assert_eq!(events(&fake), ["run id -u"]);
}

#[test]
fn refuses_root_for_a_user_service() {
    let mut fake = Fake::default().uid(0).file(LINUX_CONFIG, CONFIG);
    let (result, _) = execute_with(&mut fake, &linux(), &["install"]);
    assert!(result.unwrap_err().contains("You are root"));
    assert!(fake.events.is_empty());
    assert!(fake.reads.borrow().is_empty());
}

#[test]
fn a_system_service_needs_root() {
    // Not upstream's: `-system` without root says how to run it.
    let mut fake = Fake::default()
        .uid(1000)
        .file("/etc/open-ferry/config.yaml", SYSTEM_CONFIG);
    let (result, _) = execute_with(
        &mut fake,
        &linux(),
        &[
            "install",
            "-system",
            "-config",
            "/etc/open-ferry/config.yaml",
        ],
    );
    assert_eq!(
        result,
        Err("Installing a system service needs root. Run:\n  sudo /home/me/.local/bin/open-ferry service install -system -config /etc/open-ferry/config.yaml".to_owned())
    );
    let mut fake = Fake::default()
        .uid(1000)
        .file("/etc/systemd/system/open-ferry.service", "unit");
    let (result, _) = execute_with(&mut fake, &linux(), &["uninstall", "-system"]);
    assert_eq!(
        result,
        Err("Removing a system service needs root. Run:\n  sudo /home/me/.local/bin/open-ferry service uninstall -system".to_owned())
    );
    assert_eq!(events(&fake), ["run id -u"]);
}

#[test]
fn a_system_service_refuses_an_auth_dir_under_home() {
    // Not upstream's: `~` would be root's home.
    let mut fake = Fake::default()
        .uid(0)
        .file("/etc/open-ferry/config.yaml", "port: 8317\n");
    let (result, _) = execute_with(
        &mut fake,
        &linux(),
        &[
            "install",
            "-system",
            "-config",
            "/etc/open-ferry/config.yaml",
        ],
    );
    assert_eq!(
        result,
        Err("The config's auth-dir is not set, so ~/.cli-proxy-api. A system service runs as root, and ~ would be root's home, not yours. Set auth-dir to a full path, or install without -system.".to_owned())
    );
}

fn system_install(fake: &mut Fake, exe: &str) -> Result<(), String> {
    let context = context(Platform::Linux, exe, "/root", &[("HOME", "/root")]);
    execute_with(
        fake,
        &context,
        &[
            "install",
            "-system",
            "-config",
            "/etc/open-ferry/config.yaml",
        ],
    )
    .0
}

#[test]
fn a_system_service_refuses_files_others_can_change() {
    // Not upstream's: root would run what someone else can change.
    let config = "/etc/open-ferry/config.yaml";
    let base = || Fake::default().uid(0).file(config, SYSTEM_CONFIG);
    let refused = |problem: &str| {
        Err(format!(
            "A system service runs as root, so open-ferry's binary and its config, and the directories above them, must be owned by root and writable by no one else; {problem}. Copy them where only root can change them, such as /usr/local/bin/open-ferry and /etc/open-ferry/config.yaml, and pass -config."
        ))
    };

    let mut fake = base().owner("/home/me/.local/bin/open-ferry", 1000, 1000, 0o755, false);
    assert_eq!(
        system_install(&mut fake, "/home/me/.local/bin/open-ferry"),
        refused("/home/me/.local/bin/open-ferry is owned by user 1000")
    );
    let mut fake = base().owner("/usr/local/bin", 0, 0, 0o777, true);
    assert_eq!(
        system_install(&mut fake, "/usr/local/bin/open-ferry"),
        refused("everyone may write to /usr/local/bin")
    );
    let mut fake = base().owner("/etc/open-ferry", 0, 50, 0o775, true);
    assert_eq!(
        system_install(&mut fake, "/usr/local/bin/open-ferry"),
        refused("group 50 may write to /etc/open-ferry")
    );
    // A link is followed: what it points to is what runs.
    let mut fake = base()
        .link("/usr/local/bin/open-ferry", "/home/me/bin/open-ferry")
        .owner("/home/me", 1000, 1000, 0o755, true);
    assert_eq!(
        system_install(&mut fake, "/usr/local/bin/open-ferry"),
        refused("/home/me is owned by user 1000")
    );
    // The real trust check also refuses root-group writes and sticky dirs.
    let mut fake = base()
        .owner("/etc/open-ferry", 0, 0, 0o775, true)
        .owner("/srv", 0, 0, 0o1777, true)
        .dir("/etc/systemd/system");
    assert!(
        system_install(&mut fake, "/srv/open-ferry")
            .unwrap_err()
            .contains("/srv is writable by another account")
    );
}

#[test]
fn installs_a_systemd_system_unit() {
    // Not upstream's: the plain system definition stays the same.
    let mut fake = Fake::default()
        .uid(0)
        .file("/etc/open-ferry/config.yaml", SYSTEM_CONFIG)
        .dir("/etc/systemd/system");
    assert_eq!(
        system_install(&mut fake, "/usr/local/bin/open-ferry"),
        Ok(())
    );
    assert_eq!(
        events(&fake),
        [
            "run id -u",
            "write /etc/systemd/system/open-ferry.service",
            "run systemctl daemon-reload",
            "run systemctl enable --now open-ferry.service",
        ]
    );
    let definition = Definition {
        data_dir: None,
        exe: "/usr/local/bin/open-ferry".to_owned(),
        config: "/etc/open-ferry/config.yaml".to_owned(),
        dir: "/etc/open-ferry".to_owned(),
    };
    assert_eq!(
        text(&fake, "/etc/systemd/system/open-ferry.service"),
        systemd::unit(&definition, true)
    );
}

#[test]
fn says_what_failed_and_how_to_undo_it() {
    // Not upstream's: a failed step stops the install.
    let mut fake = Fake::default()
        .uid(1000)
        .file(LINUX_CONFIG, CONFIG)
        .dir("/home/me/.config/systemd/user")
        .fail(
            "systemctl --user enable --now open-ferry.service",
            1,
            "Failed to connect to bus: No medium found\n",
        );
    let (result, out) = execute_with(&mut fake, &linux(), &["install"]);
    assert_eq!(
        result,
        Err("`systemctl --user enable --now open-ferry.service` failed with exit code 1: Failed to connect to bus: No medium found
What was done so far is still in place: `open-ferry service uninstall` removes it.".to_owned())
    );
    assert!(
        out.ends_with("Ran: systemctl --user daemon-reload\n"),
        "{out}"
    );
}

#[test]
fn uninstalls_a_systemd_unit() {
    // Not upstream's: `uninstall` on Linux, going on when the unit isn't
    // running.
    let mut fake = Fake::default()
        .uid(1000)
        .file(LINUX_UNIT, "unit")
        .file(LINUX_CONFIG, CONFIG)
        .fail(
            "systemctl --user disable --now open-ferry.service",
            1,
            "Failed to disable unit",
        )
        // The definition can't be read, so a supervisor may run: the manager
        // is asked, and says the unit isn't running.
        .answer(
            "systemctl --user is-active open-ferry.service",
            3,
            "inactive\n",
        );
    let (result, out) = execute_with(&mut fake, &linux(), &["uninstall"]);
    assert_eq!(result, Ok(()));
    assert_eq!(
        events(&fake),
        [
            "run id -u",
            "run systemctl --user disable --now open-ferry.service",
            "run systemctl --user is-active open-ferry.service",
            "remove /home/me/.config/systemd/user/open-ferry.service",
            "run systemctl --user daemon-reload",
        ]
    );
    assert!(fake.files.contains_key(LINUX_CONFIG));
    assert_eq!(
        out,
        "The service's data directory is /home/me/.local/share/open-ferry (worked out from the environment, as nothing else names it).
Ran: systemctl --user disable --now open-ferry.service (failed with exit code 1: Failed to disable unit; the service is not running, going on)
Removed /home/me/.config/systemd/user/open-ferry.service
Ran: systemctl --user daemon-reload
open-ferry is no longer installed as a systemd user service. The config and the auth directory are left as they are.
"
    );
}

#[test]
fn says_when_nothing_is_installed() {
    // Not upstream's: `uninstall` and `status` of a service that isn't there.
    let mut fake = Fake::default().uid(1000);
    let (result, _) = execute_with(&mut fake, &linux(), &["uninstall"]);
    assert_eq!(
        result,
        Err(format!(
            "open-ferry isn't installed as a systemd user service ({LINUX_UNIT} doesn't exist). For the whole machine's service, add -system."
        ))
    );
    let (result, _) = execute_with(&mut fake, &linux(), &["status", "-system"]);
    assert_eq!(
        result,
        Err("open-ferry isn't installed as a systemd system service (/etc/systemd/system/open-ferry.service doesn't exist). For your own service, leave out -system.".to_owned())
    );
    assert!(fake.events.is_empty(), "{:?}", fake.events);
}

#[test]
fn shows_the_status_whatever_the_manager_says() {
    // Not upstream's: `status` shows the service manager's own output.
    let mut fake = Fake::default().file(LINUX_UNIT, "unit").fail(
        "systemctl --user --no-pager status open-ferry.service",
        3,
        "",
    );
    let (result, out) = execute_with(&mut fake, &linux(), &["status"]);
    assert_eq!(result, Ok(()));
    assert_eq!(
        events(&fake),
        ["show systemctl --user --no-pager status open-ferry.service"]
    );
    assert_eq!(
        out,
        format!("open-ferry is installed as a systemd user service: {LINUX_UNIT}\n")
    );
}

#[test]
fn installs_and_uninstalls_a_launchd_agent() {
    // Not upstream's: `install` and `uninstall` on macOS.
    let mut fake = Fake::default()
        .uid(501)
        .file(MACOS_CONFIG, CONFIG)
        .dir("/Users/me/Library/Logs");
    let (result, out) = execute_with(&mut fake, &macos(), &["install"]);
    assert_eq!(result, Ok(()));
    assert_eq!(
        events(&fake),
        [
            "run id -u",
            "mkdir /Users/me/Library/LaunchAgents",
            &format!("write {MACOS_AGENT}"),
            "run launchctl enable gui/501/io.github.loft-902-co-llc.open-ferry",
            &format!("run launchctl bootstrap gui/501 {MACOS_AGENT}"),
            "write_atomic /Users/me/.local/share/open-ferry/service.json",
        ]
    );
    let definition = Definition {
        data_dir: None,
        exe: "/Users/me/.local/bin/open-ferry".to_owned(),
        config: MACOS_CONFIG.to_owned(),
        dir: "/Users/me/.config/open-ferry".to_owned(),
    };
    assert_eq!(
        text(&fake, MACOS_AGENT),
        launchd::plist(&definition, "/Users/me/Library/Logs/open-ferry.log", false)
    );
    assert!(out.contains(
        "open-ferry is installed and started, as a launchd agent: it starts when you log in, and again when it fails.\nIts output: /Users/me/Library/Logs/open-ferry.log\n"
    ));

    fake.events.clear();
    let (result, _) = execute_with(&mut fake, &macos(), &["uninstall"]);
    assert_eq!(result, Ok(()));
    assert_eq!(
        events(&fake),
        [
            "run id -u",
            "run launchctl bootout gui/501/io.github.loft-902-co-llc.open-ferry",
            &format!("remove {MACOS_AGENT}"),
            "remove /Users/me/.local/share/open-ferry/service.json",
        ]
    );
}

#[test]
fn refuses_a_launchd_daemon_before_reading_or_running_anything() {
    let context = context(
        Platform::MacOs,
        "/usr/local/bin/open-ferry",
        "/",
        &[("HOME", "/var/root")],
    );
    for dry_run in [false, true] {
        let mut fake = Fake::default().uid(0);
        let mut args = vec!["install", "-system"];
        if dry_run {
            args.push("-dry-run");
        }
        let error = execute_with(&mut fake, &context, &args).0.unwrap_err();
        assert!(error.contains("macOS"));
        assert!(error.contains("without -system"));
        assert!(fake.events.is_empty());
        assert!(fake.reads.borrow().is_empty());
    }
    let plist = launchd::plist_path(&context, true).unwrap();
    for action in ["status", "uninstall"] {
        let mut fake = Fake::default().uid(0).file(&plist, "old daemon");
        assert!(
            execute_with(&mut fake, &context, &[action, "-system"])
                .0
                .is_ok()
        );
        assert!(fake.reads.borrow().iter().all(|path| path == &plist));
    }
}

fn windows_user() -> Fake {
    Fake::default()
        .file(WINDOWS_CONFIG, CONFIG)
        .fail(
            "schtasks.exe /query /tn open-ferry",
            1,
            "ERROR: The system cannot find the file specified.",
        )
        .answer(
            "whoami.exe /user /fo csv /nh",
            0,
            &format!("\"desktop-1\\me\",\"{SID}\"\r\n"),
        )
}

#[test]
fn installs_a_scheduled_task() {
    // Not upstream's: `install` on Windows registers a task for the user.
    let mut fake = windows_user();
    let (result, out) = execute_with(&mut fake, &windows(), &["install"]);
    assert_eq!(result, Ok(()));
    // A failed query by name reads the same for a task that isn't there and
    // for one the scheduler wouldn't say: the list tells them apart.
    assert_eq!(
        events(&fake),
        [
            "run schtasks.exe /query /tn open-ferry",
            "run schtasks.exe /query /fo csv /nh",
            "run whoami.exe /user /fo csv /nh",
            &format!("write {TASK_FILE}"),
            &format!("run schtasks.exe /create /tn open-ferry /xml {TASK_FILE}"),
            "run schtasks.exe /run /tn open-ferry",
            &format!("remove {TASK_FILE}"),
        ]
    );
    assert!(out.contains(
        r"Its output: C:\Users\me\AppData\Roaming\open-ferry\service.log
"
    ));
}

#[test]
fn removes_the_task_file_when_registering_fails() {
    // Not upstream's: the task's XML is a temporary file.
    let mut fake = windows_user().fail(
        &format!("schtasks.exe /create /tn open-ferry /xml {TASK_FILE}"),
        1,
        "ERROR: Access is denied.\r\n",
    );
    let (result, out) = execute_with(&mut fake, &windows(), &["install"]);
    assert_eq!(
        result,
        Err(format!(
            "`schtasks.exe /create /tn open-ferry /xml {TASK_FILE}` failed with exit code 1: ERROR: Access is denied."
        ))
    );
    assert!(!fake.files.contains_key(TASK_FILE));
    assert_eq!(
        out,
        format!(
            "Installing the plain form, without the supervisor: the test asks for the plain form.\nWrote {TASK_FILE}\nRemoved {TASK_FILE}\n"
        )
    );
}

#[test]
fn plans_the_task_in_utf16_for_the_user_and_removes_its_file() {
    // Not upstream's: `schtasks /create /xml` reads UTF-16, and the file is
    // only needed until the task is registered.
    let mut fake = windows_user();
    let context = windows();
    let definition = Definition {
        data_dir: None,
        exe: context.exe.clone(),
        config: WINDOWS_CONFIG.to_owned(),
        dir: r"C:\Users\me\AppData\Roaming\open-ferry".to_owned(),
    };
    let plan = install_plan(
        &mut fake,
        &context,
        Target::ScheduledTask,
        &definition,
        &Account::Windows { elevated: false },
        None,
    )
    .unwrap();
    assert_eq!(
        plan,
        Plan {
            steps: vec![
                Step::Write {
                    path: TASK_FILE.to_owned(),
                    text: windows::task_xml(&definition, SID),
                    utf16: true,
                },
                Step::Run(Cmd::new(
                    "schtasks.exe",
                    &["/create", "/tn", "open-ferry", "/xml", TASK_FILE]
                )),
                Step::Run(Cmd::new("schtasks.exe", &["/run", "/tn", "open-ferry"])),
            ],
            cleanup: vec![TASK_FILE.to_owned()],
        }
    );
    assert_eq!(events(&fake), ["run whoami.exe /user /fo csv /nh"]);
}

#[test]
fn uninstalls_a_scheduled_task() {
    // Not upstream's: `uninstall` on Windows ends the task, then deletes it.
    let mut fake = Fake::default().fail("schtasks.exe /end /tn open-ferry", 1, "not running");
    let (result, out) = execute_with(&mut fake, &windows(), &["uninstall"]);
    assert_eq!(result, Ok(()));
    assert_eq!(
        events(&fake),
        [
            "run schtasks.exe /query /tn open-ferry",
            "run schtasks.exe /query /tn open-ferry /xml",
            "run schtasks.exe /end /tn open-ferry",
            "run schtasks.exe /delete /tn open-ferry /f",
        ]
    );
    assert!(out.ends_with(
        "open-ferry is no longer installed as a scheduled task. The config and the auth directory are left as they are.\n"
    ));

    let mut fake = Fake::default();
    let (result, _) = execute_with(&mut fake, &windows(), &["status"]);
    assert_eq!(result, Ok(()));
    assert_eq!(
        events(&fake),
        [
            "run schtasks.exe /query /tn open-ferry",
            "show schtasks.exe /query /tn open-ferry /fo list /v",
        ]
    );
}

const PROGRAM_FILES_EXE: &str = r"C:\Program Files\open-ferry\open-ferry.exe";
const PROGRAM_FILES_CONFIG: &str = r"C:\Program Files\open-ferry\config.yaml";

fn windows_admin(exe: &str) -> (Fake, Context) {
    let fake = Fake::default()
        .file(PROGRAM_FILES_CONFIG, "auth-dir: 'C:\\ProgramData\\open-ferry\\auth'\n")
        .answer(
            "whoami.exe /groups /fo csv /nh",
            0,
            "\"Everyone\",\"Well-known group\",\"S-1-1-0\",\"Mandatory group\"\r\n\"Mandatory Label\\High Mandatory Level\",\"Label\",\"S-1-16-12288\",\"\"\r\n",
        )
        .fail("sc.exe query open-ferry", 1060, "[SC] EnumQueryServicesStatus:OpenService FAILED 1060");
    let mut context = windows();
    context.exe = exe.to_owned();
    (fake, context)
}

#[test]
fn installs_a_windows_service() {
    // Not upstream's: `install -system` on Windows creates a service.
    let (mut fake, context) = windows_admin(PROGRAM_FILES_EXE);
    let (result, out) = execute_with(
        &mut fake,
        &context,
        &["install", "-system", "-config", PROGRAM_FILES_CONFIG],
    );
    assert_eq!(result, Ok(()));
    assert_eq!(
        events(&fake),
        [
            "run sc.exe query open-ferry",
            r#"run sc.exe create open-ferry binPath= "\"C:\Program Files\open-ferry\open-ferry.exe\" service run -system -config \"C:\Program Files\open-ferry\config.yaml\"" start= auto DisplayName= open-ferry"#,
            r#"run sc.exe description open-ferry "open-ferry AI proxy""#,
            "run sc.exe failure open-ferry reset= 86400 actions= restart/5000/restart/60000/restart/60000",
            "run sc.exe failureflag open-ferry 1",
            "run sc.exe start open-ferry",
        ]
    );
    assert!(out.contains(
        r"open-ferry is installed and started, as a Windows service: it starts at boot as LocalSystem, and again when it fails.
Its output: C:\Program Files\open-ferry\service.log; the service manager logs its starts and stops in the System event log.
`open-ferry service status -system` shows it; `open-ferry service uninstall -system` removes it.
"
    ));
}

#[test]
fn a_windows_service_needs_an_administrator_and_protected_files() {
    // Not upstream's: LocalSystem would run what the user can change.
    let (fake, context) = windows_admin(PROGRAM_FILES_EXE);
    let mut fake = fake.answer("whoami.exe /groups /fo csv /nh", 0, "\"Everyone\",\"Well-known group\",\"S-1-1-0\",\"\"\r\n\"Mandatory Label\\Medium Mandatory Level\",\"Label\",\"S-1-16-8192\",\"\"\r\n");
    let (result, _) = execute_with(
        &mut fake,
        &context,
        &["install", "-system", "-config", PROGRAM_FILES_CONFIG],
    );
    assert_eq!(
        result,
        Err(format!(
            "Installing a Windows service needs an administrator. Run this in a terminal opened with \"Run as administrator\":\n  \"{PROGRAM_FILES_EXE}\" service install -system -config \"{PROGRAM_FILES_CONFIG}\""
        ))
    );

    let (mut fake, context) = windows_admin(&windows().exe);
    let (result, _) = execute_with(
        &mut fake,
        &context,
        &["install", "-system", "-config", PROGRAM_FILES_CONFIG],
    );
    assert_eq!(
        result,
        Err(format!(
            r"A Windows service runs as LocalSystem, so open-ferry's binary and its config must be where only administrators can change them, such as C:\Program Files; {} isn't. Copy them to C:\Program Files\open-ferry, and pass -config.",
            windows().exe
        ))
    );

    let (fake, context) = windows_admin(PROGRAM_FILES_EXE);
    let mut fake = fake.file(r"C:\ProgramData\open-ferry\config.yaml", SYSTEM_CONFIG);
    fake.untrusted
        .insert(r"C:\ProgramData\open-ferry\config.yaml".to_owned());
    fake.events.clear();
    let (result, _) = execute_with(
        &mut fake,
        &context,
        &[
            "install",
            "-system",
            "-config",
            r"C:\ProgramData\open-ferry\config.yaml",
        ],
    );
    assert!(
        result
            .unwrap_err()
            .contains(r"C:\ProgramData\open-ferry\config.yaml failed the system trust check"),
    );
}

#[test]
fn a_windows_service_dry_run_says_it_needs_an_administrator() {
    // Not upstream's: a dry run shows the plan without an administrator.
    let (fake, context) = windows_admin(PROGRAM_FILES_EXE);
    let mut fake = fake.answer("whoami.exe /groups /fo csv /nh", 0, "");
    let (result, out) = execute_with(
        &mut fake,
        &context,
        &[
            "install",
            "-system",
            "-dry-run",
            "-config",
            PROGRAM_FILES_CONFIG,
        ],
    );
    assert_eq!(result, Ok(()));
    assert_eq!(events(&fake), ["run sc.exe query open-ferry"]);
    assert!(out.ends_with(
        "Would run: sc.exe start open-ferry\nDry run: nothing was changed.\nWithout -dry-run, this needs an administrator.\n"
    ));
}

#[test]
fn uninstalls_a_windows_service() {
    // Not upstream's: `uninstall -system` on Windows stops the service, then
    // deletes it.
    let (fake, context) = windows_admin(PROGRAM_FILES_EXE);
    let mut fake = fake
        .answer("sc.exe query open-ferry", 0, "STATE : 4 RUNNING")
        .answer(
            "sc.exe qc open-ferry",
            0,
            &format!(
                "BINARY_PATH_NAME : {}",
                windows::service_command(&Definition {
                    exe: PROGRAM_FILES_EXE.to_owned(),
                    config: PROGRAM_FILES_CONFIG.to_owned(),
                    dir: r"C:\Program Files\open-ferry".to_owned(),
                    data_dir: None
                })
            ),
        );
    let (result, _) = execute_with(&mut fake, &context, &["uninstall", "-system"]);
    assert_eq!(result, Ok(()));
    assert_eq!(
        events(&fake),
        [
            "run sc.exe query open-ferry",
            "run sc.exe qc open-ferry",
            "run sc.exe stop open-ferry",
            "run sc.exe delete open-ferry",
        ]
    );

    // A query that fails for another reason isn't taken for "not installed".
    let (fake, context) = windows_admin(PROGRAM_FILES_EXE);
    let mut fake = fake.fail(
        "sc.exe query open-ferry",
        5,
        "[SC] OpenService FAILED 5:\r\n\r\nAccess is denied.",
    );
    let (result, _) = execute_with(&mut fake, &context, &["status", "-system"]);
    assert_eq!(
        result,
        Err("`sc.exe query open-ferry` failed with exit code 5: [SC] OpenService FAILED 5:\r\n\r\nAccess is denied.".to_owned())
    );
}

#[test]
fn refuses_paths_a_definition_cannot_hold() {
    // Not upstream's: a path that would change what the definition says.
    let mut fake = Fake::default().uid(1000);
    let (result, _) = execute_with(
        &mut fake,
        &linux(),
        &["install", "-config", "/tmp/a\nb.yaml"],
    );
    assert_eq!(
        result,
        Err("The config's path, \"/tmp/a\\nb.yaml\", has a control character, which a service definition can't hold.".to_owned())
    );
    let (result, _) = execute_with(
        &mut fake,
        &linux(),
        &["install", "-config", r"/tmp/a\b.yaml"],
    );
    assert_eq!(
        result,
        Err(r"The config's path, /tmp/a\b.yaml, has a backslash, which a systemd unit would read as an escape.".to_owned())
    );
    let (result, _) = execute_with(
        &mut fake,
        &windows(),
        &["install", "-config", r"C:\%USERNAME%\c.yaml"],
    );
    assert_eq!(
        result,
        Err(r"The config's path, C:\%USERNAME%\c.yaml, has a %, which Windows would read as part of a variable.".to_owned())
    );
    assert!(fake.events.is_empty(), "{:?}", fake.events);
}

#[test]
fn quotes_windows_arguments_as_the_c_runtime_reads_them() {
    // Not upstream's: the task's and the service's command lines.
    let definition = Definition {
        data_dir: None,
        exe: r"C:\Program Files\open-ferry\open-ferry.exe".to_owned(),
        config: r"D:\weird dir\".to_owned(),
        dir: r"D:\weird dir".to_owned(),
    };
    assert_eq!(
        windows::service_command(&definition),
        r#""C:\Program Files\open-ferry\open-ferry.exe" service run -system -config "D:\weird dir\\" -dir "D:\weird dir""#
    );
}

// Not upstream's: Windows gives a real path in its verbatim form; it is
// shown and compared in the usual one.
#[test]
fn real_paths_lose_the_verbatim_prefix() {
    assert_eq!(
        plain_windows_path(r"\\?\C:\Users\me\cpa".to_owned()),
        r"C:\Users\me\cpa"
    );
    assert_eq!(
        plain_windows_path(r"\\?\UNC\server\share\cpa".to_owned()),
        r"\\server\share\cpa"
    );
    assert_eq!(
        plain_windows_path("/home/me/cpa".to_owned()),
        "/home/me/cpa"
    );
    assert_eq!(
        plain_windows_path(r"\\?\Volume{1}\cpa".to_owned()),
        r"\\?\Volume{1}\cpa"
    );
}

// The supervised form, which runs the blue-green supervisor.

const LINUX_DATA: &str = "/home/me/.local/share/open-ferry";
const MACOS_DATA: &str = "/Users/me/.local/share/open-ferry";
const WINDOWS_DATA: &str = r"C:\Users\me\AppData\Local\open-ferry";

fn supervised_definition() -> Definition {
    Definition {
        data_dir: Some(LINUX_DATA.to_owned()),
        ..user_definition()
    }
}

fn windows_supervised_definition() -> Definition {
    Definition {
        data_dir: Some(WINDOWS_DATA.to_owned()),
        exe: r"C:\Users\me\AppData\Local\Programs\open-ferry\open-ferry.exe".to_owned(),
        config: WINDOWS_CONFIG.to_owned(),
        dir: r"C:\Users\me\AppData\Roaming\open-ferry".to_owned(),
    }
}

/// A Windows context that has a `LOCALAPPDATA`, so a data directory.
fn windows_with_data() -> Context {
    let mut context = windows();
    context.env.insert(
        "LOCALAPPDATA".to_owned(),
        r"C:\Users\me\AppData\Local".to_owned(),
    );
    context
}

/// Runs `service` with `command_line` against `fake` as `updating` decides.
fn execute_updating(
    fake: &mut Fake,
    context: &Context,
    command_line: &[&str],
    updating: &Updating,
) -> (Result<(), String>, String) {
    let request = parse(args(command_line)).unwrap();
    let mut out = Vec::new();
    let result = execute(fake, context, &request, updating, &mut out);
    (result, String::from_utf8(out).unwrap())
}

#[test]
fn writes_a_supervised_systemd_user_unit() {
    // Not upstream's: the supervised unit differs from the plain one in its
    // command line and in how stopping it ends the processes.
    assert_eq!(
        systemd::unit(&supervised_definition(), false),
        "# Installed by `open-ferry service install`; `open-ferry service uninstall` removes it.
[Unit]
Description=open-ferry AI proxy
Documentation=https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy

[Service]
Type=simple
ExecStart=\"/home/me/.local/bin/open-ferry\" -supervise -config \"/home/me/.config/open-ferry/config.yaml\" -data-dir \"/home/me/.local/share/open-ferry\"
WorkingDirectory=/home/me/.config/open-ferry
Restart=on-failure
RestartSec=5
KillMode=mixed
TimeoutStopSec=60

[Install]
WantedBy=default.target
"
    );
}

#[test]
fn writes_a_supervised_launchd_agent() {
    // Not upstream's: the supervised agent runs the supervisor and gives it
    // 60 s to stop.
    let definition = Definition {
        data_dir: Some(MACOS_DATA.to_owned()),
        exe: "/Users/me/.local/bin/open-ferry".to_owned(),
        config: MACOS_CONFIG.to_owned(),
        dir: "/Users/me/.config/open-ferry".to_owned(),
    };
    let log = "/Users/me/Library/Logs/open-ferry.log";
    let expected = [
        r#"<?xml version="1.0" encoding="UTF-8"?>"#,
        r#"<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">"#,
        "<!-- Installed by `open-ferry service install`; `open-ferry service uninstall` removes it. -->",
        r#"<plist version="1.0">"#,
        "<dict>",
        "\t<key>Label</key>",
        "\t<string>io.github.loft-902-co-llc.open-ferry</string>",
        "\t<key>ProgramArguments</key>",
        "\t<array>",
        "\t\t<string>/Users/me/.local/bin/open-ferry</string>",
        "\t\t<string>-supervise</string>",
        "\t\t<string>-config</string>",
        "\t\t<string>/Users/me/.config/open-ferry/config.yaml</string>",
        "\t\t<string>-data-dir</string>",
        "\t\t<string>/Users/me/.local/share/open-ferry</string>",
        "\t</array>",
        "\t<key>WorkingDirectory</key>",
        "\t<string>/Users/me/.config/open-ferry</string>",
        "\t<key>RunAtLoad</key>",
        "\t<true/>",
        "\t<key>KeepAlive</key>",
        "\t<dict>",
        "\t\t<key>SuccessfulExit</key>",
        "\t\t<false/>",
        "\t</dict>",
        "\t<key>ExitTimeOut</key>",
        "\t<integer>60</integer>",
        "\t<key>StandardOutPath</key>",
        "\t<string>/Users/me/Library/Logs/open-ferry.log</string>",
        "\t<key>StandardErrorPath</key>",
        "\t<string>/Users/me/Library/Logs/open-ferry.log</string>",
        "</dict>",
        "</plist>",
        "",
    ]
    .join("\n");
    assert_eq!(launchd::plist(&definition, log, false), expected);
    // The plain agent has no ExitTimeOut and no supervisor flags.
    let plain = Definition {
        data_dir: None,
        ..definition
    };
    let plain = launchd::plist(&plain, log, false);
    assert!(!plain.contains("ExitTimeOut"), "{plain}");
    assert!(!plain.contains("-supervise"), "{plain}");
}

#[test]
fn writes_a_supervised_scheduled_task() {
    // Not upstream's: the supervised task runs `service run -supervise`, and
    // is otherwise the plain task.
    let plain = windows::task_xml(
        &Definition {
            data_dir: None,
            ..windows_supervised_definition()
        },
        SID,
    );
    let supervised = windows::task_xml(&windows_supervised_definition(), SID);
    let plain_line = r"      <Arguments>service run -config &quot;C:\Users\me\AppData\Roaming\open-ferry\config.yaml&quot;</Arguments>";
    let supervised_line = r"      <Arguments>service run -supervise -config &quot;C:\Users\me\AppData\Roaming\open-ferry\config.yaml&quot; -data-dir &quot;C:\Users\me\AppData\Local\open-ferry&quot;</Arguments>";
    assert!(plain.contains(plain_line), "{plain}");
    assert_eq!(supervised, plain.replace(plain_line, supervised_line));
    // Both forms restart the wrapper if the scheduler fails to launch it.
    for xml in [&plain, &supervised] {
        assert!(
            xml.contains(
                "    <RestartOnFailure>\n      <Interval>PT1M</Interval>\n      <Count>10</Count>\n    </RestartOnFailure>\n  </Settings>"
            ),
            "{xml}"
        );
    }
}

#[test]
fn reads_the_flags_back_from_each_definition() {
    // Not upstream's: `uninstall` learns from the definition itself whether a
    // supervisor runs and where its data directory is.
    let unit = systemd::unit(&supervised_definition(), false);
    let words = systemd::exec_words(&unit).unwrap();
    let exe = words.first().cloned();
    assert_eq!(
        flagged(&words, exe.clone()),
        Flagged {
            supervised: true,
            data_dir: Some(LINUX_DATA.to_owned()),
            exe: Some(supervised_definition().exe),
            config: Some(LINUX_CONFIG.to_owned()),
        }
    );
    let plain = systemd::exec_words(&systemd::unit(&user_definition(), false)).unwrap();
    assert_eq!(
        flagged(&plain, None),
        Flagged {
            config: Some(LINUX_CONFIG.to_owned()),
            ..Flagged::default()
        }
    );
    // A path with what systemd expands, quoted and read back whole.
    let odd = Definition {
        data_dir: Some("/home/me/100%/$x \"q\"".to_owned()),
        ..supervised_definition()
    };
    let words = systemd::exec_words(&systemd::unit(&odd, false)).unwrap();
    assert_eq!(
        flagged(&words, None).data_dir.as_deref(),
        Some("/home/me/100%/$x \"q\"")
    );

    let plist = launchd::plist(
        &Definition {
            data_dir: Some("/Users/me/a&b".to_owned()),
            ..supervised_definition()
        },
        "/log",
        false,
    );
    let words = launchd::program_arguments(&plist).unwrap();
    assert_eq!(
        flagged(&words, None).data_dir.as_deref(),
        Some("/Users/me/a&b")
    );
    assert!(flagged(&words, None).supervised);

    let definition = windows_supervised_definition();
    let xml = windows::task_xml(&definition, SID);
    let words = windows::task_words(&xml).unwrap();
    assert_eq!(
        windows::task_command(&xml).as_deref(),
        Some(definition.exe.as_str())
    );
    assert_eq!(
        flagged(&words, windows::task_command(&xml)),
        Flagged {
            supervised: true,
            data_dir: Some(WINDOWS_DATA.to_owned()),
            exe: Some(definition.exe),
            config: Some(WINDOWS_CONFIG.to_owned()),
        }
    );
}

#[test]
fn the_data_directory_follows_xdg_then_home() {
    // Not upstream's: the same lookup `open-ferry update` makes.
    assert_eq!(data_dir(&linux()).as_deref(), Ok(LINUX_DATA));
    let xdg = context(
        Platform::Linux,
        "/home/me/.local/bin/open-ferry",
        "/",
        &[("HOME", "/home/me"), ("XDG_DATA_HOME", "/data")],
    );
    assert_eq!(data_dir(&xdg).as_deref(), Ok("/data/open-ferry"));
    assert_eq!(data_dir(&macos()).as_deref(), Ok(MACOS_DATA));
    assert_eq!(data_dir(&windows_with_data()).as_deref(), Ok(WINDOWS_DATA));
    assert!(data_dir(&windows()).is_err());
    let bare = context(Platform::Linux, "/bin/open-ferry", "/", &[]);
    assert!(data_dir(&bare).is_err());
    assert!(ENV_VARS.contains(&"XDG_DATA_HOME"));
}

#[test]
fn says_which_form_it_installs_and_why() {
    // Not upstream's: the choice is printed.
    let supervised = Updating::supervised(LINUX_DATA).form();
    assert!(
        supervised.starts_with("Installing the supervised form"),
        "{supervised}"
    );
    assert!(supervised.contains(LINUX_DATA), "{supervised}");
    let plain = Updating::plain(Ok(LINUX_DATA.to_owned()), "it is a build here").form();
    assert_eq!(
        plain,
        "Installing the plain form, without the supervisor: it is a build here."
    );
}

#[test]
fn installs_a_supervised_systemd_user_unit() {
    // Not upstream's: the choice reaches the unit and the record.
    let mut fake = Fake::default()
        .uid(1000)
        .file(LINUX_CONFIG, CONFIG)
        .dir("/home/me/.config");
    let updating = Updating::supervised(LINUX_DATA);
    let (result, out) = execute_updating(&mut fake, &linux(), &["install"], &updating);
    assert_eq!(result, Ok(()));
    assert_eq!(
        text(&fake, LINUX_UNIT),
        systemd::unit(&supervised_definition(), false)
    );
    assert!(out.starts_with("Installing the supervised form"), "{out}");
    let record: serde_json::Value =
        serde_json::from_str(&text(&fake, &format!("{LINUX_DATA}/service.json"))).unwrap();
    assert_eq!(record["supervised"], true);
    assert_eq!(record["manager"], "systemd");
    assert_eq!(record["data_dir"], LINUX_DATA);
    assert_eq!(
        events(&fake).last().copied(),
        Some("write_atomic /home/me/.local/share/open-ferry/service.json")
    );
}

#[test]
fn installs_a_supervised_scheduled_task() {
    // Not upstream's: the task's XML and the record for the supervised form.
    let mut fake = windows_user();
    let updating = Updating::supervised(WINDOWS_DATA);
    let (result, out) = execute_updating(&mut fake, &windows_with_data(), &["install"], &updating);
    assert_eq!(result, Ok(()));
    assert!(out.starts_with("Installing the supervised form"), "{out}");
    let record: serde_json::Value =
        serde_json::from_str(&text(&fake, &format!(r"{WINDOWS_DATA}\service.json"))).unwrap();
    assert_eq!(record["manager"], "task");
    assert_eq!(record["supervised"], true);
    assert_eq!(record["data_dir"], WINDOWS_DATA);
    // The task file is gone, and the record is written after the task runs.
    assert!(!fake.files.contains_key(TASK_FILE));
    let events = events(&fake);
    let run = events
        .iter()
        .position(|event| *event == "run schtasks.exe /run /tn open-ferry")
        .unwrap();
    let record = events
        .iter()
        .position(|event| event.starts_with("write_atomic ") && event.ends_with("service.json"))
        .unwrap();
    assert!(run < record, "{events:?}");
}

fn record_of(manager: &str, data_dir: &str, supervised: bool, exe: &str, config: &str) -> String {
    serde_json::json!({
        "format": 1,
        "kind": "user",
        "manager": manager,
        "supervised": supervised,
        "data_dir": data_dir,
        "exe": exe,
        "config": config,
    })
    .to_string()
}

/// The record of the scheduled task the test definitions install.
fn task_record(data_dir: &str, supervised: bool) -> String {
    let definition = windows_supervised_definition();
    record_of(
        "task",
        data_dir,
        supervised,
        &definition.exe,
        &definition.config,
    )
}

/// The record of the systemd user unit the test definitions install.
fn unit_record(data_dir: &str, supervised: bool) -> String {
    let definition = supervised_definition();
    record_of(
        "systemd",
        data_dir,
        supervised,
        &definition.exe,
        &definition.config,
    )
}

const XML_QUERY: &str = "schtasks.exe /query /tn open-ferry /xml";

fn supervised_task_fake() -> Fake {
    Fake::default().answer(
        XML_QUERY,
        0,
        &windows::task_xml(&windows_supervised_definition(), SID),
    )
}

fn stopped(fake: &Fake) -> Vec<&str> {
    fake.events
        .iter()
        .map(String::as_str)
        .filter(|event| event.starts_with("stop_supervisor"))
        .collect()
}

#[test]
fn uninstalls_a_task_by_asking_its_supervisor_to_stop_first() {
    // Not upstream's: stop, then end, then delete, and the record goes last.
    let record = format!(r"{WINDOWS_DATA}\service.json");
    let outcomes = [
        (StopOutcome::Stopped, "Stopped the supervisor\n"),
        (StopOutcome::NotRunning, "No supervisor was running\n"),
        (
            StopOutcome::TimedOut("still draining".to_owned()),
            "The supervisor didn't stop within 50 s (still draining); going on\n",
        ),
        (
            StopOutcome::Refused("no endpoint".to_owned()),
            "Failed to ask the supervisor to stop (no endpoint); going on\n",
        ),
    ];
    for (outcome, message) in outcomes {
        let mut fake = supervised_task_fake().file(&record, &task_record(WINDOWS_DATA, true));
        fake.stop = Some(outcome.clone());
        let (result, out) = execute_with(&mut fake, &windows_with_data(), &["uninstall"]);
        // Whatever the answer, the task is ended and deleted.
        assert_eq!(result, Ok(()), "{outcome:?}");
        let expected = [
            "run schtasks.exe /query /tn open-ferry".to_owned(),
            format!("run {XML_QUERY}"),
            format!("stop_supervisor {WINDOWS_DATA} 50s"),
            "run schtasks.exe /end /tn open-ferry".to_owned(),
            "run schtasks.exe /delete /tn open-ferry /f".to_owned(),
            format!("remove {record}"),
        ];
        assert_eq!(fake.events, expected, "{outcome:?}");
        assert!(
            out.starts_with(&format!(
                "The service's data directory is {WINDOWS_DATA} (from its definition).\n{message}Ran: schtasks.exe /end"
            )),
            "{outcome:?}: {out}"
        );
        assert!(!fake.files.contains_key(&record));
    }
}

#[test]
fn finds_the_data_directory_from_the_definition_then_the_record_then_the_environment() {
    // Not upstream's: the order of the lookup `uninstall` makes.
    let record_path = format!(r"{WINDOWS_DATA}\service.json");

    // The definition's own `-data-dir` wins over the record.
    let pinned = Definition {
        data_dir: Some(r"D:\pinned".to_owned()),
        ..windows_supervised_definition()
    };
    let mut fake = Fake::default()
        .answer(XML_QUERY, 0, &windows::task_xml(&pinned, SID))
        .file(&record_path, &task_record(WINDOWS_DATA, true));
    let (result, out) = execute_with(&mut fake, &windows_with_data(), &["uninstall"]);
    assert_eq!(result, Ok(()));
    assert_eq!(stopped(&fake), [r"stop_supervisor D:\pinned 50s"]);
    assert!(
        out.contains(r"The service's data directory is D:\pinned (from its definition)."),
        "{out}"
    );

    // Without a readable definition no record can be checked, so the
    // environment's directory is asked, and the message says it may not be
    // the task's own.
    let mut fake = Fake::default()
        .fail(XML_QUERY, 1, "denied")
        .file(&record_path, &task_record(WINDOWS_DATA, true));
    let (result, out) = execute_with(&mut fake, &windows_with_data(), &["uninstall"]);
    assert_eq!(result, Ok(()));
    assert_eq!(
        stopped(&fake),
        [format!("stop_supervisor {WINDOWS_DATA} 50s")]
    );
    assert!(out.contains(UNREADABLE), "{out}");
    assert!(
        out.contains("because the task's definition couldn't be read, so the supervisor of this directory is asked to stop, which may not be the task's own"),
        "{out}"
    );
    assert!(fake.files.contains_key(&record_path));

    // A readable definition with no `-data-dir` of its own: the record in
    // the environment's directory, which matches it, names the directory,
    // and it is removed with the task.
    let plain = Definition {
        data_dir: None,
        ..windows_supervised_definition()
    };
    let mut fake = Fake::default()
        .answer(XML_QUERY, 0, &windows::task_xml(&plain, SID))
        .file(&record_path, &task_record(WINDOWS_DATA, false));
    let (result, out) = execute_with(&mut fake, &windows_with_data(), &["uninstall"]);
    assert_eq!(result, Ok(()));
    assert!(out.contains(&format!("(from {record_path})")), "{out}");
    assert!(!out.contains("Leaving"), "{out}");
    assert!(!fake.files.contains_key(&record_path));

    // With neither, the directory the environment gives is asked.
    let mut fake = Fake::default().fail(XML_QUERY, 1, "denied");
    let (result, out) = execute_with(&mut fake, &windows_with_data(), &["uninstall"]);
    assert_eq!(result, Ok(()));
    assert_eq!(
        stopped(&fake),
        [format!("stop_supervisor {WINDOWS_DATA} 50s")]
    );
    assert!(out.contains("worked out from the environment"), "{out}");

    // With no data directory at all nothing is asked, and it says so.
    let mut fake = Fake::default().fail(XML_QUERY, 1, "denied");
    let (result, out) = execute_with(&mut fake, &windows(), &["uninstall"]);
    assert_eq!(result, Ok(()));
    assert!(stopped(&fake).is_empty());
    assert!(
        out.contains("There is no data directory to ask a supervisor in"),
        "{out}"
    );
}

#[test]
fn a_plain_task_is_not_asked_to_stop() {
    // Not upstream's: only a supervised task has a supervisor to ask.
    let plain = Definition {
        data_dir: None,
        ..windows_supervised_definition()
    };
    let mut fake = Fake::default().answer(XML_QUERY, 0, &windows::task_xml(&plain, SID));
    let (result, _) = execute_with(&mut fake, &windows_with_data(), &["uninstall"]);
    assert_eq!(result, Ok(()));
    assert!(stopped(&fake).is_empty());

    // A record that says plain does not count when the definition can't be
    // read: it is left alone, and the environment's directory is asked.
    let record = format!(r"{WINDOWS_DATA}\service.json");
    let mut fake = Fake::default()
        .fail(XML_QUERY, 1, "denied")
        .file(&record, &task_record(WINDOWS_DATA, false));
    let (result, out) = execute_with(&mut fake, &windows_with_data(), &["uninstall"]);
    assert_eq!(result, Ok(()));
    assert_eq!(
        stopped(&fake),
        [format!("stop_supervisor {WINDOWS_DATA} 50s")]
    );
    assert!(out.contains(UNREADABLE), "{out}");
    assert!(fake.files.contains_key(&record));
}

#[test]
fn a_supervised_unit_is_stopped_by_systemd_alone() {
    // Not upstream's: systemd and launchd stop the whole unit or job within
    // their time-outs, so no supervisor is asked, and the record goes last.
    let record = format!("{LINUX_DATA}/service.json");
    let mut fake = Fake::default()
        .uid(1000)
        .file(LINUX_UNIT, &systemd::unit(&supervised_definition(), false))
        .file(&record, &unit_record(LINUX_DATA, true));
    let (result, out) = execute_with(&mut fake, &linux(), &["uninstall"]);
    assert_eq!(result, Ok(()));
    assert_eq!(
        events(&fake),
        [
            "run id -u",
            "run systemctl --user disable --now open-ferry.service",
            &format!("remove {LINUX_UNIT}"),
            "run systemctl --user daemon-reload",
            &format!("remove {record}"),
        ]
    );
    assert!(out.contains("(from its definition)"), "{out}");
}

#[test]
fn a_record_that_is_not_ours_is_ignored() {
    // Not upstream's: `service.json` is read with care.
    let good = unit_record(LINUX_DATA, true);
    let definition = supervised_definition();
    assert_eq!(
        parse_record(good.as_bytes()),
        Some(Record {
            manager: "systemd".to_owned(),
            supervised: true,
            data_dir: LINUX_DATA.to_owned(),
            exe: definition.exe,
            config: definition.config,
        })
    );
    let tail = r#""exe":"/e","config":"/c""#;
    for bad in [
        String::new(),
        "not json".to_owned(),
        format!(
            r#"{{"format":2,"kind":"user","manager":"systemd","supervised":true,"data_dir":"/a",{tail}}}"#
        ),
        format!(
            r#"{{"format":1,"kind":"system","manager":"systemd","supervised":true,"data_dir":"/a",{tail}}}"#
        ),
        format!(r#"{{"format":1,"kind":"user","manager":"systemd","data_dir":"/a",{tail}}}"#),
        format!(r#"{{"format":1,"kind":"user","supervised":true,"data_dir":"/a",{tail}}}"#),
        r#"{"format":1,"kind":"user","manager":"systemd","supervised":true,"data_dir":"/a"}"#
            .to_owned(),
    ] {
        assert_eq!(parse_record(bad.as_bytes()), None, "{bad}");
    }
}

/// What an uninstall of the scheduled task with `record` at the data
/// directory's `service.json` and the supervised task's definition does.
fn uninstall_task_with(record: &str) -> (Fake, Result<(), String>, String) {
    let mut fake = supervised_task_fake().file(&format!(r"{WINDOWS_DATA}\service.json"), record);
    let (result, out) = execute_with(&mut fake, &windows_with_data(), &["uninstall"]);
    (fake, result, out)
}

#[test]
fn a_record_naming_another_directory_is_not_used_or_removed() {
    // Not upstream's: a record is the service's only if it names the
    // directory it is in, so a stale or crafted one can't point the stop (or
    // the removal) at another data directory.
    let path = format!(r"{WINDOWS_DATA}\service.json");
    let other = r"D:\other";
    // The definition is readable, so the record is checked against it.
    let (fake, result, out) = uninstall_task_with(&task_record(other, true));
    assert_eq!(result, Ok(()));
    assert_eq!(
        stopped(&fake),
        [format!("stop_supervisor {WINDOWS_DATA} 50s")]
    );
    assert!(
        out.contains(&format!(
            "Leaving {path} alone and not using it: it names {other}, not the directory it is in."
        )),
        "{out}"
    );
    // It is not the service's, so it stays.
    assert!(fake.files.contains_key(&path));
    assert!(!fake.events.contains(&format!("remove {path}")));
}

#[test]
fn a_record_of_another_manager_is_not_used_or_removed() {
    let path = format!(r"{WINDOWS_DATA}\service.json");
    let definition = windows_supervised_definition();
    let record = record_of(
        "systemd",
        WINDOWS_DATA,
        true,
        &definition.exe,
        &definition.config,
    );
    let mut fake = supervised_task_fake().file(&path, &record);
    let (result, out) = execute_with(&mut fake, &windows_with_data(), &["uninstall"]);
    assert_eq!(result, Ok(()));
    assert!(
        out.contains("it is of a systemd service, not a task one"),
        "{out}"
    );
    // Not used: the data directory is the definition's.
    assert!(out.contains("(from its definition)"), "{out}");
    assert!(fake.files.contains_key(&path));
}

#[test]
fn a_record_that_differs_from_the_definition_is_not_used_or_removed() {
    let path = format!(r"{WINDOWS_DATA}\service.json");
    let definition = windows_supervised_definition();
    for (record, said) in [
        (
            record_of(
                "task",
                WINDOWS_DATA,
                true,
                r"C:\elsewhere\open-ferry.exe",
                &definition.config,
            ),
            "as the binary",
        ),
        (
            record_of(
                "task",
                WINDOWS_DATA,
                true,
                &definition.exe,
                r"C:\elsewhere\config.yaml",
            ),
            "as the config",
        ),
    ] {
        let (fake, result, out) = uninstall_task_with(&record);
        assert_eq!(result, Ok(()));
        assert!(
            out.contains(&format!("Leaving {path} alone and not using it: it names"))
                && out.contains(said),
            "{out}"
        );
        assert!(fake.files.contains_key(&path), "{record}");
    }
    // Paths compare as Windows compares them: by case.
    let record = record_of(
        "task",
        &WINDOWS_DATA.to_uppercase(),
        true,
        &definition.exe.to_uppercase(),
        &definition.config.to_lowercase(),
    );
    let (fake, result, out) = uninstall_task_with(&record);
    assert_eq!(result, Ok(()));
    assert!(!out.contains("Leaving"), "{out}");
    assert!(!fake.files.contains_key(&path));
}

#[test]
fn only_the_services_own_record_is_in_the_uninstall_plan() {
    // Not upstream's: a rejected `service.json` is not in the plan's
    // removals, and a valid one is, last.
    let path = format!(r"{WINDOWS_DATA}\service.json");
    let definition = windows_supervised_definition();
    let rejected = [
        // Another format, and a system service's.
        r#"{"format":2,"kind":"user"}"#.to_owned(),
        serde_json::json!({
            "format": 1, "kind": "system", "manager": "task", "supervised": true,
            "data_dir": WINDOWS_DATA, "exe": definition.exe, "config": definition.config,
        })
        .to_string(),
        task_record(r"D:\other", true),
        "not json".to_owned(),
    ];
    for record in rejected {
        let mut fake = supervised_task_fake().file(&path, &record);
        let (result, out) =
            execute_with(&mut fake, &windows_with_data(), &["uninstall", "-dry-run"]);
        assert_eq!(result, Ok(()));
        assert!(!out.contains("Would remove"), "{record}: {out}");
        assert!(
            out.contains(&format!("Leaving {path} alone")),
            "{record}: {out}"
        );
        // The real thing leaves it too.
        let mut fake = supervised_task_fake().file(&path, &record);
        let (result, _) = execute_with(&mut fake, &windows_with_data(), &["uninstall"]);
        assert_eq!(result, Ok(()));
        assert!(fake.files.contains_key(&path), "{record}");
    }
    let mut fake = supervised_task_fake().file(&path, &task_record(WINDOWS_DATA, true));
    let (result, out) = execute_with(&mut fake, &windows_with_data(), &["uninstall", "-dry-run"]);
    assert_eq!(result, Ok(()));
    let removals: Vec<&str> = out
        .lines()
        .filter(|line| line.starts_with("Would remove"))
        .collect();
    assert_eq!(removals, [format!("Would remove {path}")]);
    let last = out.lines().rev().nth(1).unwrap();
    assert_eq!(last, format!("Would remove {path}"), "{out}");
}

#[test]
fn the_record_removed_is_in_the_directory_the_definition_names() {
    // Not upstream's: a definition pinned to A, run where the environment
    // gives B, has A's record looked at and removed; B's isn't this
    // service's, so it is neither used, removed nor mentioned.
    let a = r"D:\pinned";
    let a_path = format!(r"{a}\service.json");
    let b_path = format!(r"{WINDOWS_DATA}\service.json");
    let pinned = Definition {
        data_dir: Some(a.to_owned()),
        ..windows_supervised_definition()
    };
    let xml = windows::task_xml(&pinned, SID);
    let run = |records: &[(&str, String)]| {
        let mut fake = Fake::default().answer(XML_QUERY, 0, &xml);
        for (path, record) in records {
            fake = fake.file(path, record);
        }
        let (result, out) = execute_with(&mut fake, &windows_with_data(), &["uninstall"]);
        assert_eq!(result, Ok(()), "{out}");
        assert!(!out.contains("Leaving"), "{out}");
        assert_eq!(stopped(&fake), [format!("stop_supervisor {a} 50s")]);
        fake
    };

    // Records in both: A's goes, B's stays.
    let fake = run(&[
        (&a_path, task_record(a, true)),
        (&b_path, task_record(WINDOWS_DATA, true)),
    ]);
    assert!(fake.events.contains(&format!("remove {a_path}")));
    assert!(!fake.files.contains_key(&a_path));
    assert!(fake.files.contains_key(&b_path));
    assert!(!fake.events.contains(&format!("remove {b_path}")));

    // Only in A: it goes.
    let fake = run(&[(&a_path, task_record(a, true))]);
    assert!(!fake.files.contains_key(&a_path));

    // Only in B: nothing is removed.
    let fake = run(&[(&b_path, task_record(WINDOWS_DATA, true))]);
    assert!(fake.files.contains_key(&b_path));
    assert!(!fake.events.iter().any(|event| event.starts_with("remove ")));
}

/// `xml` with the task's `<Arguments>` replaced by `arguments`.
fn with_arguments(xml: &str, arguments: &str) -> String {
    let start = xml.find("<Arguments>").unwrap();
    let end = xml.find("</Arguments>").unwrap() + "</Arguments>".len();
    format!(
        "{}<Arguments>{arguments}</Arguments>{}",
        &xml[..start],
        &xml[end..]
    )
}

#[test]
fn a_record_or_definition_without_full_paths_is_not_used_or_removed() {
    // Not upstream's: paths are compared only when they are full ones, so
    // `home/me/x` is not `/home/me/x`.
    let path = format!(r"{WINDOWS_DATA}\service.json");
    let definition = windows_supervised_definition();
    let xml = windows::task_xml(&definition, SID);
    let left_alone = |fake: &Fake, out: &str, said: &str| {
        assert!(
            out.contains(&format!("Leaving {path} alone and not using it: ")) && out.contains(said),
            "{out}"
        );
        assert!(fake.files.contains_key(&path));
        assert!(!fake.events.contains(&format!("remove {path}")));
    };

    // Relative paths in the record.
    for (record, said) in [
        (
            record_of(
                "task",
                WINDOWS_DATA,
                true,
                &definition.exe,
                r"Roaming\config.yaml",
            ),
            "its config, Roaming\\config.yaml, isn't a full path",
        ),
        (
            record_of(
                "task",
                WINDOWS_DATA,
                true,
                "open-ferry.exe",
                &definition.config,
            ),
            "its binary, open-ferry.exe, isn't a full path",
        ),
        (
            record_of(
                "task",
                "local/open-ferry",
                true,
                &definition.exe,
                &definition.config,
            ),
            "its data directory, local/open-ferry, isn't a full path",
        ),
    ] {
        let mut fake = Fake::default()
            .answer(XML_QUERY, 0, &xml)
            .file(&path, &record);
        let (result, out) = execute_with(&mut fake, &windows_with_data(), &["uninstall"]);
        assert_eq!(result, Ok(()));
        left_alone(&fake, &out, said);
    }

    // A readable definition whose config is relative, or missing.
    let record = task_record(WINDOWS_DATA, true);
    for (arguments, said) in [
        (
            format!(r#"service run -supervise -config config.yaml -data-dir "{WINDOWS_DATA}""#),
            "the installed service's config, config.yaml, isn't a full path",
        ),
        (
            format!(r#"service run -supervise -data-dir "{WINDOWS_DATA}""#),
            "the installed service's definition doesn't name its config",
        ),
    ] {
        let mut fake = Fake::default()
            .answer(XML_QUERY, 0, &with_arguments(&xml, &arguments))
            .file(&path, &record);
        let (result, out) = execute_with(&mut fake, &windows_with_data(), &["uninstall"]);
        assert_eq!(result, Ok(()));
        left_alone(&fake, &out, said);
    }

    // The same on Unix: the definition's `home/me/..` is not the record's
    // `/home/me/..`.
    let unit_path = format!("{LINUX_DATA}/service.json");
    let relative = Definition {
        config: "home/me/.config/open-ferry/config.yaml".to_owned(),
        ..supervised_definition()
    };
    let record = record_of(
        "systemd",
        LINUX_DATA,
        true,
        &relative.exe,
        "/home/me/.config/open-ferry/config.yaml",
    );
    let mut fake = Fake::default()
        .uid(1000)
        .file(LINUX_UNIT, &systemd::unit(&relative, false))
        .file(&unit_path, &record);
    let (result, out) = execute_with(&mut fake, &linux(), &["uninstall"]);
    assert_eq!(result, Ok(()), "{out}");
    assert!(out.contains("isn't a full path"), "{out}");
    assert!(fake.files.contains_key(&unit_path));
}

#[test]
fn a_relative_data_dir_in_the_definition_is_never_a_stop_target() {
    // Not upstream's: a relative `-data-dir` would be read from the
    // uninstall's own working directory, so no supervisor is asked there,
    // nor in the record's or the environment's directory in its place; the
    // task is still ended and deleted.
    let env_record = format!(r"{WINDOWS_DATA}\service.json");
    let relative_record = r"data\service.json";
    let pinned = Definition {
        data_dir: Some("data".to_owned()),
        ..windows_supervised_definition()
    };
    let mut fake = Fake::default()
        .answer(XML_QUERY, 0, &windows::task_xml(&pinned, SID))
        .file(&env_record, &task_record(WINDOWS_DATA, true))
        .file(relative_record, &task_record("data", true));
    let (result, out) = execute_with(&mut fake, &windows_with_data(), &["uninstall"]);
    assert_eq!(result, Ok(()), "{out}");
    assert!(stopped(&fake).is_empty(), "{:?}", fake.events);
    assert!(
        out.contains(
            "There is no data directory to ask a supervisor in (its definition names data as its data directory, which isn't a full path), so none is asked to stop."
        ),
        "{out}"
    );
    assert!(!out.contains("Leaving"), "{out}");
    assert_eq!(
        fake.events,
        [
            "run schtasks.exe /query /tn open-ferry".to_owned(),
            format!("run {XML_QUERY}"),
            "run schtasks.exe /end /tn open-ferry".to_owned(),
            "run schtasks.exe /delete /tn open-ferry /f".to_owned(),
        ]
    );
    assert!(fake.files.contains_key(&env_record));
    assert!(fake.files.contains_key(relative_record));

    // A unit pinning a relative directory: its records are left alone too.
    let unit_record_path = format!("{LINUX_DATA}/service.json");
    let relative = Definition {
        data_dir: Some("data".to_owned()),
        ..supervised_definition()
    };
    let mut fake = Fake::default()
        .uid(1000)
        .file(LINUX_UNIT, &systemd::unit(&relative, false))
        .file(&unit_record_path, &unit_record(LINUX_DATA, true))
        .file("data/service.json", &unit_record("data", true));
    let (result, out) = execute_with(&mut fake, &linux(), &["uninstall"]);
    assert_eq!(result, Ok(()), "{out}");
    assert!(
        !fake
            .events
            .iter()
            .any(|e| e.starts_with("remove ") && e.ends_with("service.json")),
        "{:?}",
        fake.events
    );
    assert!(fake.files.contains_key(&unit_record_path));
    assert!(fake.files.contains_key("data/service.json"));
    assert!(!out.contains("Leaving"), "{out}");
}

#[test]
fn a_task_with_no_arguments_is_a_readable_definition() {
    // Not upstream's: the scheduler gave the task back, so a stale record in
    // the environment's directory is compared to it (and found not to
    // match), and the task is not taken for a supervised one.
    let path = format!(r"{WINDOWS_DATA}\service.json");
    let xml = windows::task_xml(&windows_supervised_definition(), SID);
    let start = xml.find("<Arguments>").unwrap();
    let end = xml.find("</Arguments>").unwrap() + "</Arguments>".len();
    for element in [
        "",
        "<Arguments/>",
        "<Arguments />",
        "<Arguments></Arguments>",
    ] {
        let without = format!("{}{element}{}", &xml[..start], &xml[end..]);
        let mut fake = Fake::default()
            .answer(XML_QUERY, 0, &without)
            .file(&path, &task_record(WINDOWS_DATA, true));
        let (result, out) = execute_with(&mut fake, &windows_with_data(), &["uninstall"]);
        assert_eq!(result, Ok(()), "{element}: {out}");
        assert!(
            out.contains(&format!(
                "Leaving {path} alone and not using it: the installed service's definition doesn't name its config."
            )),
            "{element}: {out}"
        );
        assert!(stopped(&fake).is_empty(), "{element}: {:?}", fake.events);
        assert!(fake.files.contains_key(&path), "{element}");
    }
    // A query that failed is still no definition at all.
    let mut fake = Fake::default()
        .fail(XML_QUERY, 1, "denied")
        .file(&path, &task_record(WINDOWS_DATA, true));
    let (result, _) = execute_with(&mut fake, &windows_with_data(), &["uninstall"]);
    assert_eq!(result, Ok(()));
    assert_eq!(
        stopped(&fake),
        [format!("stop_supervisor {WINDOWS_DATA} 50s")]
    );
}

#[test]
fn numeric_character_references_are_decoded_in_one_pass() {
    // Not upstream's: other writers than ours and the scheduler write them.
    let command = |text: &str| {
        format!("<Actions><Exec><Command>{text}</Command><Arguments>x</Arguments></Exec></Actions>")
    };
    for text in [
        r"&#34;C:\Program Files\x\open-ferry.exe&#34;",
        r"&#x22;C:\Program Files\x\open-ferry.exe&#x22;",
        r"&#X22;C:\Program Files\x\open-ferry.exe&quot;",
    ] {
        assert_eq!(
            windows::task_command(&command(text)).as_deref(),
            Some(r"C:\Program Files\x\open-ferry.exe"),
            "{text}"
        );
    }
    let arguments = r"<Actions><Exec><Command>x</Command><Arguments>service run -config &#34;C:\My Dir\config.yaml&#34; -supervise</Arguments></Exec></Actions>";
    assert_eq!(
        windows::task_words(arguments),
        words(&[
            "service",
            "run",
            "-config",
            r"C:\My Dir\config.yaml",
            "-supervise"
        ])
    );
    // What a reference decodes to is not decoded again.
    assert_eq!(launchd::unescape("&amp;#34;").as_deref(), Some("&#34;"));
    assert_eq!(launchd::unescape("&amp;amp;").as_deref(), Some("&amp;"));
    assert_eq!(launchd::unescape("&#38;#34;").as_deref(), Some("&#34;"));
    // Invalid or out of range references, unknown entities and a bare `&`
    // are unreadable.
    for text in [
        "&#;",
        "&#x;",
        "&#xD800;",
        "&#1114112;",
        "&#-34;",
        "&#0;",
        "&#abc;",
        "&nope;",
        "&",
        "a & b",
        "&#34",
        "&nbsp;",
    ] {
        assert_eq!(launchd::unescape(text), None, "{text}");
    }
    assert_eq!(
        launchd::unescape("a&#47;b&#x2F;c").as_deref(),
        Some("a/b/c")
    );
    // Any number of leading zeros; no `;` is unreadable.
    assert_eq!(launchd::unescape("&#0000000047;").as_deref(), Some("/"));
    assert_eq!(
        launchd::unescape("&#x00000000000002F;").as_deref(),
        Some("/")
    );
    assert_eq!(
        launchd::unescape(&format!("&#{}47;", "0".repeat(500))).as_deref(),
        Some("/")
    );
    for text in [
        format!("&#{}", "0".repeat(500)),
        format!("&#x{}", "0".repeat(500)),
        format!("&#{}47 and a; later", "0".repeat(50)),
        "&#99999999999;".to_owned(),
        "&#xFFFFFFFFF;".to_owned(),
        "&#4x7;".to_owned(),
        "&#x4g;".to_owned(),
    ] {
        assert_eq!(launchd::unescape(&text), None, "{text}");
    }
    // Only XML 1.0 `Char`s decode.
    for text in [
        "&#1;",
        "&#8;",
        "&#xB;",
        "&#xC;",
        "&#x1F;",
        "&#xFFFE;",
        "&#xFFFF;",
        "&#xDFFF;",
        "&#x110000;",
    ] {
        assert_eq!(launchd::unescape(text), None, "{text}");
    }
    assert_eq!(
        launchd::unescape("&#9;&#xA;&#13;&#x20;&#xD7FF;&#xE000;&#xFFFD;&#x10000;&#x10FFFF;")
            .as_deref(),
        Some("\t\n\r \u{D7FF}\u{E000}\u{FFFD}\u{10000}\u{10FFFF}")
    );
    let plist = "<plist><dict><key>ProgramArguments</key><array><string>&#47;usr&#47;bin&#47;open-ferry</string></array></dict></plist>";
    assert_eq!(
        launchd::program_arguments(plist),
        Some(vec!["/usr/bin/open-ferry".to_owned()])
    );
}

#[test]
fn a_task_command_with_spaces_is_one_path() {
    // Not upstream's: `<Command>` holds one path, quoted or not.
    let xml = |command: &str| {
        format!(
            "<Actions><Exec><Command>{command}</Command><Arguments>x</Arguments></Exec></Actions>"
        )
    };
    for (command, expected) in [
        (
            r"C:\Program Files\open-ferry\open-ferry.exe",
            r"C:\Program Files\open-ferry\open-ferry.exe",
        ),
        (
            "&quot;C:\\Program Files\\open-ferry\\open-ferry.exe&quot;",
            r"C:\Program Files\open-ferry\open-ferry.exe",
        ),
        (
            "  C:\\Program Files\\a &amp; b\\of.exe \n",
            r"C:\Program Files\a & b\of.exe",
        ),
        (r"C:\open-ferry\of.exe", r"C:\open-ferry\of.exe"),
    ] {
        assert_eq!(
            windows::task_command(&xml(command)).as_deref(),
            Some(expected),
            "{command}"
        );
    }
    assert_eq!(windows::task_command(&xml("  ")), None);

    // What `task_xml` writes comes back, for a path with spaces and one with
    // an ampersand.
    for exe in [
        r"C:\Program Files\open-ferry\open-ferry.exe",
        r"C:\Users\me\a & b\open-ferry.exe",
        r"C:\Users\me\AppData\Local\Programs\open-ferry\open-ferry.exe",
    ] {
        let definition = Definition {
            exe: exe.to_owned(),
            ..windows_supervised_definition()
        };
        assert_eq!(
            windows::task_command(&windows::task_xml(&definition, SID)).as_deref(),
            Some(exe)
        );
    }
}

#[test]
fn a_task_is_read_only_when_it_has_our_one_action() {
    // Not upstream's: anything but our shape is unreadable, which keeps the
    // stop check on.
    let xml = windows::task_xml(&windows_supervised_definition(), SID);
    let unreadable = |xml: &str, why: &str| {
        assert_eq!(windows::task_definition(xml), None, "{why}");
    };
    assert!(windows::task_definition(&xml).is_some());
    let cmd_exec = "<Exec><Command>cmd.exe</Command><Arguments>/c exit 0</Arguments></Exec>";
    let actions = "<Actions Context=\"Author\">\n";
    assert!(xml.contains(actions));
    unreadable(
        &xml.replace(actions, &format!("{actions}{cmd_exec}\n")),
        "cmd first",
    );
    unreadable(
        &xml.replace("  </Actions>", &format!("{cmd_exec}\n  </Actions>")),
        "cmd second",
    );
    unreadable(
        &xml.replace(
            "  </Actions>",
            "<ComHandler><ClassId>{x}</ClassId></ComHandler>\n  </Actions>",
        ),
        "com handler",
    );
    unreadable(
        &xml.replace("</Actions>", "</Actions><Command>x</Command>"),
        "command outside",
    );
    unreadable(
        &xml.replace("</Arguments>", "</Arguments><Arguments>x</Arguments>"),
        "two arguments",
    );
    unreadable(
        &xml.replace("</Actions>", "</Actions><Actions/>"),
        "two actions",
    );
    unreadable(
        &xml.replace("<Actions Context=\"Author\">", "<Actions/><X>"),
        "empty",
    );
    unreadable(&xml.replace("<Task ", "<!-- x --><Task "), "comment");
    unreadable(
        &with_arguments(&xml, "<![CDATA[service run -supervise]]>"),
        "cdata",
    );
    for arguments in [
        "service run\u{a0}-supervise",
        "service run\u{2003}-supervise",
        "service run\n-supervise",
        "service run &nbsp;-supervise",
        "service run & -supervise",
        "service run &#0; -supervise",
    ] {
        unreadable(&with_arguments(&xml, arguments), arguments);
        // The words alone are unreadable when the arguments are the defect.
        assert_eq!(
            windows::task_words(&with_arguments(&xml, arguments)),
            None,
            "{arguments}"
        );
    }
    for broken in [
        xml.replace("</Arguments>", "</Arguments><Arguments>x</Arguments>"),
        xml.replace("<Task ", "<!-- x --><Task "),
        xml.replace(actions, &format!("{actions}{cmd_exec}\n")),
    ] {
        assert_eq!(windows::task_words(&broken), None);
    }
    unreadable(
        &xml.replace("<Command>", "<Command>&nbsp;"),
        "unknown entity in command",
    );
    unreadable(
        &xml.replace("<Command>", "<Command>a & "),
        "bare & in command",
    );
    unreadable(
        &xml.replace("<Command>", "<Command><b/>"),
        "markup in command",
    );
    // The `ExecutionTimeLimit` among the settings is no action.
    assert!(xml.contains("<ExecutionTimeLimit>"));

    // Through `uninstall`: a stale `supervised: false` record is left alone
    // and the supervisor of the environment's directory is asked.
    let two = xml.replace(actions, &format!("{actions}{cmd_exec}\n"));
    let path = format!(r"{WINDOWS_DATA}\service.json");
    let mut fake = Fake::default()
        .answer(XML_QUERY, 0, &two)
        .file(&path, &task_record(WINDOWS_DATA, false));
    let (result, out) = execute_with(&mut fake, &windows_with_data(), &["uninstall"]);
    assert_eq!(result, Ok(()), "{out}");
    assert!(out.contains(UNREADABLE), "{out}");
    assert_eq!(
        stopped(&fake),
        [format!("stop_supervisor {WINDOWS_DATA} 50s")]
    );
    assert!(fake.files.contains_key(&path));
}

#[test]
fn what_task_xml_writes_reads_back_however_it_is_laid_out() {
    // Not upstream's: odd characters in paths, and the XML as the scheduler
    // may print it (CRLF, re-indented).
    let odd = r#"C:\Users\me\a "q" b\\"c&d<e>é\o\config.yaml"#;
    let data = r#"D:\data "x"\\&é<"#;
    for supervised in [false, true] {
        for exe in [
            r"C:\Program Files\open-ferry\open-ferry.exe",
            r"C:\Users\me\a & b é<\open-ferry.exe",
        ] {
            let definition = Definition {
                data_dir: supervised.then(|| data.to_owned()),
                exe: exe.to_owned(),
                config: odd.to_owned(),
                dir: "C:\\Users\\me\\My\u{a0}Dir".to_owned(),
            };
            let xml = windows::task_xml(&definition, SID);
            let crlf = xml.replace('\n', "\r\n");
            let reindented = xml
                .lines()
                .map(|line| format!("\t\t{}\r\n", line.trim_start()))
                .collect::<String>();
            for (layout, text) in [
                ("as is", &xml),
                ("crlf", &crlf),
                ("re-indented", &reindented),
            ] {
                let (words, found) = windows::task_definition(text)
                    .unwrap_or_else(|| panic!("{layout}: {exe} unreadable"));
                assert_eq!(found.as_deref(), Some(exe), "{layout}");
                let flags = flagged(&words, found);
                assert_eq!(flags.supervised, supervised, "{layout}");
                assert_eq!(flags.config.as_deref(), Some(odd), "{layout}");
                assert_eq!(
                    flags.data_dir.as_deref(),
                    supervised.then_some(data),
                    "{layout}"
                );
                assert!(
                    words
                        .iter()
                        .any(|word| word == "C:\\Users\\me\\My\u{a0}Dir"),
                    "{layout}: {words:?}"
                );
            }
        }
    }
}

#[test]
fn the_task_schedulers_own_serialization_of_our_task_reads_back() {
    // Not upstream's. This text did not come from our generator: it is what
    // the Task Scheduler's serializer (COM `NewTask(0)`, set `XmlText`, read
    // `XmlText` back; the same one `schtasks /query /xml` prints with) gives
    // for our task, minus the byte order mark. It has literal `"` and `'` in
    // the text, `&amp;`, a literal é, and a UTF-16 prolog.
    let lf = r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>test</Description>
    <URI>\open-ferry-xmlcheck</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>S-1-5-REDACTED</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>S-1-5-REDACTED</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>true</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>true</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>true</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>10</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>"C:\Users\me\AppData\Local\Programs\open ferry\open-ferry.exe"</Command>
      <Arguments>service run -supervise -config "C:\Users\me\My Odd&amp;Dir's é\config.yaml" -data-dir "C:\Users\me\data dir &amp; 100%\open-ferry"</Arguments>
      <WorkingDirectory>C:\Users\me\My Odd&amp;Dir</WorkingDirectory>
    </Exec>
  </Actions>
</Task>"#;
    let exe = r"C:\Users\me\AppData\Local\Programs\open ferry\open-ferry.exe";
    let config = r"C:\Users\me\My Odd&Dir's é\config.yaml";
    let data = r"C:\Users\me\data dir & 100%\open-ferry";
    for (layout, text) in [("lf", lf.to_owned()), ("crlf", lf.replace('\n', "\r\n"))] {
        let (found_words, found) =
            windows::task_definition(&text).unwrap_or_else(|| panic!("{layout}: unreadable"));
        assert_eq!(found.as_deref(), Some(exe), "{layout}");
        assert_eq!(
            Some(found_words.clone()),
            words(&[
                "service",
                "run",
                "-supervise",
                "-config",
                config,
                "-data-dir",
                data
            ]),
            "{layout}"
        );
        assert_eq!(windows::task_words(&text), Some(found_words.clone()));
        let flags = flagged(&found_words, found);
        assert!(flags.supervised, "{layout}");
        assert_eq!(flags.data_dir.as_deref(), Some(data), "{layout}");
        assert_eq!(flags.config.as_deref(), Some(config), "{layout}");
    }
}

const SYSTEMD_STOP: &str = "systemctl --user disable --now open-ferry.service";
const SYSTEMD_ACTIVE: &str = "systemctl --user is-active open-ferry.service";

fn supervised_unit_fake(record: &str) -> Fake {
    Fake::default()
        .uid(1000)
        .file(LINUX_UNIT, &systemd::unit(&supervised_definition(), false))
        .file(record, &unit_record(LINUX_DATA, true))
}

#[test]
fn a_supervised_unit_that_cannot_be_stopped_and_may_run_is_kept() {
    // Not upstream's: nothing is removed while the manager can't say the
    // supervisor is down.
    let record = format!("{LINUX_DATA}/service.json");
    let answers: [(Fake, &str); 3] = [
        (
            supervised_unit_fake(&record)
                .fail(SYSTEMD_STOP, 1, "Failed to stop")
                .answer(SYSTEMD_ACTIVE, 0, "active\n"),
            "running",
        ),
        // The query itself fails in a way that proves nothing.
        (
            supervised_unit_fake(&record)
                .fail(SYSTEMD_STOP, 1, "Failed to stop")
                .fail(SYSTEMD_ACTIVE, 1, "Failed to connect to bus"),
            "bus",
        ),
        (
            supervised_unit_fake(&record)
                .fail(SYSTEMD_STOP, 1, "Failed to stop")
                .answer(SYSTEMD_ACTIVE, 3, "deactivating\n"),
            "deactivating",
        ),
    ];
    for (mut fake, case) in answers {
        let (result, _) = execute_with(&mut fake, &linux(), &["uninstall"]);
        let error = result.unwrap_err();
        assert!(error.contains("may still be running"), "{case}: {error}");
        assert!(error.contains(SYSTEMD_STOP), "{case}: {error}");
        assert!(
            error.contains("run the uninstall again; that is safe"),
            "{case}: {error}"
        );
        assert!(fake.files.contains_key(LINUX_UNIT), "{case}");
        assert!(fake.files.contains_key(&record), "{case}");
        assert!(
            !fake.events.iter().any(|event| event.starts_with("remove ")),
            "{case}: {:?}",
            fake.events
        );
    }
}

#[test]
fn a_supervised_unit_that_cannot_be_stopped_but_is_not_running_is_removed() {
    let record = format!("{LINUX_DATA}/service.json");
    for answer in ["inactive\n", "failed\n", "unknown\n"] {
        let mut fake = supervised_unit_fake(&record)
            .fail(SYSTEMD_STOP, 1, "Failed to stop")
            .answer(SYSTEMD_ACTIVE, 3, answer);
        let (result, out) = execute_with(&mut fake, &linux(), &["uninstall"]);
        assert_eq!(result, Ok(()), "{answer}");
        assert!(
            out.contains("the service is not running, going on"),
            "{out}"
        );
        assert!(!fake.files.contains_key(LINUX_UNIT));
        assert!(!fake.files.contains_key(&record));
    }
}

#[test]
fn a_stop_that_works_is_not_double_checked_and_a_plain_service_is_as_before() {
    let record = format!("{LINUX_DATA}/service.json");
    let mut fake = supervised_unit_fake(&record);
    let (result, _) = execute_with(&mut fake, &linux(), &["uninstall"]);
    assert_eq!(result, Ok(()));
    assert!(!fake.events.iter().any(|event| event.contains("is-active")));
    // A plain unit whose stop fails goes on without asking.
    let mut fake = Fake::default()
        .uid(1000)
        .file(LINUX_UNIT, &systemd::unit(&user_definition(), false))
        .fail(SYSTEMD_STOP, 1, "Failed to stop");
    let (result, out) = execute_with(&mut fake, &linux(), &["uninstall"]);
    assert_eq!(result, Ok(()));
    assert!(
        out.contains("failed with exit code 1: Failed to stop; going on"),
        "{out}"
    );
    assert!(!fake.events.iter().any(|event| event.contains("is-active")));
}

const LAUNCHD_BOOTOUT: &str = "launchctl bootout gui/501/io.github.loft-902-co-llc.open-ferry";
const LAUNCHD_PRINT: &str = "launchctl print gui/501/io.github.loft-902-co-llc.open-ferry";

fn supervised_agent_fake() -> Fake {
    let definition = Definition {
        data_dir: Some(MACOS_DATA.to_owned()),
        exe: "/Users/me/.local/bin/open-ferry".to_owned(),
        config: MACOS_CONFIG.to_owned(),
        dir: "/Users/me/.config/open-ferry".to_owned(),
    };
    Fake::default()
        .uid(501)
        .file(
            MACOS_AGENT,
            &launchd::plist(&definition, "/Users/me/Library/Logs/open-ferry.log", false),
        )
        .file(
            &format!("{MACOS_DATA}/service.json"),
            &record_of(
                "launchd",
                MACOS_DATA,
                true,
                &definition.exe,
                &definition.config,
            ),
        )
}

#[test]
fn a_plist_with_program_is_a_readable_definition() {
    // Not upstream's: `Program` is the executable, and `ProgramArguments` is
    // then its `argv`, whose first string is not the executable.
    let exe = "/Users/me/.local/bin/open-ferry";
    let program_only =
        format!("<plist><dict><key>Program</key><string>{exe}</string></dict></plist>");
    assert_eq!(
        launchd::program_arguments(&program_only),
        Some(vec![exe.to_owned()])
    );
    let both = format!(
        "<plist><dict><key>Program</key><string>{exe}</string><key>ProgramArguments</key><array><string>argv0</string><string>-supervise</string><string>-config</string><string>{MACOS_CONFIG}</string><string>-data-dir</string><string>{MACOS_DATA}</string></array></dict></plist>"
    );
    let words = launchd::program_arguments(&both).unwrap();
    assert_eq!(
        flagged(&words, words.first().cloned()),
        Flagged {
            supervised: true,
            data_dir: Some(MACOS_DATA.to_owned()),
            exe: Some(exe.to_owned()),
            config: Some(MACOS_CONFIG.to_owned()),
        }
    );
    // Neither key: nothing to tell.
    assert_eq!(launchd::program_arguments("<plist><dict/></plist>"), None);

    let record_path = format!("{MACOS_DATA}/service.json");
    // `Program` alone: no config is named, so the stale self-naming record is
    // left alone, and nothing is taken for supervised.
    let mut fake = supervised_agent_fake().file(MACOS_AGENT, &program_only);
    let (result, out) = execute_with(&mut fake, &macos(), &["uninstall"]);
    assert_eq!(result, Ok(()), "{out}");
    assert!(
        out.contains(&format!(
            "Leaving {record_path} alone and not using it: the installed service's definition doesn't name its config."
        )),
        "{out}"
    );
    assert!(fake.files.contains_key(&record_path));
    assert!(!fake.events.contains(&format!("remove {record_path}")));
    assert!(!fake.files.contains_key(MACOS_AGENT));

    // Not taken for supervised: a failed bootout is not checked, and the
    // plist goes.
    let mut fake = supervised_agent_fake()
        .file(MACOS_AGENT, &program_only)
        .fail(LAUNCHD_BOOTOUT, 5, "Boot-out failed");
    let (result, out) = execute_with(&mut fake, &macos(), &["uninstall"]);
    assert_eq!(result, Ok(()), "{out}");
    assert!(
        !fake
            .events
            .iter()
            .any(|event| event.contains("launchctl print")),
        "{:?}",
        fake.events
    );
    assert!(
        !fake
            .events
            .iter()
            .any(|event| event.starts_with("stop_supervisor"))
    );
    assert!(!fake.files.contains_key(MACOS_AGENT));
    assert!(fake.files.contains_key(&record_path));

    // `Program` with `ProgramArguments`: the executable compared is
    // `Program`, so the matching record is the service's and goes.
    let mut fake = supervised_agent_fake().file(MACOS_AGENT, &both);
    let (result, out) = execute_with(&mut fake, &macos(), &["uninstall"]);
    assert_eq!(result, Ok(()), "{out}");
    assert!(!out.contains("Leaving"), "{out}");
    assert!(!fake.files.contains_key(&record_path));
}

/// A plist with `body` as its job dict's keys.
fn plist_of(body: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n{body}\n</dict>\n</plist>\n"
    )
}

fn words(items: &[&str]) -> Option<Vec<String>> {
    Some(items.iter().map(|item| (*item).to_owned()).collect())
}

#[test]
fn a_plist_is_read_by_its_structure() {
    // Not upstream's: comments, nested dicts and other keys' values do not
    // change what is read.
    // A comment between a key and its value, before the root, between keys
    // and between array elements.
    let plist = format!(
        "<!-- before -->{}",
        plist_of(
            "<!-- a --><key>Program</key><!-- executable --><string>/new/open-ferry</string>\n<!-- b -->\n<key>ProgramArguments</key><!-- c --><array><!-- d --><string>argv0</string><!-- e --><string>-supervise</string></array>"
        )
    );
    assert_eq!(
        launchd::program_arguments(&plist),
        words(&["/new/open-ferry", "-supervise"])
    );

    // A `Program` inside another key's dict is not the job's.
    let plist = plist_of(
        "<key>EnvironmentVariables</key><dict><key>Program</key><string>/old/open-ferry</string><key>X</key><array><string>y</string></array></dict>\n<key>ProgramArguments</key><array><string>/new/open-ferry</string><string>-supervise</string></array>",
    );
    assert_eq!(
        launchd::program_arguments(&plist),
        words(&["/new/open-ferry", "-supervise"])
    );
    // So a record naming the nested path is not the installed service's.
    let record_path = format!("{MACOS_DATA}/service.json");
    let stale = record_of("launchd", MACOS_DATA, true, "/old/open-ferry", MACOS_CONFIG);
    let plist = plist_of(&format!(
        "<key>EnvironmentVariables</key><dict><key>Program</key><string>/old/open-ferry</string></dict>\n<key>ProgramArguments</key><array><string>/new/open-ferry</string><string>-supervise</string><string>-config</string><string>{MACOS_CONFIG}</string><string>-data-dir</string><string>{MACOS_DATA}</string></array>"
    ));
    let mut fake = supervised_agent_fake()
        .file(MACOS_AGENT, &plist)
        .file(&record_path, &stale);
    let (result, out) = execute_with(&mut fake, &macos(), &["uninstall"]);
    assert_eq!(result, Ok(()), "{out}");
    assert!(
        out.contains(
            "it names /old/open-ferry as the binary, but the installed service runs /new/open-ferry"
        ),
        "{out}"
    );
    assert!(fake.files.contains_key(&record_path));

    // An empty `ProgramArguments` is empty: the next array is not it.
    let plist = plist_of(
        "<key>Program</key><string>/bin/of</string>\n<key>ProgramArguments</key><array/>\n<key>WatchPaths</key><array><string>/tmp/a</string><string>/tmp/b</string></array>",
    );
    assert_eq!(launchd::program_arguments(&plist), words(&["/bin/of"]));
    // Other values of every type are skipped by depth.
    let plist = plist_of(
        "<key>RunAtLoad</key><true/><key>Nice</key><integer>5</integer><key>X</key><real>1.5</real><key>D</key><date>2026-01-01T00:00:00Z</date><key>B</key><data>AAAA</data><key>E</key><dict/><key>F</key><false/><key>S</key><string/><key>A</key><array><dict><key>k</key><array/></dict></array><key>ProgramArguments</key><array><string>/bin/of</string><string/></array>",
    );
    assert_eq!(launchd::program_arguments(&plist), words(&["/bin/of", ""]));

    // Unreadable: a value of another type, a string with markup in it, a
    // key given twice, a document that is cut off or not a plist, and a
    // plist that says neither.
    for body in [
        "<key>ProgramArguments</key><array><string>/bin/of</string><integer>1</integer></array>",
        "<key>ProgramArguments</key><array><string>/bin/of</string><array/></array>",
        "<key>ProgramArguments</key><array><string>/bin/<!-- x -->of</string></array>",
        "<key>ProgramArguments</key><array><string><![CDATA[/bin/of]]></string></array>",
        "<key>ProgramArguments</key><array><string><b>x</b></string></array>",
        "<key>ProgramArguments</key><string>/bin/of</string>",
        "<key>Program</key><array><string>/bin/of</string></array>",
        "<key>Program</key><integer>1</integer>",
        "<key>Program</key><string>/a</string><key>Program</key><string>/b</string>",
        "<key>ProgramArguments</key><array/><key>ProgramArguments</key><array/>",
        "<key>Program</key>",
        "<key>Program</key><string>/bin/of",
        "<key>Program</key><string>/bin/of</strong>",
        "stray<key>Program</key><string>/bin/of</string>",
        "<key>Label</key><string>x</string>",
    ] {
        assert_eq!(launchd::program_arguments(&plist_of(body)), None, "{body}");
    }
    for text in [
        "",
        "not a plist",
        "<dict><key>Program</key><string>/bin/of</string></dict>",
        "<plist><dict><key>Program</key><string>/bin/of</string>",
        "<plist><dict><key>Program</key><string>/bin/of</string></dict>",
        "<plist/>",
        "<plist><dict><key>Program",
        "<plist><!-- never closed",
    ] {
        assert_eq!(launchd::program_arguments(text), None, "{text}");
    }
    // A deep value costs a stack of tags, not the call stack.
    let deep = format!(
        "<key>E</key>{}{}<key>Program</key><string>/bin/of</string>",
        "<dict><key>k</key>".repeat(100_000),
        "</dict>".repeat(100_000)
    );
    assert_eq!(
        launchd::program_arguments(&plist_of(&deep)),
        words(&["/bin/of"])
    );
}

#[test]
fn a_supervised_agent_is_kept_unless_launchd_says_it_is_not_loaded() {
    // Not upstream's: the same rule as the unit's.
    let mut fake = supervised_agent_fake()
        .fail(LAUNCHD_BOOTOUT, 5, "Boot-out failed")
        .answer(LAUNCHD_PRINT, 0, "state = running\n");
    let (result, _) = execute_with(&mut fake, &macos(), &["uninstall"]);
    assert!(result.unwrap_err().contains("may still be running"));
    assert!(fake.files.contains_key(MACOS_AGENT));
    assert!(
        fake.files
            .contains_key(&format!("{MACOS_DATA}/service.json"))
    );
    assert!(!fake.events.iter().any(|event| event.starts_with("remove ")));

    let mut fake = supervised_agent_fake()
        .fail(LAUNCHD_BOOTOUT, 5, "Boot-out failed")
        .fail(LAUNCHD_PRINT, 113, "Could not find service");
    let (result, out) = execute_with(&mut fake, &macos(), &["uninstall"]);
    assert_eq!(result, Ok(()));
    assert!(
        out.contains("the service is not running, going on"),
        "{out}"
    );
    assert!(!fake.files.contains_key(MACOS_AGENT));

    let mut fake = supervised_agent_fake();
    let (result, _) = execute_with(&mut fake, &macos(), &["uninstall"]);
    assert_eq!(result, Ok(()));
    assert!(
        !fake
            .events
            .iter()
            .any(|event| event.contains("launchctl print"))
    );
}

const UNREADABLE: &str =
    "the installed service's definition can't be read, so the record can't be checked against it";

#[test]
fn a_plist_that_is_there_and_cannot_be_read_rejects_the_record_and_keeps_the_stop_check() {
    // Not upstream's: a stale `supervised: false` record in the environment's
    // directory must not turn the stop check off when the plist can't say
    // what the job runs.
    let record_path = format!("{MACOS_DATA}/service.json");
    let stale = record_of(
        "launchd",
        MACOS_DATA,
        false,
        "/Users/me/.local/bin/open-ferry",
        MACOS_CONFIG,
    );
    for plist in [
        "bplist00\u{1}\u{0}\u{8}",
        "<plist><dict><key>Program</key><string>/bin/of",
        "<plist><dict><key>Label</key><string>x</string></dict></plist>",
    ] {
        // The job stays loaded: nothing is removed.
        let mut fake = supervised_agent_fake()
            .file(MACOS_AGENT, plist)
            .file(&record_path, &stale)
            .fail(LAUNCHD_BOOTOUT, 5, "Boot-out failed")
            .answer(LAUNCHD_PRINT, 0, "state = running\n");
        let (result, out) = execute_with(&mut fake, &macos(), &["uninstall"]);
        let error = result.unwrap_err();
        assert!(error.contains("may still be running"), "{plist}: {error}");
        assert!(out.contains(UNREADABLE), "{plist}: {out}");
        assert!(fake.files.contains_key(MACOS_AGENT), "{plist}");
        assert!(fake.files.contains_key(&record_path), "{plist}");
        assert!(
            !fake.events.iter().any(|event| event.starts_with("remove ")),
            "{plist}: {:?}",
            fake.events
        );

        // The job is gone: the plist goes, the record it can't vouch for stays.
        let mut fake = supervised_agent_fake()
            .file(MACOS_AGENT, plist)
            .file(&record_path, &stale)
            .fail(LAUNCHD_BOOTOUT, 5, "Boot-out failed")
            .fail(LAUNCHD_PRINT, 113, "Could not find service");
        let (result, out) = execute_with(&mut fake, &macos(), &["uninstall"]);
        assert_eq!(result, Ok(()), "{plist}: {out}");
        assert!(out.contains(UNREADABLE), "{plist}: {out}");
        assert!(!fake.files.contains_key(MACOS_AGENT), "{plist}");
        assert!(fake.files.contains_key(&record_path), "{plist}");
    }
}

#[test]
fn a_unit_without_a_command_rejects_the_record_and_keeps_the_stop_check() {
    let record = format!("{LINUX_DATA}/service.json");
    let stale = unit_record(LINUX_DATA, false);
    let unit = "[Unit]\nDescription=open-ferry\n[Service]\nRestart=always\n";
    let mut fake = supervised_unit_fake(&record)
        .file(LINUX_UNIT, unit)
        .file(&record, &stale)
        .fail(SYSTEMD_STOP, 1, "Failed to stop")
        .answer(SYSTEMD_ACTIVE, 0, "active\n");
    let (result, out) = execute_with(&mut fake, &linux(), &["uninstall"]);
    assert!(result.unwrap_err().contains("may still be running"));
    assert!(out.contains(UNREADABLE), "{out}");
    assert!(fake.files.contains_key(LINUX_UNIT));
    assert!(fake.files.contains_key(&record));
    assert!(!fake.events.iter().any(|event| event.starts_with("remove ")));
}

#[test]
fn a_unit_is_read_only_when_it_is_plainly_ours() {
    // Not upstream's: anything systemd would read differently from us is
    // unreadable, so the stop check stays on.
    let unit = |exec: &str| {
        format!(
            "[Unit]\nDescription=x\n[Service]\nType=simple\n{exec}\n[Install]\nWantedBy=default.target\n"
        )
    };
    let ok = |exec: &str| systemd::exec_words(&unit(exec));
    // What the installer writes, and a plain hand edit: comments, blank
    // lines, another section's ExecStart, spaces around `=`.
    assert_eq!(
        ok("ExecStart=\"/bin/of\" -config \"/a b/c.yaml\""),
        words(&["/bin/of", "-config", "/a b/c.yaml"])
    );
    assert_eq!(
        ok("# a comment\n; another\n\nExecStart = /bin/of \"100%%\" \"$$x\" \"q\\\"\\\\\""),
        words(&["/bin/of", "100%", "$x", "q\"\\"])
    );
    // Unicode whitespace inside quotes is part of the word; tabs and CRLF
    // are separators and line ends.
    assert_eq!(
        systemd::exec_words("[Service]\r\nExecStart=\t/bin/of\t\"/tmp/my\u{a0}config.yaml\"\r\n"),
        words(&["/bin/of", "/tmp/my\u{a0}config.yaml"])
    );
    assert_eq!(
        systemd::exec_words("[Unit]\nExecStart=/other\n[Service]\nExecStart=/bin/of\n"),
        words(&["/bin/of"])
    );
    for exec in [
        // The reviewer's unterminated quote, and a quote cut by a backslash.
        "ExecStart=/bin/of -config \"/tmp/config.yaml",
        "ExecStart=/bin/of \"/tmp/x\\",
        // None, two, a reset, and an empty one.
        "Restart=always",
        "ExecStart=/bin/of\nExecStart=/bin/of2",
        "ExecStart=\nExecStart=/bin/of",
        "ExecStart=",
        "ExecStart=   ",
        // A continuation line.
        "ExecStart=/bin/of \\\n-config x",
        "ExecStart=/bin/of\nEnvironment=A=b \\\nc",
        // Prefixes.
        "ExecStart=-/bin/of",
        "ExecStart=@/bin/of x",
        "ExecStart=:/bin/of",
        "ExecStart=+/bin/of",
        "ExecStart=!/bin/of",
        // A prefix on the decoded executable, quoted.
        "ExecStart=\"-/bin/of\" -config /c",
        "ExecStart=\"@/bin/of\"",
        "ExecStart=\"+\"/bin/of",
        // Unicode whitespace outside quotes is not systemd's separator.
        "ExecStart=/bin/of -config /tmp/my\u{a0}config.yaml",
        "ExecStart=/bin/of\u{2003}-config",
        // Quotes and escapes.
        "ExecStart=/bin/of 'a b'",
        "ExecStart=/bin/of \"a\\nb\"",
        "ExecStart=/bin/of a\\ b",
        // A single `%` or `$`.
        "ExecStart=/bin/of %h",
        "ExecStart=/bin/of \"$HOME\"",
        "ExecStart=/bin/of 100%",
    ] {
        assert_eq!(ok(exec), None, "{exec}");
    }
    // The ExecStart must be in `[Service]`.
    assert_eq!(
        systemd::exec_words("[Unit]\nExecStart=/bin/of\n[Install]\nExecStart=/x\n"),
        None
    );
    assert_eq!(systemd::exec_words("ExecStart=/bin/of\n"), None);
}

#[test]
fn a_unit_that_is_unreadable_to_us_keeps_the_stop_check_and_the_record() {
    let record = format!("{LINUX_DATA}/service.json");
    let stale = unit_record(LINUX_DATA, false);
    let cut = "[Service]\nExecStart=/bin/of -config \"/tmp/config.yaml\n";
    let drop_in = format!("{LINUX_UNIT}.d");
    let cases: [(Fake, &str); 2] = [
        (supervised_unit_fake(&record).file(LINUX_UNIT, cut), "cut"),
        // A drop-in directory beside a unit we could read.
        (supervised_unit_fake(&record).dir(&drop_in), "drop-in"),
    ];
    for (fake, case) in cases {
        let mut fake = fake
            .file(&record, &stale)
            .fail(SYSTEMD_STOP, 1, "Failed to stop")
            .answer(SYSTEMD_ACTIVE, 0, "active\n");
        let (result, out) = execute_with(&mut fake, &linux(), &["uninstall"]);
        assert!(
            result.unwrap_err().contains("may still be running"),
            "{case}"
        );
        assert!(out.contains(UNREADABLE), "{case}: {out}");
        assert!(fake.files.contains_key(LINUX_UNIT), "{case}");
        assert!(fake.files.contains_key(&record), "{case}");
        assert!(
            !fake.events.iter().any(|event| event.starts_with("remove ")),
            "{case}: {:?}",
            fake.events
        );
    }
}

#[test]
fn a_plist_reader_takes_what_xml_allows_in_tags_doctypes_and_the_start() {
    let job = "<dict><key>ProgramArguments</key><array><string>/bin/of</string></array></dict>";
    let wanted = words(&["/bin/of"]);
    for text in [
        // A `>` inside a quoted attribute value.
        format!("<plist version=\"1.0\" note=\">\">{job}</plist>"),
        format!("<plist version='1.0' note='>'>{job}</plist>"),
        // Quoted `>` and an internal subset in the DOCTYPE.
        format!("<!DOCTYPE plist SYSTEM \"a>b\"><plist>{job}</plist>"),
        format!("<!DOCTYPE plist [<!ENTITY x \"]>\">]><plist>{job}</plist>"),
        format!("<!DOCTYPE plist [<!-- ]> --><!ENTITY x 'y'>] ><plist>{job}</plist>"),
        // One byte order mark, whitespace and comments after the root.
        format!("\u{FEFF}<plist>{job}</plist>"),
        format!("<plist>{job}</plist>\n<!-- done --><?pi x?> \n"),
        // Dashes in a comment that are not `--`.
        format!("<!-- a-b - c --><plist>{job}</plist>"),
    ] {
        assert_eq!(launchd::program_arguments(&text), wanted, "{text}");
    }
    for text in [
        format!("<plist>{job}</plist><"),
        format!("<plist>{job}</plist>text"),
        format!("<plist>{job}</plist><plist>{job}</plist>"),
        format!("<plist>{job}</plist><![CDATA[x]]>"),
        format!("\u{FEFF}\u{FEFF}<plist>{job}</plist>"),
        format!("<!-- bad -- comment --><plist>{job}</plist>"),
        format!("<!-- bad ---><plist>{job}</plist>"),
        format!("<plist>{job}</plist><!-- a -- b -->"),
        format!("<!DOCTYPE plist [<!ENTITY x \"]>\"><plist>{job}</plist>"),
        "<plist note=\"><dict/></plist>".to_owned(),
        // A declaration after the root, and `--` in a subset's comment.
        format!("<plist>{job}</plist><!DOCTYPE plist>"),
        format!("<!DOCTYPE plist [<!-- a -- b -->]><plist>{job}</plist>"),
        format!("<!DOCTYPE plist [<!-- a --->]><plist>{job}</plist>"),
    ] {
        assert_eq!(launchd::program_arguments(&text), None, "{text}");
    }
    // A dict of many distinct keys is read in a blink, not squared.
    let many: String = (0..100_000)
        .map(|n| format!("<key>K{n}</key><true/>"))
        .collect();
    let plist = plist_of(&format!(
        "{many}<key>ProgramArguments</key><array><string>/bin/of</string></array>"
    ));
    assert_eq!(launchd::program_arguments(&plist), wanted);
    // Any direct key twice is unreadable, compared once decoded.
    for body in [
        "<key>Label</key><string>a</string><key>Label</key><string>b</string>",
        "<key>Nice</key><integer>1</integer><key>N&#105;ce</key><integer>2</integer>",
        "<key>RunAtLoad</key><true/><key>RunAtLoad</key><false/>",
    ] {
        let plist = plist_of(&format!(
            "{body}<key>ProgramArguments</key><array><string>/bin/of</string></array>"
        ));
        assert_eq!(launchd::program_arguments(&plist), None, "{body}");
    }
}

#[cfg(unix)]
#[test]
fn a_backslash_in_a_unix_name_is_kept_in_the_data_directory() {
    // Not upstream's: only a Unix context built on Windows has its
    // separators rewritten.
    let home = context(
        Platform::Linux,
        "/home/a\\b/open-ferry",
        "/home/a\\b",
        &[("HOME", "/home/a\\b")],
    );
    assert_eq!(
        data_dir(&home).unwrap(),
        "/home/a\\b/.local/share/open-ferry"
    );
    let xdg = context(
        Platform::Linux,
        "/home/me/open-ferry",
        "/home/me",
        &[("HOME", "/home/me"), ("XDG_DATA_HOME", "/data/x\\y")],
    );
    assert_eq!(data_dir(&xdg).unwrap(), "/data/x\\y/open-ferry");
}

#[test]
fn refuses_a_data_directory_a_definition_cannot_hold() {
    // Not upstream's: the pinned data directory is checked as the other
    // paths are, so it can't split a unit, a plist or a task.
    let cases: [(Fake, Context, &str); 3] = [
        (
            Fake::default().uid(1000).file(LINUX_CONFIG, CONFIG),
            linux(),
            "/home/me/.local/share/open\nferry",
        ),
        (
            Fake::default().uid(501).file(MACOS_CONFIG, CONFIG),
            macos(),
            "/Users/me/.local/share/open\nferry",
        ),
        (
            windows_user(),
            windows_with_data(),
            "C:\\Users\\me\\AppData\\Local\\open\nferry",
        ),
    ];
    for (mut fake, context, data) in cases {
        let updating = Updating::supervised(data);
        let (result, _) = execute_updating(&mut fake, &context, &["install"], &updating);
        let error = result.unwrap_err();
        assert!(
            error.starts_with("The data directory's path, ") && error.contains("control character"),
            "{error}"
        );
        assert!(
            !fake.events.iter().any(|event| {
                event.starts_with("write")
                    || event.starts_with("mkdir")
                    || event.contains("enable")
                    || event.contains("/create")
                    || event.contains("bootstrap")
            }),
            "{:?}",
            fake.events
        );
    }
}

#[test]
fn parses_the_supervised_run() {
    // Not upstream's: `service run -supervise` needs the data directory the
    // definition pins, and `-data-dir` goes with nothing else. The task and
    // the Windows service (`-system`) each have a supervised form.
    let data = std::env::temp_dir().to_string_lossy().into_owned();
    let config = std::env::temp_dir()
        .join("c.yaml")
        .to_string_lossy()
        .into_owned();
    let run = parse(args(&[
        "run",
        "-supervise",
        "-config",
        &config,
        "-data-dir",
        &data,
    ]))
    .unwrap();
    assert!(run.supervise);
    assert_eq!(run.data_dir.as_deref(), Some(data.as_str()));
    let system = parse(args(&[
        "run",
        "-system",
        "-supervise",
        "-config",
        &config,
        "-data-dir",
        &data,
    ]))
    .unwrap();
    assert!(system.system && system.supervise);
    assert_eq!(system.data_dir.as_deref(), Some(data.as_str()));
    let cases: [(Vec<&str>, &str); 4] = [
        (vec!["run", "-supervise", "-config", &config], "-data-dir"),
        (
            vec!["run", "-config", &config, "-data-dir", &data],
            "-supervise",
        ),
        (
            vec!["run", "-supervise", "-system", "-config", &config],
            "-data-dir",
        ),
        (
            vec![
                "run",
                "-supervise",
                "-config",
                &config,
                "-data-dir",
                "relative",
            ],
            "absolute path",
        ),
    ];
    for (line, why) in cases {
        let error = parse(args(&line));
        assert!(
            matches!(&error, Err(FlagError::Invalid(message)) if message.contains(why)),
            "{line:?}: {error:?}"
        );
    }
    assert!(parse(args(&["install", "-supervise"])).is_err());
}

#[test]
fn the_host_stops_a_system_supervisor_only_through_a_trusted_directory() {
    // A directory another account can change: the test's own on Windows,
    // and one anybody may write to on Unix.
    let dir = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    {
        let output = std::process::Command::new(helper_path("icacls.exe").unwrap())
            .arg(dir.path())
            .args(["/grant", "*S-1-1-0:(W)"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "failed to grant scratch write access"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    }
    let path = dir.path().to_str().unwrap();
    let reason = Host.trusted_dir(path).unwrap_err();
    assert_eq!(
        Host.stop_supervisor(path, Duration::from_secs(1), ServiceKind::System),
        StopOutcome::Refused(reason)
    );
}

const MACHINE_DATA: &str = "/var/lib/open-ferry";
const SYSTEM_UNIT: &str = "/etc/systemd/system/open-ferry.service";
const MACHINE_CONFIG: &str = "/etc/open-ferry/config.yaml";
const MACHINE_EXE: &str = "/usr/local/bin/open-ferry";
const SCM_DATA: &str = r"C:\ProgramData\open-ferry";

fn machine_definition(supervised: bool) -> Definition {
    Definition {
        exe: MACHINE_EXE.to_owned(),
        config: MACHINE_CONFIG.to_owned(),
        dir: "/etc/open-ferry".to_owned(),
        data_dir: supervised.then(|| MACHINE_DATA.to_owned()),
    }
}

fn machine_context() -> Context {
    context(Platform::Linux, MACHINE_EXE, "/root", &[("HOME", "/root")])
}

#[test]
fn a_system_service_defaults_to_the_machine_config() {
    let mut fake = Fake::default().uid(0).file(MACHINE_CONFIG, SYSTEM_CONFIG);
    assert_eq!(
        execute_with(&mut fake, &machine_context(), &["install", "-system"]).0,
        Ok(())
    );
    assert!(text(&fake, SYSTEM_UNIT).contains("-config \"/etc/open-ferry/config.yaml\""));

    let (fake, context) = windows_admin(PROGRAM_FILES_EXE);
    let config = r"C:\ProgramData\open-ferry\config.yaml";
    let mut fake = fake.file(config, r"auth-dir: 'C:\ProgramData\open-ferry\auth'");
    assert_eq!(
        execute_with(&mut fake, &context, &["install", "-system"]).0,
        Ok(())
    );
    assert!(
        fake.events
            .iter()
            .any(|event| event.starts_with("run sc.exe create")
                && event.contains(r"C:\ProgramData\open-ferry\config.yaml"))
    );
}

fn scm_definition(supervised: bool) -> Definition {
    Definition {
        exe: PROGRAM_FILES_EXE.to_owned(),
        config: PROGRAM_FILES_CONFIG.to_owned(),
        dir: r"C:\Program Files\open-ferry".to_owned(),
        data_dir: supervised.then(|| SCM_DATA.to_owned()),
    }
}

#[test]
fn writes_a_supervised_systemd_system_unit() {
    assert_eq!(systemd::unit(&machine_definition(true), true),
        "# Installed by `open-ferry service install -system`; `open-ferry service uninstall -system` removes it.
[Unit]
Description=open-ferry AI proxy
Documentation=https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
ExecStart=\"/usr/local/bin/open-ferry\" -supervise -system -config \"/etc/open-ferry/config.yaml\" -data-dir \"/var/lib/open-ferry\"
WorkingDirectory=/etc/open-ferry
Restart=on-failure
RestartSec=5
KillMode=mixed
TimeoutStopSec=60

[Install]
WantedBy=multi-user.target
");
}

#[test]
fn writes_a_supervised_scm_service() {
    let definition = scm_definition(true);
    assert_eq!(
        windows::service_command(&definition),
        r#""C:\Program Files\open-ferry\open-ferry.exe" service run -system -supervise -config "C:\Program Files\open-ferry\config.yaml" -data-dir "C:\ProgramData\open-ferry""#
    );
    let commands: Vec<String> = windows::service_install_steps(&definition)
        .iter()
        .map(|step| {
            let Step::Run(cmd) = step else {
                panic!("expected a command")
            };
            cmd.to_string()
        })
        .collect();
    assert_eq!(
        commands,
        [
            r#"sc.exe create open-ferry binPath= "\"C:\Program Files\open-ferry\open-ferry.exe\" service run -system -supervise -config \"C:\Program Files\open-ferry\config.yaml\" -data-dir \"C:\ProgramData\open-ferry\"" start= auto DisplayName= open-ferry"#,
            r#"sc.exe description open-ferry "open-ferry AI proxy""#,
            "sc.exe failure open-ferry reset= 86400 actions= restart/5000/restart/60000/restart/60000",
            "sc.exe failureflag open-ferry 1",
            "sc.exe start open-ferry",
        ]
    );
    // No obj= means SCM's default LocalSystem account.
}

#[test]
fn writes_a_plain_launchd_daemon_plist() {
    assert_eq!(
        launchd::plist(
            &machine_definition(false),
            "/Library/Logs/open-ferry.log",
            true
        ),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Installed by `open-ferry service install -system`; `open-ferry service uninstall -system` removes it. -->
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>io.github.loft-902-co-llc.open-ferry</string>
	<key>ProgramArguments</key>
	<array>
		<string>/usr/local/bin/open-ferry</string>
		<string>-config</string>
		<string>/etc/open-ferry/config.yaml</string>
	</array>
	<key>WorkingDirectory</key>
	<string>/etc/open-ferry</string>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>StandardOutPath</key>
	<string>/Library/Logs/open-ferry.log</string>
	<key>StandardErrorPath</key>
	<string>/Library/Logs/open-ferry.log</string>
</dict>
</plist>
"#
    );
}

#[test]
fn every_system_form_requires_real_trust_of_the_exe_config_and_working_directory() {
    for supervised in [false, true] {
        for platform in [Platform::Linux, Platform::Windows] {
            let (base, context, config, dir) = if platform == Platform::Windows {
                let (fake, context) = windows_admin(PROGRAM_FILES_EXE);
                (
                    fake,
                    context,
                    PROGRAM_FILES_CONFIG,
                    r"C:\Program Files\open-ferry\work",
                )
            } else {
                (
                    Fake::default().uid(0).file(MACHINE_CONFIG, SYSTEM_CONFIG),
                    machine_context(),
                    MACHINE_CONFIG,
                    "/srv/work",
                )
            };
            for path in [&context.exe, config, dir] {
                let mut fake = Fake {
                    untrusted: BTreeSet::from([path.to_owned()]),
                    ..Fake::default()
                };
                fake.files = base.files.clone();
                fake.answers = base.answers.clone();
                fake.privileged = base.privileged;
                let updating = if supervised {
                    Updating::supervised(MACHINE_DATA)
                } else {
                    Updating::plain(Ok(MACHINE_DATA.to_owned()), "no machine install")
                };
                let error = prepare_install(
                    &mut fake,
                    &context,
                    Target::new(platform, true),
                    config,
                    Some(dir),
                    false,
                    &updating,
                )
                .err()
                .unwrap();
                assert_eq!(
                    error,
                    format!(
                        "Refusing the system service: {path} failed the system trust check: {path} is not safe: {path} is untrusted"
                    )
                );
                assert!(!fake.events.iter().any(|event| event.starts_with("write ")
                    || event.starts_with("write_atomic ")
                    || event.contains("create open-ferry")));
            }
        }
    }
}

#[test]
fn real_trust_refuses_the_sticky_group_writable_and_link_paths_protected_accepted() {
    for (path, owner) in [("/srv", 0o1777), ("/usr/local/bin", 0o775)] {
        let exe = format!("{path}/open-ferry");
        let mut fake = Fake::default()
            .uid(0)
            .file(MACHINE_CONFIG, SYSTEM_CONFIG)
            .owner(path, 0, 0, owner, true);
        let context = context(Platform::Linux, &exe, "/root", &[("HOME", "/root")]);
        assert!(protected(&mut fake, &context, MACHINE_CONFIG, None).is_ok());
        assert!(
            system_install(&mut fake, &exe)
                .unwrap_err()
                .contains("failed the system trust check")
        );
    }
    let mut fake = Fake::default()
        .uid(0)
        .file(MACHINE_CONFIG, SYSTEM_CONFIG)
        .link(MACHINE_EXE, "/opt/open-ferry");
    assert!(protected(&mut fake, &machine_context(), MACHINE_CONFIG, None).is_ok());
    assert!(
        system_install(&mut fake, MACHINE_EXE)
            .unwrap_err()
            .contains("is a link")
    );
}

struct ScopeFake {
    platform: Platform,
    receipt_binary: Option<String>,
    refused: Option<String>,
    bad_child: bool,
}

impl ScopeFake {
    fn root(&self) -> &'static str {
        if self.platform == Platform::Windows {
            SCM_DATA
        } else {
            MACHINE_DATA
        }
    }
}

impl Default for ScopeFake {
    fn default() -> Self {
        Self {
            platform: Platform::Linux,
            receipt_binary: Some(MACHINE_EXE.to_owned()),
            refused: None,
            bad_child: false,
        }
    }
}

impl scope::Probe for ScopeFake {
    fn platform(&self) -> Platform {
        self.platform
    }
    fn privileged(&self) -> Result<bool, String> {
        Ok(true)
    }
    fn account_dir(&self) -> Result<String, String> {
        Ok(if self.platform == Platform::Windows {
            r"C:\Users\root\AppData\Local\open-ferry"
        } else {
            "/root/.local/share/open-ferry"
        }
        .to_owned())
    }
    fn program_data(&self) -> Result<String, String> {
        Ok(r"C:\ProgramData".to_owned())
    }
    fn state(&self, path: &str) -> scope::State {
        if path.ends_with(open_ferry_update::data_dir::RECEIPT_FILE) {
            if path.starts_with(self.root()) && self.receipt_binary.is_some() {
                scope::State::File
            } else {
                scope::State::Missing
            }
        } else if path.ends_with("untrusted-child") {
            scope::State::Other
        } else {
            scope::State::Directory
        }
    }
    fn entries(&self, dir: &str, _limit: usize) -> io::Result<Vec<String>> {
        if dir == self.root() && self.bad_child {
            Ok(vec!["untrusted-child".to_owned()])
        } else {
            Ok(Vec::new())
        }
    }
    fn trusted_dir(&self, path: &str) -> Result<(), String> {
        match &self.refused {
            Some(why) if path == self.root() => Err(why.clone()),
            _ => Ok(()),
        }
    }
    fn trusted_file(&self, _path: &str) -> Result<(), String> {
        Ok(())
    }
    fn readable_file(&self, _path: &str) -> io::Result<()> {
        Ok(())
    }

    fn receipt(
        &self,
        path: &str,
    ) -> Result<Option<Receipt>, open_ferry_update::receipt::ReceiptError> {
        Ok(self
            .receipt_binary
            .as_ref()
            .filter(|_| path.starts_with(self.root()))
            .map(|binary| Receipt {
                binary: binary.clone(),
                ..Receipt::default()
            }))
    }
    fn canonical(&self, path: &str) -> io::Result<String> {
        Ok(path.to_owned())
    }
    fn create_machine_dir(&mut self, _path: &str) -> io::Result<()> {
        panic!("assessment never provisions")
    }
}

#[derive(Default)]
struct InstallFake {
    container: bool,
    read_only: bool,
}

impl update_install::System for InstallFake {
    fn current_exe(&self) -> io::Result<std::path::PathBuf> {
        panic!("assessment takes the service binary")
    }
    fn canonicalize(&self, path: &Path) -> io::Result<std::path::PathBuf> {
        // Forward slashes let assessment read a Windows binary's name on
        // Unix too; these fixtures contain no Unix backslashes.
        Ok(path.to_string_lossy().replace('\\', "/").into())
    }
    fn in_container(&self) -> bool {
        self.container
    }
    fn dir_writable(&self, _dir: &Path) -> bool {
        !self.read_only
    }
}

#[test]
fn a_system_service_chooses_only_a_trusted_self_updating_machine_install() {
    let context = machine_context();
    let machine = InstallFake::default();
    let updating = Updating::assess_system(&ScopeFake::default(), &machine, &context);
    assert_eq!(updating, Updating::supervised(MACHINE_DATA));
    for receipt_binary in [None, Some("/another/open-ferry".to_owned())] {
        let probe = ScopeFake {
            receipt_binary,
            ..ScopeFake::default()
        };
        assert_eq!(
            Updating::assess_system(&probe, &machine, &context)
                .why_plain
                .as_deref(),
            Some(NO_MACHINE_INSTALL)
        );
    }
    let probe = ScopeFake {
        refused: Some("the machine directory is unsafe".to_owned()),
        ..ScopeFake::default()
    };
    let refusal = scope::lookup(&probe, &context.exe).unwrap_err().to_string();
    assert_eq!(
        Updating::assess_system(&probe, &machine, &context).why_plain,
        Some(refusal)
    );
    let probe = ScopeFake {
        bad_child: true,
        ..ScopeFake::default()
    };
    assert!(
        Updating::assess_system(&probe, &machine, &context)
            .why_plain
            .unwrap()
            .contains("untrusted-child")
    );
    for machine in [
        InstallFake {
            container: true,
            read_only: false,
        },
        InstallFake {
            container: false,
            read_only: true,
        },
    ] {
        let install = update_install::assess_for(
            &machine,
            Ok(Some(Receipt {
                binary: MACHINE_EXE.to_owned(),
                ..Receipt::default()
            })),
            Path::new(MACHINE_EXE),
        );
        assert_eq!(
            Updating::assess_system(&ScopeFake::default(), &machine, &context).why_plain,
            Some(install.why_not().unwrap().to_string())
        );
    }
    let probe = ScopeFake {
        platform: Platform::MacOs,
        ..ScopeFake::default()
    };
    let mut context = context;
    context.platform = Platform::MacOs;
    assert_eq!(
        Updating::assess_system(&probe, &machine, &context).why_plain,
        Some(macos_system_refusal())
    );
}

#[test]
fn a_windows_system_service_pins_the_known_machine_directory() {
    let (_, mut context) = windows_admin(PROGRAM_FILES_EXE);
    // ProgramData from the environment is deliberately not an input.
    context
        .env
        .insert("ProgramData".to_owned(), r"D:\untrusted".to_owned());
    let probe = ScopeFake {
        platform: Platform::Windows,
        receipt_binary: Some(PROGRAM_FILES_EXE.to_owned()),
        ..ScopeFake::default()
    };
    assert_eq!(
        Updating::assess_system(&probe, &InstallFake::default(), &context),
        Updating::supervised(SCM_DATA)
    );
}

#[test]
fn system_records_are_atomic_after_install_and_only_for_the_supervised_form() {
    for supervised in [false, true] {
        for platform in [Platform::Linux, Platform::Windows, Platform::MacOs] {
            let (mut fake, mut context, config, data) = if platform == Platform::Windows {
                let (fake, context) = windows_admin(PROGRAM_FILES_EXE);
                (fake, context, PROGRAM_FILES_CONFIG, SCM_DATA)
            } else {
                (
                    Fake::default().uid(0).file(MACHINE_CONFIG, SYSTEM_CONFIG),
                    machine_context(),
                    MACHINE_CONFIG,
                    MACHINE_DATA,
                )
            };
            context.platform = platform;
            let updating = if supervised {
                Updating::supervised(data)
            } else {
                Updating::plain(Ok(data.to_owned()), "no machine install")
            };
            let (result, _) = execute_updating(
                &mut fake,
                &context,
                &["install", "-system", "-config", config],
                &updating,
            );
            if platform == Platform::MacOs {
                assert!(result.unwrap_err().contains("macOS"));
                assert!(fake.reads.borrow().is_empty());
                continue;
            }
            assert_eq!(result, Ok(()));
            let path = platform.join(data, RECORD_FILE);
            if supervised && platform != Platform::MacOs {
                let record: serde_json::Value = serde_json::from_slice(&fake.files[&path]).unwrap();
                assert_eq!(record["format"], 1);
                assert_eq!(record["kind"], "system");
                assert_eq!(
                    record["manager"],
                    if platform == Platform::Linux {
                        "systemd"
                    } else {
                        "scm"
                    }
                );
                assert_eq!(record["supervised"], true);
                assert_eq!(record["data_dir"], data);
                assert_eq!(fake.events.last(), Some(&format!("write_atomic {path}")));
            } else {
                assert!(!fake.files.contains_key(&path));
            }
        }
    }
}

fn scm_fake() -> (Fake, Context) {
    let (fake, context) = windows_admin(PROGRAM_FILES_EXE);
    let definition = scm_definition(true);
    let fake = fake
        .answer("sc.exe query open-ferry", 0, "STATE : 1 STOPPED")
        .answer(
            "sc.exe qc open-ferry",
            0,
            &format!(
                "BINARY_PATH_NAME : {}",
                windows::service_command(&definition)
            ),
        )
        .file(
            &Platform::Windows.join(SCM_DATA, RECORD_FILE),
            &record_text(Target::WindowsService, &definition, SCM_DATA),
        );
    (fake, context)
}

fn scm_states(fake: &mut Fake, states: &[&str]) {
    fake.queued.insert(
        "sc.exe query open-ferry".to_owned(),
        states
            .iter()
            .map(|stdout| Output {
                code: Some(0),
                stdout: (*stdout).to_owned(),
                stderr: String::new(),
            })
            .collect(),
    );
}

#[test]
fn scm_uninstall_stops_the_system_supervisor_then_waits_before_delete_and_record_removal() {
    for stop in [
        StopOutcome::Stopped,
        StopOutcome::NotRunning,
        StopOutcome::Refused("unavailable".to_owned()),
        StopOutcome::TimedOut("pending".to_owned()),
    ] {
        let (mut fake, context) = scm_fake();
        fake.stop = Some(stop);
        scm_states(
            &mut fake,
            &[
                // target.installed consumes RUNNING; the wait starts at STOP_PENDING.
                "STATE : 4 RUNNING",
                "STATE : 3 STOP_PENDING",
                "STATE : 1 STOPPED",
            ],
        );
        let (result, _) = execute_with(&mut fake, &context, &["uninstall", "-system"]);
        assert_eq!(result, Ok(()));
        assert_eq!(
            events(&fake),
            [
                "run sc.exe query open-ferry",
                "run sc.exe qc open-ferry",
                r"stop_supervisor C:\ProgramData\open-ferry 50s system",
                "run sc.exe stop open-ferry",
                "run sc.exe query open-ferry",
                "pause 1s",
                "run sc.exe query open-ferry",
                "run sc.exe delete open-ferry",
                r"remove C:\ProgramData\open-ferry\service.json",
            ]
        );
    }
}

#[test]
fn scm_stopped_on_the_last_poll_counts_when_the_clock_overshoots() {
    let (mut fake, _) = scm_fake();
    scm_states(&mut fake, &["STATE : 3 STOP_PENDING", "STATE : 1 STOPPED"]);
    let ticks = RefCell::new(VecDeque::from([
        Duration::ZERO,
        scm_stop_budget() + Duration::from_millis(5),
    ]));
    let mut out = Vec::new();
    assert_eq!(
        wait_service_stopped_with_clock(&mut fake, &mut out, || ticks
            .borrow_mut()
            .pop_front()
            .unwrap()),
        Ok(())
    );
    assert_eq!(
        events(&fake),
        [
            "run sc.exe query open-ferry",
            "pause 1s",
            "run sc.exe query open-ferry"
        ]
    );
}

#[test]
fn a_system_service_refuses_an_untrusted_dotenv_in_its_working_directory() {
    let fake = Fake::default().uid(0).file(MACHINE_CONFIG, SYSTEM_CONFIG);
    let context = machine_context();
    let dotenv = "/etc/open-ferry/.env";
    let mut fake = fake.file(dotenv, "unused");
    fake.untrusted.insert(dotenv.to_owned());
    let (result, _) = execute_with(
        &mut fake,
        &context,
        &[
            "install",
            "-system",
            "-config",
            "/etc/open-ferry/config.yaml",
        ],
    );
    let error = result.unwrap_err();
    assert!(error.contains(dotenv), "{error}");
    assert!(error.contains("remove it"), "{error}");
    assert!(
        !fake
            .events
            .iter()
            .any(|event| event.starts_with("write ") || event.starts_with("mkdir "))
    );
    fake.untrusted.clear();
    assert_eq!(
        execute_with(
            &mut fake,
            &context,
            &[
                "install",
                "-system",
                "-config",
                "/etc/open-ferry/config.yaml"
            ]
        )
        .0,
        Ok(())
    );
}

#[test]
fn scm_uninstall_keeps_the_service_and_record_when_running_or_unknown_at_the_deadline() {
    for answer in ["STATE : 3 STOP_PENDING", "unrecognized"] {
        let (mut fake, context) = scm_fake();
        fake.answers.insert(
            "sc.exe query open-ferry".to_owned(),
            Output {
                code: Some(0),
                stdout: answer.to_owned(),
                stderr: String::new(),
            },
        );
        let (result, _) = execute_with(&mut fake, &context, &["uninstall", "-system"]);
        let error = result.unwrap_err();
        assert!(error.starts_with("Uninstall incomplete:"), "{error}");
        assert!(error.contains("after 60 s"), "{error}");
        assert_eq!(
            fake.events
                .iter()
                .filter(|event| event.starts_with("pause "))
                .count(),
            60
        );
        assert_eq!(
            fake.events
                .iter()
                .filter(|event| *event == "run sc.exe query open-ferry")
                .count(),
            62
        );
        assert!(
            !fake.events.iter().any(
                |event| event == "run sc.exe delete open-ferry" || event.starts_with("remove ")
            )
        );
        assert!(
            fake.files
                .contains_key(&Platform::Windows.join(SCM_DATA, RECORD_FILE))
        );
    }
}

#[test]
fn scm_uninstall_checks_stopped_even_when_sc_stop_fails() {
    let (fake, context) = scm_fake();
    let mut fake = fake.fail("sc.exe stop open-ferry", 1062, "not running");
    let (result, out) = execute_with(&mut fake, &context, &["uninstall", "-system"]);
    assert_eq!(result, Ok(()));
    assert!(out.contains("not running; going on"));
    assert!(
        fake.events
            .iter()
            .any(|event| event == "run sc.exe delete open-ferry")
    );
}

#[test]
fn a_failed_scm_delete_keeps_the_system_record() {
    let (fake, context) = scm_fake();
    let mut fake = fake.fail("sc.exe delete open-ferry", 5, "access denied");
    let (result, _) = execute_with(&mut fake, &context, &["uninstall", "-system"]);
    assert!(result.unwrap_err().contains("access denied"));
    assert!(
        fake.files
            .contains_key(&Platform::Windows.join(SCM_DATA, RECORD_FILE))
    );
    assert!(!fake.events.iter().any(|event| event.starts_with("remove ")));
}

#[test]
fn scm_definitions_are_read_from_one_complete_binary_path_line() {
    let definition = scm_definition(true);
    let command = windows::service_command(&definition);
    let output = format!(
        "[SC] QueryServiceConfig SUCCESS\r\n\r\nSERVICE_NAME: open-ferry\r\n        BINARY_PATH_NAME   : {command}\r\n        SERVICE_START_NAME : LocalSystem\r\n"
    );
    let words = windows::service_definition(&output).unwrap();
    assert_eq!(
        flagged(&words, words.first().cloned()),
        Flagged {
            supervised: true,
            data_dir: Some(SCM_DATA.to_owned()),
            exe: Some(PROGRAM_FILES_EXE.to_owned()),
            config: Some(PROGRAM_FILES_CONFIG.to_owned()),
        }
    );
    for bad in [
        "no command".to_owned(),
        "BINARY_PATH_NAME : ".to_owned(),
        "BINARY_PATH_NAME : \"\"".to_owned(),
        format!("{output}\n BINARY_PATH_NAME: {command}"),
        "BINARY_PATH_NAME : \"unfinished".to_owned(),
    ] {
        assert_eq!(windows::service_definition(&bad), None);
    }
}

#[test]
fn a_missing_or_unqueryable_scm_state_is_never_taken_for_stopped() {
    for code in [5, 1060] {
        let (mut fake, context) = scm_fake();
        scm_states(&mut fake, &["STATE : 4 RUNNING"]);
        fake = fake.fail("sc.exe query open-ferry", code, "query failed");
        let (result, _) = execute_with(&mut fake, &context, &["uninstall", "-system"]);
        assert!(result.unwrap_err().starts_with("Uninstall incomplete:"));
        assert!(
            !fake
                .events
                .iter()
                .any(|event| event == "run sc.exe delete open-ferry")
        );
        assert!(
            fake.files
                .contains_key(&Platform::Windows.join(SCM_DATA, RECORD_FILE))
        );
    }
}

fn system_unit_fake() -> Fake {
    Fake::default()
        .uid(0)
        .file(SYSTEM_UNIT, &systemd::unit(&machine_definition(true), true))
        .file(
            &format!("{MACHINE_DATA}/{RECORD_FILE}"),
            &record_text(
                Target::SystemdSystem,
                &machine_definition(true),
                MACHINE_DATA,
            ),
        )
}

#[test]
fn every_system_unit_has_a_checked_stop_and_keeps_its_record_until_complete() {
    for supervised in [false, true] {
        for active in ["active", "activating", "", "inactive", "failed", "unknown"] {
            let mut fake = system_unit_fake()
                .file(
                    SYSTEM_UNIT,
                    &systemd::unit(&machine_definition(supervised), true),
                )
                .fail(
                    "systemctl disable --now open-ferry.service",
                    1,
                    "stop failed",
                )
                .answer("systemctl is-active open-ferry.service", 3, active);
            let (result, _) =
                execute_with(&mut fake, &machine_context(), &["uninstall", "-system"]);
            let ended = matches!(active, "inactive" | "failed" | "unknown");
            assert_eq!(result.is_ok(), ended, "{active}: {result:?}");
            assert_eq!(fake.files.contains_key(SYSTEM_UNIT), !ended);
            assert_eq!(
                fake.files
                    .contains_key(&format!("{MACHINE_DATA}/{RECORD_FILE}")),
                !ended || !supervised
            );
            let mut expected = vec![
                "run id -u",
                "run systemctl disable --now open-ferry.service",
                "run systemctl is-active open-ferry.service",
            ];
            if ended {
                expected.extend([
                    "remove /etc/systemd/system/open-ferry.service",
                    "run systemctl daemon-reload",
                ]);
                if supervised {
                    expected.push("remove /var/lib/open-ferry/service.json");
                }
            }
            assert_eq!(events(&fake), expected);
        }
    }
    let mut fake = system_unit_fake().fail("systemctl daemon-reload", 1, "reload failed");
    assert!(
        execute_with(&mut fake, &machine_context(), &["uninstall", "-system"])
            .0
            .is_err()
    );
    assert!(
        fake.files
            .contains_key(&format!("{MACHINE_DATA}/{RECORD_FILE}"))
    );
}

#[test]
fn a_plain_daemon_is_kept_when_bootout_fails_unless_print_proves_it_is_not_loaded() {
    let mut context = machine_context();
    context.platform = Platform::MacOs;
    let path = launchd::plist_path(&context, true).unwrap();
    for (code, stdout, removed) in [
        (0, "loaded", false),
        (1, "error", false),
        (113, "", true),
        (1, "Could not find service", true),
    ] {
        let mut fake = Fake::default()
            .uid(0)
            .file(
                &path,
                &launchd::plist(
                    &machine_definition(false),
                    "/Library/Logs/open-ferry.log",
                    true,
                ),
            )
            .fail(
                "launchctl bootout system/io.github.loft-902-co-llc.open-ferry",
                1,
                "bootout failed",
            )
            .answer(
                "launchctl print system/io.github.loft-902-co-llc.open-ferry",
                code,
                stdout,
            );
        let (result, _) = execute_with(&mut fake, &context, &["uninstall", "-system"]);
        assert_eq!(result.is_ok(), removed);
        assert_eq!(fake.files.contains_key(&path), !removed);
        assert_eq!(
            fake.events[1..3],
            [
                "run launchctl bootout system/io.github.loft-902-co-llc.open-ferry",
                "run launchctl print system/io.github.loft-902-co-llc.open-ferry"
            ]
        );
    }
}

#[test]
fn untrusted_system_data_is_never_read_stopped_through_or_removed() {
    for windows in [false, true] {
        let (mut fake, context, dir) = if windows {
            let (fake, context) = scm_fake();
            (fake, context, SCM_DATA)
        } else {
            (system_unit_fake(), machine_context(), MACHINE_DATA)
        };
        fake.untrusted.insert(dir.to_owned());
        let (result, out) = execute_with(&mut fake, &context, &["uninstall", "-system"]);
        assert_eq!(result, Ok(()));
        assert!(out.contains(&format!("Not using the service's data directory {dir}")));
        assert!(!fake.reads.borrow().iter().any(|path| path.starts_with(dir)));
        assert!(
            !fake
                .events
                .iter()
                .any(|event| event.starts_with("stop_supervisor ")
                    || event == &format!("remove {}", context.platform.join(dir, RECORD_FILE)))
        );
        assert!(
            fake.files
                .contains_key(&context.platform.join(dir, RECORD_FILE))
        );
    }
}

#[test]
fn system_uninstall_never_uses_the_environment_or_an_invalid_record_as_its_data_directory() {
    let (mut fake, context) = scm_fake();
    let command = windows::service_command(&scm_definition(true)).replace(SCM_DATA, "relative");
    fake = fake.answer(
        "sc.exe qc open-ferry",
        0,
        &format!("BINARY_PATH_NAME : {command}"),
    );
    let (result, out) = execute_with(&mut fake, &context, &["uninstall", "-system"]);
    assert_eq!(result, Ok(()));
    assert!(out.contains("it isn't a full path"));
    assert!(stopped(&fake).is_empty());
    for record in [
        "{}".to_owned(),
        record_text(Target::ScheduledTask, &scm_definition(true), SCM_DATA),
        record_text(
            Target::WindowsService,
            &Definition {
                exe: r"C:\other.exe".to_owned(),
                ..scm_definition(true)
            },
            SCM_DATA,
        ),
    ] {
        let (fake, context) = scm_fake();
        let path = Platform::Windows.join(SCM_DATA, RECORD_FILE);
        let mut fake = fake.file(&path, &record);
        let (result, out) = execute_with(&mut fake, &context, &["uninstall", "-system"]);
        assert_eq!(result, Ok(()));
        assert!(out.contains("Leaving "));
        assert!(fake.files.contains_key(&path));
    }
    let (fake, context) = scm_fake();
    let mut fake = fake.fail("sc.exe qc open-ferry", 5, "access denied");
    assert_eq!(
        execute_with(&mut fake, &context, &["uninstall", "-system"]).0,
        Ok(())
    );
    assert!(stopped(&fake).is_empty());
    assert!(
        fake.files
            .contains_key(&Platform::Windows.join(SCM_DATA, RECORD_FILE))
    );
}

#[test]
fn system_dry_runs_describe_both_forms_and_the_wait_without_changing_anything() {
    for supervised in [false, true] {
        for windows in [false, true] {
            let (mut fake, context, config, data) = if windows {
                let (fake, context) = windows_admin(PROGRAM_FILES_EXE);
                (fake, context, PROGRAM_FILES_CONFIG, SCM_DATA)
            } else {
                (
                    Fake::default().uid(0).file(MACHINE_CONFIG, SYSTEM_CONFIG),
                    machine_context(),
                    MACHINE_CONFIG,
                    MACHINE_DATA,
                )
            };
            let updating = if supervised {
                Updating::supervised(data)
            } else {
                Updating::plain(Ok(data.to_owned()), NO_MACHINE_INSTALL)
            };
            let (result, out) = execute_updating(
                &mut fake,
                &context,
                &["install", "-system", "-dry-run", "-config", config],
                &updating,
            );
            assert_eq!(result, Ok(()));
            assert!(out.contains(if windows {
                "Would run: sc.exe create open-ferry"
            } else {
                "Would write /etc/systemd/system/open-ferry.service:"
            }));
            assert_eq!(out.contains("-supervise"), supervised);
            assert_eq!(out.contains("service.json:"), supervised);
            assert!(out.contains("Dry run: nothing was changed."));
            assert!(
                !fake
                    .events
                    .iter()
                    .any(|event| event.starts_with("write") || event.contains(" create "))
            );
        }
    }
    let (mut fake, context) = scm_fake();
    let (result, out) = execute_with(&mut fake, &context, &["uninstall", "-system", "-dry-run"]);
    assert_eq!(result, Ok(()));
    let before = out.find("Would ask the supervisor").unwrap();
    let stop = out.find("Would run: sc.exe stop").unwrap();
    let wait = out.find("Would wait up to 60 s").unwrap();
    let delete = out.find("Would run: sc.exe delete").unwrap();
    let remove = out.find("Would remove C:").unwrap();
    assert!(before < stop && stop < wait && wait < delete && delete < remove);
    assert_eq!(
        events(&fake),
        ["run sc.exe query open-ferry", "run sc.exe qc open-ferry"]
    );
}
#[test]
fn elevated_windows_account_installs_are_refused_before_any_plan_or_config_read() {
    for dry_run in [false, true] {
        let mut fake = windows_user().answer(
            "whoami.exe /groups /fo csv /nh",
            0,
            "\"Mandatory Label\\High Mandatory Level\",\"Label\",\"S-1-16-12288\",\"\"\r\n",
        );
        let mut args = vec!["install"];
        if dry_run {
            args.push("-dry-run");
        }
        let (result, _) = execute_with(&mut fake, &windows(), &args);
        let error = result.unwrap_err();
        assert!(error.contains("This terminal is elevated"), "{error}");
        assert!(error.contains("add -system"), "{error}");
        assert!(events(&fake).is_empty());
        assert!(fake.reads.borrow().is_empty());
        assert!(!fake.files.contains_key(TASK_FILE));
    }
}

#[test]
fn elevated_windows_account_services_can_still_be_removed_or_shown() {
    for action in ["uninstall", "status"] {
        let mut fake =
            Fake::default().answer("whoami.exe /groups /fo csv /nh", 0, "\"S-1-16-16384\"\r\n");
        assert_eq!(execute_with(&mut fake, &windows(), &[action]).0, Ok(()));
        let expected = if action == "uninstall" {
            "run schtasks.exe /delete /tn open-ferry /f"
        } else {
            "show schtasks.exe /query /tn open-ferry /fo list /v"
        };
        assert!(events(&fake).contains(&expected));
    }
}

#[test]
fn system_installs_keep_auth_files_out_of_machine_records() {
    for (platform, auth, accepted) in [
        (Platform::Linux, "/var/lib/open-ferry", false),
        (Platform::Linux, "/var/lib/open-ferry/versions", false),
        (Platform::Linux, "/var/lib/open-ferry/auth", true),
        (Platform::Linux, "/srv/credentials", true),
        (Platform::Linux, "../../var/lib/open-ferry/versions", false),
        (Platform::Windows, r"C:\ProgramData\open-ferry\logs", false),
        (Platform::Windows, r"C:\ProgramData\open-ferry\auth", true),
        (Platform::Windows, r"D:\credentials", true),
    ] {
        let (mut fake, context, config) = if platform == Platform::Linux {
            let mut context = linux();
            context.exe = "/usr/local/bin/open-ferry".into();
            (
                Fake::default().uid(0),
                context,
                "/etc/open-ferry/config.yaml",
            )
        } else {
            let (fake, context) = windows_admin(PROGRAM_FILES_EXE);
            (fake, context, PROGRAM_FILES_CONFIG)
        };
        fake = fake.file(config, &format!("auth-dir: '{auth}'\n"));
        let (result, _) = execute_with(
            &mut fake,
            &context,
            &["install", "-system", "-config", config, "-dry-run"],
        );
        if accepted {
            assert_eq!(result, Ok(()), "{auth}");
        } else {
            let error = result.unwrap_err();
            assert!(
                error.contains("credential files among the machine's records")
                    || error.contains("without `.` or `..`"),
                "{auth}: {error}"
            );
        }
        assert!(
            !events(&fake)
                .iter()
                .any(|event| event.starts_with("write "))
        );
    }
}

#[test]
fn an_untrusted_system_config_is_never_read_as_given_or_resolved() {
    for platform in [Platform::Linux, Platform::Windows] {
        let (fake, context, config) = if platform == Platform::Linux {
            let mut context = linux();
            context.exe = "/usr/local/bin/open-ferry".into();
            (
                Fake::default().uid(0),
                context,
                "/etc/open-ferry/config.yaml",
            )
        } else {
            let (fake, context) = windows_admin(PROGRAM_FILES_EXE);
            (fake, context, PROGRAM_FILES_CONFIG)
        };
        for resolved in [false, true] {
            let mut fake = fake.clone();
            fake = fake.file(config, "not: [valid YAML");
            let bad = if resolved && platform == Platform::Linux {
                let destination = "/etc/open-ferry/unsafe.yaml";
                fake.resolved_paths
                    .insert(config.to_owned(), destination.to_owned());
                fake = fake.file(destination, "not: [valid YAML");
                destination
            } else {
                config
            };
            fake.untrusted.insert(bad.to_owned());
            let (result, _) = execute_with(
                &mut fake,
                &context,
                &["install", "-system", "-config", config, "-dry-run"],
            );
            assert!(result.is_err());
            assert!(
                fake.reads.borrow().is_empty(),
                "{platform:?}: read before refusal"
            );
        }
    }
}

#[test]
fn install_auth_placement_resolves_both_the_auth_directory_and_machine_root() {
    for (platform, root, existing_root, canonical_root, auth, canonical_auth, accepted) in [
        (
            Platform::Windows,
            r"C:\PROGRA~3\OPEN-F~1",
            r"C:\PROGRA~3\OPEN-F~1",
            r"C:\ProgramData\open-ferry",
            r"C:\ProgramData\open-ferry\logs",
            r"C:\ProgramData\open-ferry\logs",
            false,
        ),
        (
            Platform::Windows,
            r"C:\PROGRA~3\OPEN-F~1",
            r"C:\PROGRA~3\OPEN-F~1",
            r"\\?\C:\ProgramData\open-ferry",
            r"D:\credentials",
            r"\\?\c:\PROGRAMDATA\OPEN-FERRY\logs",
            false,
        ),
        (
            Platform::Windows,
            r"C:\PROGRA~3\OPEN-F~1",
            r"C:\PROGRA~3",
            r"\\?\C:\ProgramData",
            r"D:\credentials",
            r"C:\ProgramData\OPEN-F~1\logs",
            false,
        ),
        (
            Platform::Windows,
            r"C:\PROGRA~3\OPEN-F~1",
            r"C:\PROGRA~3\OPEN-F~1",
            r"\\?\C:\ProgramData\open-ferry",
            r"C:\PROGRA~3\OPEN-F~1\auth",
            r"\\?\c:\PROGRAMDATA\OPEN-FERRY\auth",
            true,
        ),
        (
            Platform::Windows,
            r"C:\PROGRA~3\OPEN-F~1",
            r"C:\PROGRA~3\OPEN-F~1",
            r"\\?\UNC\server\share\open-ferry",
            r"D:\credentials",
            r"\\server\share\open-ferry\logs",
            false,
        ),
        (
            Platform::Windows,
            r"C:\PROGRA~3\OPEN-F~1",
            r"C:\PROGRA~3\OPEN-F~1",
            r"\\?\UNC\server\share\open-ferry",
            r"C:\PROGRA~3\OPEN-F~1\auth",
            r"\\server\share\OPEN-FERRY\auth",
            true,
        ),
        (
            Platform::Windows,
            r"C:\PROGRA~3\OPEN-F~1",
            r"C:\PROGRA~3\OPEN-F~1",
            r"\\?\C:\ProgramData\open-ferry",
            r"D:\credentials",
            r"\\?\D:\credentials",
            true,
        ),
        (
            Platform::Linux,
            "/machine",
            "/machine",
            "/var/lib/open-ferry",
            "/credentials",
            "/var/lib/open-ferry/logs",
            false,
        ),
        (
            Platform::Linux,
            "/machine",
            "/",
            "/real",
            "/credentials",
            "/real/machine/logs",
            false,
        ),
        (
            Platform::Linux,
            "/machine",
            "/machine",
            "/var/lib/open-ferry",
            "/machine/auth",
            "/var/lib/open-ferry/auth",
            true,
        ),
    ] {
        let mut fake = Fake::default().dir(existing_root).dir(auth);
        fake.resolved_paths
            .insert(existing_root.into(), canonical_root.into());
        fake.resolved_paths
            .insert(auth.into(), canonical_auth.into());
        let result = authorize_auth_dir_with(
            platform,
            root,
            auth,
            &|path| {
                if fake.dirs.contains(path) {
                    scope::State::Directory
                } else {
                    scope::State::Missing
                }
            },
            &|path| fake.trusted_dir(path),
            &|path| fake.real_path(path),
        );
        assert_eq!(result.is_ok(), accepted, "{root}, {auth}: {result:?}");
        if !accepted {
            assert!(
                result
                    .unwrap_err()
                    .contains("credential files among the machine's records")
            );
        }
    }
}

#[test]
fn auth_directory_trust_and_aliases_are_checked_at_install() {
    for (auth, existing, untrusted, alias, accepted) in [
        (
            r"C:\ProgramData\open-ferry\auth",
            r"C:\ProgramData\open-ferry",
            false,
            None,
            true,
        ),
        (r"D:\credentials", r"D:\credentials", false, None, true),
        (r"D:\credentials", r"D:\credentials", true, None, false),
        (
            r"D:\credentials\missing",
            r"D:\credentials",
            true,
            None,
            false,
        ),
        (
            r"C:\PROGRA~3\OPEN-F~1\logs",
            r"C:\PROGRA~3\OPEN-F~1",
            false,
            Some(r"C:\ProgramData\open-ferry"),
            false,
        ),
        (
            r"C:\PROGRA~3\OPEN-F~1\logs",
            r"C:\PROGRA~3\OPEN-F~1",
            false,
            Some(r"\\?\C:\ProgramData\open-ferry"),
            false,
        ),
        (
            r"c:\programdata\OPEN-FERRY\logs",
            r"c:\programdata\OPEN-FERRY",
            false,
            None,
            false,
        ),
        (
            r"D:\credentials\..\auth",
            r"D:\credentials",
            false,
            None,
            false,
        ),
        (
            r"D:\credentials\.\auth",
            r"D:\credentials",
            false,
            None,
            false,
        ),
    ] {
        let (mut fake, context) = windows_admin(PROGRAM_FILES_EXE);
        fake = fake
            .dir(existing)
            .file(PROGRAM_FILES_CONFIG, &format!("auth-dir: '{auth}'\n"));
        if untrusted {
            fake.untrusted.insert(existing.to_owned());
        }
        if let Some(actual) = alias {
            fake.resolved_paths
                .insert(existing.to_owned(), actual.to_owned());
        }
        let result = execute_with(
            &mut fake,
            &context,
            &[
                "install",
                "-system",
                "-dry-run",
                "-config",
                PROGRAM_FILES_CONFIG,
            ],
        )
        .0;
        assert_eq!(result.is_ok(), accepted, "{auth}: {result:?}");
        if !accepted {
            assert!(result.unwrap_err().contains("set auth-dir"));
        }
    }
    for file in [false, true] {
        let (mut fake, context) = windows_admin(PROGRAM_FILES_EXE);
        let auth = r"D:\credentials";
        fake = fake.file(PROGRAM_FILES_CONFIG, &format!("auth-dir: '{auth}'\n"));
        if file {
            fake = fake.file(auth, "credentials");
        } else {
            fake = fake.dir(auth).link(auth, r"C:\ProgramData\open-ferry\logs");
        }
        let error = execute_with(
            &mut fake,
            &context,
            &[
                "install",
                "-system",
                "-dry-run",
                "-config",
                PROGRAM_FILES_CONFIG,
            ],
        )
        .0
        .unwrap_err();
        assert!(error.contains("Refusing the auth directory"), "{error}");
    }
    // A refused default directory isn't suggested in its own place.
    for auth in [
        r"C:\ProgramData\open-ferry\auth",
        r"c:\programdata\open-ferry\AUTH",
    ] {
        let (mut fake, context) = windows_admin(PROGRAM_FILES_EXE);
        fake = fake
            .file(PROGRAM_FILES_CONFIG, &format!("auth-dir: '{auth}'\n"))
            .dir(auth);
        fake.untrusted.insert(auth.to_owned());
        let error = execute_with(
            &mut fake,
            &context,
            &[
                "install",
                "-system",
                "-dry-run",
                "-config",
                PROGRAM_FILES_CONFIG,
            ],
        )
        .0
        .unwrap_err();
        assert!(error.contains("Refusing the auth directory"), "{error}");
        assert!(!error.contains("set auth-dir"), "{error}");
    }
}

#[test]
fn helper_paths_do_not_search_the_working_directory_or_path() {
    #[cfg(windows)]
    for name in ["whoami.exe", "schtasks.exe", "sc.exe"] {
        let path = system_helper_path(name).unwrap();
        assert!(path.is_absolute());
        assert_eq!(path.file_name().unwrap(), name);
        assert!(
            path.parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .eq_ignore_ascii_case("system32")
        );
    }
    #[cfg(unix)]
    {
        let path = system_helper_path("id").unwrap();
        assert!(
            ["/usr/sbin", "/usr/bin", "/sbin", "/bin"]
                .iter()
                .any(|dir| path.parent().unwrap() == Path::new(dir))
        );
        assert!(system_helper_path("open-ferry-missing-test-helper").is_err());
    }
}

#[test]
fn helpers_search_path_only_for_an_unprivileged_probe() {
    let dir = crate::scope::runtime_test_probe::scratch_dir();
    let name = format!("open-ferry-fake-helper{}", std::env::consts::EXE_SUFFIX);
    let helper = dir.path().join(&name);
    std::fs::write(&helper, "fake helper: never executed").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = std::env::join_paths([
        Path::new(""),
        Path::new("."),
        Path::new("relative"),
        dir.path(),
    ])
    .unwrap();
    let mut probe = crate::update::scope_probe::Fake::new(Platform::HOST.unwrap(), false);
    assert_eq!(
        helper_path_with(&name, &probe, Some(&path), None).unwrap(),
        helper
    );
    for privilege in [Ok(true), Err("can't determine privilege".to_owned())] {
        probe.privileged = privilege;
        let found = helper_path_with(&name, &probe, Some(&path), None);
        #[cfg(windows)]
        assert_eq!(found.unwrap(), system_helper_path(&name).unwrap());
        #[cfg(unix)]
        assert_eq!(found.unwrap_err().kind(), io::ErrorKind::NotFound);
    }
}

#[test]
fn unprivileged_helpers_ignore_missing_empty_and_relative_path_entries() {
    let probe = crate::update::scope_probe::Fake::new(Platform::HOST.unwrap(), false);
    let path = std::env::join_paths(["", ".", "relative"]).unwrap();
    for path in [None, Some(path.as_os_str())] {
        // Cargo.toml exists in the test's working directory, but must never be found there.
        assert_eq!(
            helper_path_with("Cargo.toml", &probe, path, None)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }
}

#[cfg(windows)]
#[test]
fn unprivileged_helpers_use_pathext_and_path_order() {
    let dir = crate::scope::runtime_test_probe::scratch_dir();
    let first = dir.path().join("first");
    let second = dir.path().join("second");
    std::fs::create_dir_all(&first).unwrap();
    std::fs::create_dir_all(&second).unwrap();
    for path in [
        first.join("docker"),
        first.join("docker.CMD"),
        first.join("docker.EXE"),
        second.join("docker.COM"),
    ] {
        std::fs::write(path, "fake: never executed").unwrap();
    }
    let path = std::env::join_paths([&first, &second]).unwrap();
    let probe = crate::update::scope_probe::Fake::new(Platform::Windows, false);
    assert_eq!(
        helper_path_with(
            "docker",
            &probe,
            Some(&path),
            Some(std::ffi::OsStr::new(".EXE;.CMD"))
        )
        .unwrap(),
        first.join("docker.EXE")
    );
    assert_eq!(
        helper_path_with("docker", &probe, Some(&path), None).unwrap(),
        first.join("docker.EXE")
    );
    assert_eq!(
        helper_path_with("docker.CMD", &probe, Some(&path), None).unwrap(),
        first.join("docker.CMD")
    );
}

#[cfg(unix)]
#[test]
fn unprivileged_helpers_skip_files_without_execute_permission() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = crate::scope::runtime_test_probe::scratch_dir();
    let helper = dir.path().join("docker");
    std::fs::write(&helper, "fake: never executed").unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o644)).unwrap();
    let path = std::env::join_paths([dir.path()]).unwrap();
    let probe = crate::update::scope_probe::Fake::new(Platform::Linux, false);
    assert_eq!(
        helper_path_with("docker", &probe, Some(&path), None)
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        helper_path_with("docker", &probe, Some(&path), None).unwrap(),
        helper
    );
}

#[test]
fn privileged_account_install_refuses_before_context_or_receipt_assessment() {
    for platform in [Platform::Linux, Platform::Windows] {
        for dry_run in [false, true] {
            let mut fake = Fake {
                privileged: true,
                ..Fake::default()
            };
            let mut args = vec!["install"];
            if dry_run {
                args.push("-dry-run");
            }
            let request = parse(super::tests::args(&args)).unwrap();
            let result = run(
                &mut fake,
                platform,
                &request,
                || panic!("must not discover context before refusal"),
                |_, _| panic!("must not assess or read a receipt before refusal"),
                &mut Vec::new(),
            );
            assert!(result.unwrap_err().contains("add -system"));
            assert!(fake.events.is_empty());
            assert!(fake.reads.borrow().is_empty());
        }
    }
}

#[test]
fn user_install_assessment_selects_the_form_and_keeps_the_plain_reason() {
    use open_ferry_update::install::NotSelfUpdating;
    for why in [
        None,
        Some(NotSelfUpdating::NoReceipt),
        Some(NotSelfUpdating::Container),
        Some(NotSelfUpdating::ReadOnly("/bin".to_owned())),
    ] {
        let install = match &why {
            Some(why) => Install::NotifyOnly(why.clone()),
            None => Install::SelfUpdating {
                binary: linux().exe.into(),
            },
        };
        let updating = Updating::from_install(Ok(LINUX_DATA.to_owned()), &install);
        let mut fake = Fake::default().uid(1000).file(LINUX_CONFIG, CONFIG);
        let prepared = prepare_install(
            &mut fake,
            &linux(),
            Target::SystemdUser,
            LINUX_CONFIG,
            Some("/home/me/working"),
            true,
            &updating,
        )
        .unwrap();
        assert_eq!(prepared.definition.dir, "/home/me/working");
        assert_eq!(
            prepared.definition.data_dir.as_deref(),
            why.is_none().then_some(LINUX_DATA)
        );
        if let Some(why) = why {
            assert!(
                prepared.form.contains(&why.to_string()),
                "{}",
                prepared.form
            );
            assert!(prepared.form.contains("Installing the plain form"));
        } else {
            assert!(prepared.form.contains("Installing the supervised form"));
        }
        assert!(
            fake.events
                .iter()
                .all(|event| event.starts_with("run id -u")
                    || event.starts_with("run systemctl --user show"))
        );
    }
}

#[test]
fn macos_system_install_refuses_before_context_or_receipt_assessment() {
    for (uid, dry_run) in [(0, false), (0, true), (501, false), (501, true)] {
        let mut fake = Fake::default().uid(uid);
        let mut command = vec!["install", "-system"];
        if dry_run {
            command.push("-dry-run");
        }
        let request = parse(args(&command)).unwrap();
        let result = run(
            &mut fake,
            Platform::MacOs,
            &request,
            || panic!("must not discover context for a macOS system install"),
            |_, _| panic!("must not assess a refused install"),
            &mut Vec::new(),
        );
        assert_eq!(
            result.unwrap_err(),
            "open-ferry doesn't support machine-wide installs or running as root on macOS in this version. Install a user service with `open-ferry service install` without -system."
        );
        assert!(fake.events.is_empty());
        assert!(fake.reads.borrow().is_empty());
    }
}
