//! open-ferry additions: both layouts and comment-preserving config writes.
use super::save::render_preserving_comments;
use super::*;

#[test]
fn ingress_limits_load_and_save_in_both_layouts_with_comments_and_explicit_defaults() {
    for v8 in [false, true] {
        let section = "ingress-limits:\n  # admission comment\n  max-connections: 7 # inline comment\n  header-timeout-seconds: 1\n  tls-handshake-timeout-seconds: 2\n  idle-timeout-seconds: 3\n  body-idle-timeout-seconds: 4\n  body-total-timeout-seconds: 5\n  buffered-body-bytes: 6\n  write-stall-timeout-seconds: -1\n";
        let source = if v8 {
            format!(
                "config-version: 8\nserver:\n{}",
                section
                    .lines()
                    .map(|line| format!("  {line}\n"))
                    .collect::<String>()
            )
        } else {
            section.to_owned()
        };
        let mut config = Config::parse(&source).unwrap();
        assert_eq!(config.ingress_limits.max_connections, 7);
        assert_eq!(config.ingress_limits.body_total_timeout_seconds, 5);
        assert_eq!(config.ingress_limits.write_stall_timeout_seconds, -1);
        config.ingress_limits.header_timeout_seconds = 0;
        config.ingress_limits.max_connections = 9;
        let out = render_preserving_comments(source.as_bytes(), &config, false).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("# admission comment"), "{out}");
        assert!(out.contains("max-connections: 9 # inline comment"), "{out}");
        assert!(out.contains("header-timeout-seconds: 0"), "{out}");
        assert!(!out.contains("# ingress-limits:"), "{out}");
        assert_eq!(
            Config::parse(&out).unwrap().ingress_limits,
            config.ingress_limits
        );
        if v8 {
            v8_edit::validate_v8_config(out.as_bytes()).unwrap();
        }
    }
}

#[test]
fn ingress_limits_v8_values_override_legacy_and_absent_defaults_are_not_added() {
    let config = Config::parse(
        "ingress-limits: {max-connections: 7}\nserver: {ingress-limits: {max-connections: 0}}\n",
    )
    .unwrap();
    assert_eq!(config.ingress_limits.max_connections, 0);
    for source in ["port: 0\n", "config-version: 8\nserver: {port: 0}\n"] {
        let config = Config::parse(source).unwrap();
        assert_eq!(config.ingress_limits, IngressLimits::default());
        let out = render_preserving_comments(source.as_bytes(), &config, false).unwrap();
        assert!(!String::from_utf8(out).unwrap().contains("ingress-limits"));
    }
}
