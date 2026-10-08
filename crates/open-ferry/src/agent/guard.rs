//! Which changes need a confirmation, and asking for it.
//!
//! These need `--yes` (`confirm: true` for a tool): deleting a credential
//! or a client key, showing a secret, replacing the whole config, and a
//! change to a sensitive setting:
//! - `management.allow-remote`, `management.secret-key` and
//!   `management.separate-address`, which decide who can reach the
//!   management API;
//! - `server.host` set to an address that isn't loopback, or unset, so
//!   the proxy listens beyond this machine;
//! - `server.tls`, which decides how clients connect;
//! - `server.trusted-proxies`, which decides whose forwarded addresses
//!   are believed, and so which clients count as local;
//! - removing the last client key in `access.api-keys`: the server trims
//!   the keys and drops empty and repeated ones, and so does this count, so
//!   a list of blank keys counts as none.
//!
//! Without the confirmation, a command asks at a terminal; with no
//! terminal to ask on, it changes nothing and says what it would change.
//! Either is scrubbed of every secret known to the setup and of the
//! configs the change goes from and to, before it is asked or returned.

use std::net::IpAddr;

use serde_json::{Value, json};

use super::mask::{Scrub, collect_secrets, known_secrets};
use super::values::{Change, get};
use super::{Caller, Context, Failure};

/// The settings a change to which needs a confirmation, each with why.
/// `server.host` needs one only when unset or not loopback, and its reason
/// is worked out from the new host. The tool descriptions in `mcp.rs` name
/// each (a test checks it).
pub(crate) const SENSITIVE_SETTINGS: [(&str, &str); 6] = [
    (
        "management.allow-remote",
        "management.allow-remote decides whether the management API answers other machines",
    ),
    (
        "management.secret-key",
        "management.secret-key is the key to the management API",
    ),
    (
        "management.separate-address",
        "management.separate-address moves the management API to another address",
    ),
    ("server.host", ""),
    (
        "server.tls",
        "server.tls decides how clients connect to the proxy",
    ),
    (
        "server.trusted-proxies",
        "server.trusted-proxies decides whose forwarded addresses are believed, and so which clients count as local to the management API",
    ),
];

/// Why a tool call that reads a value or a config from a file
/// (`from_file`) needs `confirm: true`: an agent could otherwise copy a
/// file the user didn't mean into the config.
pub(crate) const READS_A_FILE: &str = "it reads a file into the config";

/// The client keys in the config tree `root`, counted as the server counts
/// them: each trimmed, without empty or repeated ones.
pub(crate) fn client_keys(root: &Value) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    let listed = get(root, &["access".to_owned(), "api-keys".to_owned()]).and_then(Value::as_array);
    for entry in listed.into_iter().flatten() {
        let text = match entry {
            Value::String(text) => text.trim().to_owned(),
            Value::Number(number) => number.to_string(),
            Value::Bool(flag) => flag.to_string(),
            _ => continue,
        };
        if !text.is_empty() && !keys.contains(&text) {
            keys.push(text);
        }
    }
    keys
}

/// Why `changes`, which make the config `before` into `after`, need a
/// confirmation: one reason for each sensitive setting they touch.
pub(crate) fn sensitive_reasons(changes: &[Change], before: &Value, after: &Value) -> Vec<String> {
    sensitive_reasons_of(changes, before, after, false)
}

