use std::collections::{BTreeMap, BTreeSet};

use super::*;

/// A system that records what is done to it, and answers commands as the
/// test says; a command it wasn't told of succeeds and prints nothing.
#[derive(Default)]
struct Fake {
    files: BTreeMap<String, Vec<u8>>,
    dirs: BTreeSet<String>,
    owners: BTreeMap<String, Owner>,
    links: BTreeMap<String, String>,
    answers: BTreeMap<String, Output>,
    events: Vec<String>,
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

    fn uid(self, uid: u32) -> Fake {
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
}

impl System for Fake {
    fn run(&mut self, cmd: &Cmd) -> io::Result<Output> {
        self.events.push(format!("run {cmd}"));
        Ok(self.answer_for(cmd))
    }

    fn show(&mut self, cmd: &Cmd) -> io::Result<Option<i32>> {
        self.events.push(format!("show {cmd}"));
        Ok(self.answer_for(cmd).code)
    }

    fn exists(&self, path: &str) -> bool {
        self.files.contains_key(path) || self.dirs.contains(path)
    }

    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }

    fn real_path(&self, path: &str) -> io::Result<String> {
        Ok(self
            .links
            .get(path)
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
const SYSTEM_CONFIG: &str = "port: 8317\nauth-dir: /var/lib/open-ferry\n";

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
    let result = execute(fake, context, &request, &mut out);
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
        ]
    );
    assert_eq!(
        text(&fake, LINUX_UNIT),
        systemd::unit(&user_definition(), false)
    );
    assert_eq!(
        out,
        "Created /home/me/.config/systemd/user
Wrote /home/me/.config/systemd/user/open-ferry.service
Ran: systemctl --user daemon-reload
Ran: systemctl --user enable --now open-ferry.service
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
            "Would create /home/me/.config/systemd/user
Would write /home/me/.config/systemd/user/open-ferry.service:
{unit}Would run: systemctl --user daemon-reload
Would run: systemctl --user enable --now open-ferry.service
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
        "Would run: systemctl --user disable --now open-ferry.service
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
    // Not upstream's: root's user service is almost certainly a mistake.
    let mut fake = Fake::default().uid(0).file(LINUX_CONFIG, CONFIG);
    let (result, _) = execute_with(&mut fake, &linux(), &["install"]);
    assert_eq!(
        result,
        Err(format!(
            "You are root, so this would install a service for root. Run it as the user the service is for, or add -system for a service of the whole machine:\n  /home/me/.local/bin/open-ferry service install -config {LINUX_CONFIG}"
        ))
    );
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
    // Root's group may write, and a sticky directory protects root's files.
    let mut fake = base()
        .owner("/etc/open-ferry", 0, 0, 0o775, true)
        .owner("/srv", 0, 0, 0o1777, true)
        .dir("/etc/systemd/system");
    assert_eq!(system_install(&mut fake, "/srv/open-ferry"), Ok(()));
}

#[test]
fn installs_a_systemd_system_unit() {
    // Not upstream's: `install -system` on Linux runs what links point to.
    let mut fake = Fake::default()
        .uid(0)
        .file("/etc/open-ferry/config.yaml", SYSTEM_CONFIG)
        .link("/usr/local/bin/open-ferry", "/opt/open-ferry/open-ferry")
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
        exe: "/opt/open-ferry/open-ferry".to_owned(),
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
        );
    let (result, out) = execute_with(&mut fake, &linux(), &["uninstall"]);
    assert_eq!(result, Ok(()));
    assert_eq!(
        events(&fake),
        [
            "run id -u",
            "run systemctl --user disable --now open-ferry.service",
            "remove /home/me/.config/systemd/user/open-ferry.service",
            "run systemctl --user daemon-reload",
        ]
    );
    assert!(fake.files.contains_key(LINUX_CONFIG));
    assert_eq!(
        out,
        "Ran: systemctl --user disable --now open-ferry.service (failed with exit code 1: Failed to disable unit; going on)
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
        ]
    );
    let definition = Definition {
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
        ]
    );
}

#[test]
fn installs_a_launchd_daemon() {
    // Not upstream's: `install -system` on macOS.
    let mut fake = Fake::default()
        .uid(0)
        .file(
            "/Library/Application Support/open-ferry/config.yaml",
            SYSTEM_CONFIG,
        )
        .dir("/Library/LaunchDaemons")
        .dir("/Library/Logs");
    let context = context(
        Platform::MacOs,
        "/usr/local/bin/open-ferry",
        "/",
        &[("HOME", "/var/root")],
    );
    let (result, _) = execute_with(
        &mut fake,
        &context,
        &[
            "install",
            "-system",
            "-config",
            "/Library/Application Support/open-ferry/config.yaml",
        ],
    );
    assert_eq!(result, Ok(()));
    let plist = "/Library/LaunchDaemons/io.github.loft-902-co-llc.open-ferry.plist";
    assert_eq!(
        events(&fake),
        [
            "run id -u",
            &format!("write {plist}"),
            "run launchctl enable system/io.github.loft-902-co-llc.open-ferry",
            &format!("run launchctl bootstrap system {plist}"),
        ]
    );
    assert!(text(&fake, plist).contains("<string>/Library/Logs/open-ferry.log</string>"));

    fake.events.clear();
    let (result, _) = execute_with(&mut fake, &context, &["status", "-system"]);
    assert_eq!(result, Ok(()));
    assert_eq!(
        events(&fake),
        [
            "run id -u",
            "show launchctl print system/io.github.loft-902-co-llc.open-ferry"
        ]
    );
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
    assert_eq!(out, format!("Wrote {TASK_FILE}\nRemoved {TASK_FILE}\n"));
}

#[test]
fn plans_the_task_in_utf16_for_the_user_and_removes_its_file() {
    // Not upstream's: `schtasks /create /xml` reads UTF-16, and the file is
    // only needed until the task is registered.
    let mut fake = windows_user();
    let context = windows();
    let definition = Definition {
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
            "run whoami.exe /groups /fo csv /nh",
            "run sc.exe query open-ferry",
            r#"run sc.exe create open-ferry binPath= "\"C:\Program Files\open-ferry\open-ferry.exe\" service run -system -config \"C:\Program Files\open-ferry\config.yaml\"" start= auto DisplayName= open-ferry"#,
            r#"run sc.exe description open-ferry "open-ferry AI proxy""#,
            "run sc.exe failure open-ferry reset= 86400 actions= restart/5000",
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
            .contains(r"; C:\ProgramData\open-ferry\config.yaml isn't."),
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
    assert_eq!(
        events(&fake),
        [
            "run whoami.exe /groups /fo csv /nh",
            "run sc.exe query open-ferry"
        ]
    );
    assert!(out.ends_with(
        "Would run: sc.exe start open-ferry\nDry run: nothing was changed.\nWithout -dry-run, this needs an administrator.\n"
    ));
}

#[test]
fn uninstalls_a_windows_service() {
    // Not upstream's: `uninstall -system` on Windows stops the service, then
    // deletes it.
    let (fake, context) = windows_admin(PROGRAM_FILES_EXE);
    let mut fake = fake.answer("sc.exe query open-ferry", 0, "STATE : 4 RUNNING");
    let (result, _) = execute_with(&mut fake, &context, &["uninstall", "-system"]);
    assert_eq!(result, Ok(()));
    assert_eq!(
        events(&fake),
        [
            "run sc.exe query open-ferry",
            "run whoami.exe /groups /fo csv /nh",
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
