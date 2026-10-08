//! Not upstream's: the findings for open-ferry's
//! `management.separate-address`.

use super::*;

/// The findings for a good setup whose management lines are `management`
/// and server lines `server`.
async fn findings_with(
    setup: &Setup,
    server: &str,
    management: &str,
    env: &Environment,
) -> Vec<Finding> {
    setup.write(
        server,
        "[\"client-key\"]",
        &format!("  secret-key: \"m\"\n{management}"),
        "",
    );
    run(&setup.path(), env).await
}

#[tokio::test]
async fn checks_the_management_address() {
    let setup = Setup::new();
    let (_socket, free) = closed_port();
    let findings = findings_with(
        &setup,
        "",
        &format!("  separate-address: \"127.0.0.1:{free}\"\n"),
        &env(),
    )
    .await;
    assert_eq!(
        levels(&findings),
        [
            (Level::Ok, "config"),
            (Level::Ok, "client keys"),
            (Level::Ok, "management key"),
            (Level::Ok, "auth directory"),
            (Level::Ok, "address"),
            (Level::Ok, "management address"),
            (Level::Ok, "dashboard"),
            (Level::Ok, "clock"),
            (Level::Ok, "self-update"),
        ],
        "{findings:#?}"
    );
    assert_eq!(
        find(&findings, "management address").message,
        format!(
            "nothing listens on 127.0.0.1:{free}; the management API and the dashboard are \
             served on 127.0.0.1:{free} alone, not on server.port (a change takes a restart)"
        )
    );
    assert_eq!(
        find(&findings, "dashboard").message,
        format!("built in, at http://127.0.0.1:{free}/dashboard/, on the management address alone")
    );
    assert_eq!(exit_code(&findings), ExitCode::SUCCESS);
}

#[tokio::test]
async fn a_busy_management_port_is_an_error() {
    let setup = Setup::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let busy = listener.local_addr().unwrap().port();
    for address in [
        format!("127.0.0.1:{busy}"),
        format!(":{busy}"),
        format!("localhost:{busy}"),
    ] {
        let findings = findings_with(
            &setup,
            "",
            &format!("  separate-address: \"{address}\"\n"),
            &env(),
        )
        .await;
        let finding = find(&findings, "management address");
        assert_eq!(finding.level, Level::Error, "{address}: {finding:?}");
        assert_eq!(
            finding.message,
            format!("something already listens on 127.0.0.1:{busy}"),
            "{address}"
        );
        assert!(
            finding.fix.contains("management.separate-address"),
            "{address}"
        );
        // The proxy's own address is fine.
        assert_eq!(find(&findings, "address").level, Level::Ok, "{address}");
        assert_eq!(exit_code(&findings), ExitCode::FAILURE);
    }
    drop(listener);
}

#[tokio::test]
async fn the_proxys_port_or_a_bad_address_fails_the_load() {
    let setup = Setup::new();
    let port = setup.port;
    for (address, message) in [
        (
            format!("127.0.0.1:{port}"),
            format!("port {port} is server.port's; the management address needs a port of its own"),
        ),
        (
            "http://127.0.0.1:8318".to_owned(),
            "\"http://127.0.0.1:8318\" is a URL; write it as host:port, such as 127.0.0.1:8318"
                .to_owned(),
        ),
    ] {
        let findings = findings_with(
            &setup,
            "",
            &format!("  separate-address: \"{address}\"\n"),
            &env(),
        )
        .await;
        assert_eq!(levels(&findings), [(Level::Error, "config")], "{address}");
        assert!(
            findings[0]
                .message
                .contains(&format!("management.separate-address: {message}")),
            "{address}: {findings:#?}"
        );
    }
}

#[tokio::test]
async fn names_and_other_machines_are_not_connected_to() {
    let setup = Setup::new();
    for (address, what) in [
        ("admin.invalid:9", "is a host name"),
        ("192.0.2.1:9", "isn't a loopback address"),
    ] {
        let findings = findings_with(
            &setup,
            "",
            &format!("  separate-address: \"{address}\"\n  allow-remote: true\n"),
            &env(),
        )
        .await;
        let finding = find(&findings, "management address");
        assert_eq!(finding.level, Level::Warning, "{address}");
        assert_eq!(
            finding.message,
            format!(
                "management.separate-address host {} {what}, so whether port 9 is free there \
                 isn't checked (check makes no network call)",
                address.trim_end_matches(":9")
            )
        );
        assert!(
            !findings
                .iter()
                .any(|finding| finding.check == "management access"),
            "{address}: {findings:#?}"
        );
    }
}