/// [`sensitive_reasons`]; with `hidden` when the values set were read from
/// a file or standard input, so no reason names one.
pub(crate) fn sensitive_reasons_of(
    changes: &[Change],
    before: &Value,
    after: &Value,
    hidden: bool,
) -> Vec<String> {
    let mut reasons = Vec::new();
    let mut add = |reason: String| {
        if !reasons.contains(&reason) {
            reasons.push(reason);
        }
    };
    for change in changes {
        let path = change.path.as_str();
        let found = SENSITIVE_SETTINGS.iter().find(|(setting, _)| {
            path == *setting
                || path
                    .strip_prefix(setting)
                    .is_some_and(|rest| rest.starts_with('.'))
        });
        let Some((setting, why)) = found else {
            continue;
        };
        if *setting != "server.host" {
            add((*why).to_owned());
            continue;
        }
        let host = change
            .new
            .as_ref()
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !is_loopback_host(host) {
            add(if host.trim().is_empty() {
                "server.host would be unset, so the proxy listens on every interface, beyond this machine".to_owned()
            } else if hidden {
                "server.host would be a host that isn't loopback, so the proxy listens beyond this machine".to_owned()
            } else {
                format!(
                    "server.host would be {host}, which isn't loopback, so the proxy listens beyond this machine"
                )
            });
        }
    }
    if !client_keys(before).is_empty() && client_keys(after).is_empty() {
        add(
            "it removes the last client key in access.api-keys, so the proxy serves every client without a key"
                .to_owned(),
        );
    }
    reasons
}

