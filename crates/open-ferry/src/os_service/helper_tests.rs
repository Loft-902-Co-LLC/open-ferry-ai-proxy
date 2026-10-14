use super::*;
use crate::scope::{Probe, State};
use crate::update::scope_probe::Fake;
use open_ferry_update::receipt::{Receipt, ReceiptError};

struct LinkedProbe {
    fake: Fake,
    aliases: BTreeMap<String, String>,
    container: bool,
}

impl Probe for LinkedProbe {
    fn platform(&self) -> Platform {
        self.fake.platform()
    }
    fn privileged(&self) -> Result<bool, String> {
        self.fake.privileged()
    }
    fn account_dir(&self) -> Result<String, String> {
        self.fake.account_dir()
    }
    fn program_data(&self) -> Result<String, String> {
        self.fake.program_data()
    }
    fn state(&self, path: &str) -> State {
        self.fake.state(path)
    }
    fn entries(&self, dir: &str, limit: usize) -> io::Result<Vec<String>> {
        self.fake.entries(dir, limit)
    }
    fn trusted_dir(&self, path: &str) -> Result<(), String> {
        self.fake.trusted_dir(path)
    }
    fn trusted_file(&self, path: &str) -> Result<(), String> {
        self.fake.trusted_file(path)
    }
    fn readable_file(&self, path: &str) -> io::Result<()> {
        self.fake.readable_file(path)
    }
    fn receipt(&self, path: &str) -> Result<Option<Receipt>, ReceiptError> {
        self.fake.receipt(path)
    }
    fn canonical(&self, path: &str) -> io::Result<String> {
        self.aliases
            .get(path)
            .cloned()
            .map_or_else(|| self.fake.canonical(path), Ok)
    }
    fn create_machine_dir(&mut self, path: &str) -> io::Result<()> {
        self.fake.create_machine_dir(path)
    }
    fn in_container(&self) -> bool {
        self.container
    }
}

fn linked_probe() -> LinkedProbe {
    let mut fake = Fake::new(Platform::Linux, true);
    fake.file("/opt/system/docker");
    fake.file("/usr/bin/docker");
    LinkedProbe {
        fake,
        aliases: BTreeMap::from([("/usr/bin/docker".into(), "/opt/system/docker".into())]),
        container: false,
    }
}

#[test]
fn trusted_helper_links_keep_the_candidate_as_argv_zero() {
    let mut probe = linked_probe();
    probe
        .aliases
        .insert("/usr/bin".into(), "/opt/system".into());
    // The fake refuses a link itself, but accepts its root-controlled target.
    probe.fake.untrust("/usr/bin/docker");
    let plan = privileged_helper_plan(&probe, "/usr/bin/docker".into(), None).unwrap();
    assert_eq!(plan.path, Path::new("/usr/bin/docker"));
    assert_eq!(plan.environment, HelperEnvironment::Unix);
    let command = plan.command();
    assert_eq!(command.get_program(), "/usr/bin/docker");
    assert_eq!(command.get_current_dir(), Some(Path::new("/")));
    let environment: Vec<_> = command.get_envs().collect();
    assert_eq!(
        environment,
        vec![
            (
                std::ffi::OsStr::new("LC_ALL"),
                Some(std::ffi::OsStr::new("C"))
            ),
            (
                std::ffi::OsStr::new("PATH"),
                Some(std::ffi::OsStr::new("/usr/sbin:/usr/bin:/sbin:/bin"))
            ),
        ]
    );
}

#[test]
fn privileged_helpers_refuse_untrusted_link_targets_and_parents() {
    for untrusted in ["/opt/system/docker", "/usr/bin"] {
        let mut probe = linked_probe();
        probe.fake.untrust(untrusted);
        let error = helper_plan_with("docker", &probe, None, None, &|_| {
            Ok(("/usr/bin/docker".into(), None))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("/usr/bin/docker"));
        assert!(error.to_string().contains(untrusted));
    }
}

#[test]
fn a_helper_privilege_probe_error_uses_the_restricted_plan() {
    let mut probe = linked_probe();
    probe.fake.privileged = Err("token unavailable".into());
    let plan = helper_plan_with("docker", &probe, None, None, &|_| {
        Ok(("/usr/bin/docker".into(), None))
    })
    .unwrap();
    assert_eq!(plan.environment, HelperEnvironment::Unix);
    probe.fake.untrust("/opt/system/docker");
    assert!(
        helper_plan_with("docker", &probe, None, None, &|_| {
            Ok(("/usr/bin/docker".into(), None))
        })
        .is_err()
    );
}

#[test]
fn windows_helper_plans_check_the_exact_exe_and_restrict_path_and_cwd() {
    let mut probe = Fake::new(Platform::Windows, true);
    probe.file("C:/Windows/System32/docker.exe");
    for program in ["docker", "docker.exe"] {
        let plan = helper_plan_with(program, &probe, None, None, &|name| {
            assert_eq!(name, "docker.exe");
            Ok((
                "C:/Windows/System32/docker.exe".into(),
                Some("C:/Windows".into()),
            ))
        })
        .unwrap();
        assert_eq!(plan.path, Path::new("C:/Windows/System32/docker.exe"));
        assert_eq!(
            plan.environment,
            HelperEnvironment::Windows {
                system_dir: "C:\\Windows\\System32".into(),
                path: "C:\\Windows\\System32;C:/Windows;C:\\Windows\\System32\\Wbem".into(),
            }
        );
        let command = plan.command();
        assert_eq!(command.get_program(), "C:/Windows/System32/docker.exe");
        assert_eq!(
            command.get_current_dir(),
            Some(Path::new("C:\\Windows\\System32"))
        );
        assert_eq!(command.get_envs().count(), 1);
    }
    probe.untrust("C:/Windows/System32/docker.exe");
    assert!(
        privileged_helper_plan(
            &probe,
            "C:/Windows/System32/docker.exe".into(),
            Some("C:/Windows")
        )
        .is_err()
    );
}

#[test]
fn unprivileged_and_container_helper_plans_keep_path_search_and_environment() {
    let dir = crate::scope::runtime_test_probe::scratch_dir();
    let name = format!("fake-helper{}", std::env::consts::EXE_SUFFIX);
    let helper = dir.path().join(&name);
    std::fs::write(&helper, "never executed").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = std::env::join_paths([dir.path()]).unwrap();
    for container in [false, true] {
        let probe = LinkedProbe {
            fake: Fake::new(Platform::HOST.unwrap(), container),
            aliases: BTreeMap::new(),
            container,
        };
        let plan = helper_plan_with(&name, &probe, Some(&path), None, &|_| {
            panic!("inherited helper plans must not resolve a system helper")
        })
        .unwrap();
        assert_eq!(plan.path, helper);
        assert_eq!(plan.environment, HelperEnvironment::Inherited);
        assert_eq!(plan.command().get_envs().count(), 0);
        assert_eq!(plan.command().get_current_dir(), None);
    }
}