#[tokio::test]
async fn warns_when_allow_remote_does_not_fit() {
    let setup = Setup::new();
    let (_socket, free) = closed_port();
    let management = format!("  separate-address: \":{free}\"\n");
    let findings = findings_with(&setup, "", &management, &env()).await;
    let access = find(&findings, "management access");
    assert_eq!(access.level, Level::Warning);
    assert_eq!(
        access.message,
        format!(
            "management.separate-address :{free} takes connections from other machines, but \
             management.allow-remote is false, so the management API and the dashboard refuse them"
        )
    );
    assert_eq!(
        access.fix,
        format!(
            "set management.allow-remote to true to manage the proxy from other machines, or \
             listen on loopback only, such as 127.0.0.1:{free}"
        )
    );

    let management = format!("  separate-address: \"127.0.0.1:{free}\"\n  allow-remote: true\n");
    let findings = findings_with(&setup, "", &management, &env()).await;
    let access = find(&findings, "management access");
    assert_eq!(access.level, Level::Warning);
    assert_eq!(
        access.message,
        format!(
            "management.allow-remote is true, but management.separate-address 127.0.0.1:{free} \
             is on loopback and server.trusted-proxies is empty, so no other machine can reach \
             it and allow-remote does nothing"
        )
    );
}

#[test]
fn judges_allow_remote_by_the_address() {
    const REFUSED: &str = "takes connections from other machines";
    const IDLE: &str = "allow-remote does nothing";
    let with_password = Environment {
        management_password: true,
        ..env()
    };
    for (address, allow_remote, trusted, password, want) in [
        // Other machines can connect, and allow-remote refuses them,
        ("192.0.2.1:9", false, false, false, Some(REFUSED)),
        (":9", false, false, false, Some(REFUSED)),
        ("0.0.0.0:9", false, false, false, Some(REFUSED)),
        ("[::]:9", false, false, false, Some(REFUSED)),
        ("[2001:db8::1]:9", false, false, false, Some(REFUSED)),
        // unless it is true or MANAGEMENT_PASSWORD is set, which lets
        // remote clients in too.
        ("192.0.2.1:9", true, false, false, None),
        (":9", false, false, true, None),
        // Loopback, where allow-remote does nothing,
        ("127.0.0.1:9", true, false, false, Some(IDLE)),
        ("[::1]:9", true, false, false, Some(IDLE)),
        ("localhost:9", true, false, false, Some(IDLE)),
        // unless a reverse proxy on this machine forwards other clients.
        ("127.0.0.1:9", true, true, false, None),
        ("127.0.0.1:9", false, false, false, None),
        ("127.0.0.1:9", false, false, true, None),
        // A host name isn't judged.
        ("admin.internal:9", false, false, false, None),
        ("admin.internal:9", true, false, false, None),
    ] {
        let mut config = listening("127.0.0.1", 8317);
        config.remote_management.separate_address = address.to_owned();
        config.remote_management.allow_remote = allow_remote;
        if trusted {
            config.trusted_proxies = vec!["127.0.0.1".to_owned()];
        }
        let env = if password { &with_password } else { &env() };
        let parsed = config
            .remote_management
            .separate_address()
            .unwrap()
            .unwrap();
        let finding = management::access_finding(&config, env, &parsed);
        let what = format!(
            "{address} allow-remote {allow_remote}, trusted {trusted}, password {password}"
        );
        match want {
            Some(text) => {
                let finding = finding.unwrap_or_else(|| panic!("{what}: no finding"));
                assert_eq!(finding.level, Level::Warning, "{what}");
                assert_eq!(finding.check, "management access", "{what}");
                assert!(
                    finding.message.contains(text),
                    "{what}: {}",
                    finding.message
                );
            }
            None => assert_eq!(finding, None, "{what}"),
        }
    }
}

#[test]
fn gives_the_dashboard_url_on_the_management_address() {
    let mut config = listening("127.0.0.1", 8317);
    assert_eq!(management::dashboard_url(&config), None);
    for (address, tls, want) in [
        ("127.0.0.1:8318", false, "http://127.0.0.1:8318/dashboard/"),
        (":8318", true, "https://127.0.0.1:8318/dashboard/"),
        ("[::]:8318", false, "http://127.0.0.1:8318/dashboard/"),
        ("[::1]:8318", false, "http://[::1]:8318/dashboard/"),
        (
            "admin.internal:8318",
            true,
            "https://admin.internal:8318/dashboard/",
        ),
    ] {
        config.remote_management.separate_address = address.to_owned();
        config.tls.enable = tls;
        assert_eq!(
            management::dashboard_url(&config).as_deref(),
            Some(want),
            "{address}"
        );
    }
}