/// Whether `host` is loopback: `localhost`, or a loopback address.
pub(crate) fn is_loopback_host(host: &str) -> bool {
    let host = host.trim();
    let bare = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    bare.eq_ignore_ascii_case("localhost")
        || bare.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Goes ahead with `what`, which needs a confirmation for `reasons`, when
/// it has one: `--yes`, or a yes at the terminal. Else a failure that says
/// what it would change, `would`, which holds its `changes`, masked.
///
/// What it asks, and the failure, are scrubbed of the secrets known to the
/// setup ([`known_secrets`]) and of those in `trees`, the configs the
/// change goes from and to, as JSON: masking misses a secret that shows
/// where no key names it, as a key in a URL's path, and one only in the
/// config a change would make is known nowhere else.
pub(crate) fn confirm(
    ctx: &Context,
    what: &str,
    reasons: &[String],
    would: Value,
    trees: &[&Value],
) -> Result<(), Failure> {
    if ctx.yes {
        return Ok(());
    }
    let mut secrets = known_secrets(ctx);
    for tree in trees {
        collect_secrets(tree, &mut secrets);
    }
    let scrub = Scrub::new(&secrets, &[]);
    let why = if reasons.is_empty() {
        String::new()
    } else {
        format!(": {}", reasons.join("; "))
    };
    if let Some(ask) = &ctx.ask {
        let mut question = format!("{what} needs a confirmation{why}.\n");
        if let Some(changes) = would.get("changes").and_then(Value::as_array)
            && !changes.is_empty()
        {
            question.push_str("It would change:\n");
            for change in changes {
                question.push_str(&super::values::change_line(change));
            }
        }
        question.push_str("Go ahead? [y/N] ");
        return if ask(&scrub.text(question)) {
            Ok(())
        } else {
            Err(Failure::new("declined", "Declined; nothing was changed."))
        };
    }
    let flag = ctx.confirm_flag();
    let mut would = scrub.json(would);
    if let Value::Object(map) = &mut would {
        map.insert("reasons".to_owned(), scrub.json(json!(reasons)));
    }
    let hint = format!("to go ahead, {}", go_ahead(ctx, &would));
    Err(Failure::new(
        "needs_confirmation",
        scrub.text(format!("{what} needs {flag}{why}. Nothing was changed.")),
    )
    .hint(hint)
    .would(would))
}

/// How to go ahead with a change that needs a confirmation: run it again
/// with it, and with the SHA-256 of the config file it was worked out
/// from when `would` has one, so it is made only to that file.
pub(crate) fn go_ahead(ctx: &Context, would: &Value) -> String {
    let flag = ctx.confirm_flag();
    match (
        ctx.caller,
        would.get("config_sha256").and_then(Value::as_str),
    ) {
        (Caller::Cli, Some(sha256)) => {
            format!("run it again with {flag} --expect-sha256 {sha256}")
        }
        (Caller::Mcp, Some(sha256)) => {
            format!("run it again with {flag} and expect_sha256: \"{sha256}\"")
        }
        (_, None) => format!("run it again with {flag}"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::agent::values::diff;

    fn reasons(before: Value, after: Value) -> Vec<String> {
        sensitive_reasons(&diff(&before, &after), &before, &after)
    }

    // Not upstream's: the sensitive settings, and the ones that aren't.
    #[test]
    fn finds_sensitive_changes() {
        let base =
            json!({"server": {"host": "127.0.0.1", "port": 1}, "access": {"api-keys": ["a"]}});
        assert!(
            reasons(
                base.clone(),
                json!({"server": {"host": "127.0.0.1", "port": 2}, "access": {"api-keys": ["a"]}})
            )
            .is_empty()
        );
        assert!(
            reasons(
                base.clone(),
                json!({"server": {"host": "::1", "port": 1}, "access": {"api-keys": ["a"]}})
            )
            .is_empty()
        );
        assert!(
            reasons(
                base.clone(),
                json!({"server": {"host": "localhost", "port": 1}, "access": {"api-keys": ["a"]}})
            )
            .is_empty()
        );
        let open = reasons(
            base.clone(),
            json!({"server": {"host": "0.0.0.0", "port": 1}, "access": {"api-keys": ["a"]}}),
        );
        assert_eq!(open.len(), 1);
        assert!(open[0].contains("isn't loopback"));
        let unset = reasons(
            base.clone(),
            json!({"server": {"port": 1}, "access": {"api-keys": ["a"]}}),
        );
        assert!(unset[0].contains("every interface"));
        let last = reasons(
            base.clone(),
            json!({"server": {"host": "127.0.0.1", "port": 1}, "access": {"api-keys": []}}),
        );
        assert!(last[0].contains("last client key"));
        for path in ["allow-remote", "secret-key", "separate-address"] {
            let mut after = base.clone();
            after["management"] = json!({path: "x"});
            assert_eq!(reasons(base.clone(), after).len(), 1, "{path}");
        }
        let mut after = base.clone();
        after["server"]["tls"] = json!({"enable": true});
        assert!(reasons(base.clone(), after)[0].contains("server.tls"));
        let mut after = base.clone();
        after["server"]["trusted-proxies"] = json!(["10.0.0.0/8"]);
        assert!(reasons(base, after)[0].contains("server.trusted-proxies"));
    }

    // Not upstream's: a list of keys that are blank, or only repeat one
    // another, counts as the server counts it.
    #[test]
    fn counts_client_keys_as_the_server_does() {
        let keys = |list: Value| client_keys(&json!({"access": {"api-keys": list}}));
        assert!(keys(json!([""])).is_empty());
        assert!(keys(json!(["  ", "\t"])).is_empty());
        assert!(keys(json!([null, {}, []])).is_empty());
        assert_eq!(keys(json!(["a", " a ", "a"])), ["a"]);
        assert_eq!(keys(json!(["a", "b"])), ["a", "b"]);
        assert!(client_keys(&json!({"access": {}})).is_empty());
        assert!(client_keys(&json!({})).is_empty());

        let base = json!({"access": {"api-keys": ["a"]}});
        for after in [
            json!({"access": {"api-keys": [""]}}),
            json!({"access": {"api-keys": ["  "]}}),
            json!({"access": {}}),
            json!({}),
        ] {
            let found = reasons(base.clone(), after.clone());
            assert!(
                found
                    .iter()
                    .any(|reason| reason.contains("last client key")),
                "{after}"
            );
        }
        assert!(reasons(base.clone(), json!({"access": {"api-keys": ["a", "a"]}})).is_empty());
        assert!(reasons(base, json!({"access": {"api-keys": ["b"]}})).is_empty());
        // A list that had no real key loses none.
        assert!(reasons(json!({"access": {"api-keys": [""]}}), json!({})).is_empty());
    }

    // Not upstream's: each sensitive setting gives a reason.
    #[test]
    fn every_sensitive_setting_gives_a_reason() {
        let base = json!({"server": {"host": "127.0.0.1"}});
        for (setting, _) in SENSITIVE_SETTINGS {
            let (parent, name) = setting.split_once('.').unwrap();
            let mut after = base.clone();
            after[parent][name] = json!("192.0.2.1");
            assert_eq!(reasons(base.clone(), after).len(), 1, "{setting}");
        }
    }
}
