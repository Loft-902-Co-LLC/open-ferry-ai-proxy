//! Reviewed dependencies of each whole hashed source file, excluding test code.
use super::*;
use proc_macro2::{TokenStream, TokenTree};
use syn::{
    Item, UseTree,
    visit::{self, Visit},
};

fn allowed(file: &str) -> BTreeSet<String> {
    let (imports, modules, paths): (&[&str], &str, &[&str]) = match file {
        "tools/parity/src/generate/config_save.rs" => (
            &[
                "std::collections::BTreeMap",
                "serde_json::Map",
                "serde_json::Value",
                "serde_json::json",
                "super::config_diff::Configs",
                "super::config_diff::yaml",
                "super::Generator",
                "super::Rng",
                "crate::cases::Case",
            ],
            "",
            &[],
        ),
        "tools/parity/src/generate/config_diff.rs" => (
            &[
                "serde_json::Map",
                "serde_json::Value",
                "serde_json::json",
                "super::Generator",
                "super::Rng",
                "crate::cases::Case",
            ],
            "",
            &[],
        ),
        // These whole files also contain unrelated suites. Their existing
        // imports/modules are reviewed over-inclusion, not random call edges.
        "tools/parity/src/generate.rs" => (
            &[
                "open_ferry_translate::json::exact",
                "serde_json::Value",
                "serde_json::json",
                "crate::cases::Case",
            ],
            "chat claude_chat claude_native claude_responses codex_models completions config_diff config_save gemini gemini_responses interactions multi_agent openai_chat openai_claude openai_responses payload quota_signals registry response responses session signature thinking to_gemini ttft usage",
            &["crate::compare::Numbers::Respelled"],
        ),
        "tools/parity/src/cases.rs" => (
            &[
                "base64::Engine as _",
                "base64::engine::general_purpose::URL_SAFE",
                "open_ferry_translate::codex::claude::convert_claude_request_to_codex",
                "serde_json::Value",
                "serde_json::json",
            ],
            "chat claude_chat claude_responses codex_models completions config_diff config_save gemini gemini_responses multi_agent openai_chat openai_claude openai_responses payload quota_signals registry responses session signature signature_relay thinking to_gemini ttft usage",
            &[],
        ),
        _ => (&[], "", &[]),
    };
    imports
        .iter()
        .map(|path| format!("use {path}"))
        .chain(modules.split_whitespace().map(|name| format!("mod {name}")))
        .chain(paths.iter().map(|path| format!("path {path}")))
        .collect()
}

fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        matches!(&attr.meta, syn::Meta::List(list)
        if list.path.is_ident("cfg") && list.tokens.to_string() == "test")
    })
}

fn use_paths(tree: &UseTree, prefix: &str, out: &mut Vec<String>) {
    match tree {
        UseTree::Path(path) => use_paths(&path.tree, &format!("{prefix}{}::", path.ident), out),
        UseTree::Name(name) => out.push(format!("{prefix}{}", name.ident)),
        UseTree::Rename(name) => out.push(format!("{prefix}{} as {}", name.ident, name.rename)),
        UseTree::Glob(_) => out.push(format!("{prefix}*")),
        UseTree::Group(group) => {
            for item in &group.items {
                use_paths(item, prefix, out);
            }
        }
    }
}

struct Dependencies<'a> {
    root: &'a Path,
    file: &'a str,
    hashed: &'a BTreeSet<PathBuf>,
    allowed: BTreeSet<String>,
    modules: BTreeSet<String>,
    errors: BTreeSet<String>,
}

impl Dependencies<'_> {
    fn require(&mut self, item: String) {
        if !self.allowed.contains(&item) {
            self.errors.insert(format!("source closure: {} has an unreviewed dependency `{item}`; review and hash its input closure", self.file));
        }
    }

    fn path(&mut self, path: &str) {
        let first = path.split("::").next().unwrap_or("");
        if path.contains("::")
            && (matches!(first, "crate" | "super") || self.modules.contains(first))
        {
            self.require(format!("path {path}"));
        }
        if path.starts_with("std::fs::")
            || path.starts_with("fs::")
            || path.starts_with("std::io::")
            || path.starts_with("File::")
        {
            self.require(format!("file access {path}"));
        }
    }

    // Rust macro arguments are token trees, not visited expressions. Inspect
    // paths in them too, while ignoring string literals containing sample code.
    fn tokens(&mut self, tokens: TokenStream) {
        let mut tokens = tokens.into_iter().peekable();
        while let Some(token) = tokens.next() {
            match token {
                TokenTree::Group(group) => self.tokens(group.stream()),
                TokenTree::Ident(ident) => {
                    let mut path = ident.to_string();
                    if matches!(path.as_str(), "use" | "mod") {
                        self.require(format!("macro {path}"));
                    }
                    loop {
                        let mut rest = tokens.clone();
                        if !matches!(rest.next(), Some(TokenTree::Punct(p)) if p.as_char() == ':')
                            || !matches!(rest.next(), Some(TokenTree::Punct(p)) if p.as_char() == ':')
                        {
                            break;
                        }
                        let Some(TokenTree::Ident(next)) = rest.next() else {
                            break;
                        };
                        path.push_str("::");
                        path.push_str(&next.to_string());
                        tokens = rest;
                    }
                    self.path(&path);
                    if matches!(path.as_str(), "include" | "include_str" | "include_bytes") {
                        self.require(format!("macro dependency {path}!"));
                    }
                }
                _ => {}
            }
        }
    }
}

impl<'ast> Visit<'ast> for Dependencies<'_> {
    fn visit_item(&mut self, item: &'ast Item) {
        let attrs = match item {
            Item::Fn(i) => &i.attrs,
            Item::Mod(i) => &i.attrs,
            Item::Use(i) => &i.attrs,
            Item::Const(i) => &i.attrs,
            Item::Static(i) => &i.attrs,
            Item::Impl(i) => &i.attrs,
            Item::Struct(i) => &i.attrs,
            Item::Enum(i) => &i.attrs,
            Item::Type(i) => &i.attrs,
            Item::Trait(i) => &i.attrs,
            Item::Macro(i) => &i.attrs,
            _ => {
                visit::visit_item(self, item);
                return;
            }
        };
        if !test_only(attrs) {
            visit::visit_item(self, item);
        }
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if !test_only(&call.attrs) {
            visit::visit_expr_call(self, call);
        }
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        let mut paths = Vec::new();
        use_paths(&item.tree, "", &mut paths);
        for path in paths {
            self.require(format!("use {path}"));
        }
        for attr in &item.attrs {
            self.visit_attribute(attr);
        }
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        self.require(format!("mod {}", item.ident));
        visit::visit_item_mod(self, item);
    }

    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
        if attr.path().is_ident("path") {
            self.require("#[path]".into());
        }
        visit::visit_attribute(self, attr);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        self.path(
            &path
                .segments
                .iter()
                .map(|part| part.ident.to_string())
                .collect::<Vec<_>>()
                .join("::"),
        );
        visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let name = mac.path.segments.last().unwrap().ident.to_string();
        if matches!(name.as_str(), "include" | "include_str" | "include_bytes") {
            if name == "include" {
                self.require("include!".into());
            }
            use syn::parse::Parser;
            let arguments =
                syn::punctuated::Punctuated::<syn::LitStr, syn::Token![,]>::parse_terminated
                    .parse2(mac.tokens.clone());
            match arguments {
                Ok(args) if args.len() == 1 => {
                    let target = self
                        .root
                        .join(self.file)
                        .parent()
                        .unwrap()
                        .join(args[0].value());
                    match target.canonicalize() {
                        Ok(target) if self.hashed.contains(&target) => {}
                        _ => {
                            self.errors.insert(format!(
                                "source closure: {} {name}! target is not fingerprinted: {}",
                                self.file,
                                target.display()
                            ));
                        }
                    }
                }
                _ => {
                    self.errors.insert(format!(
                        "source closure: {} {name}! requires a reviewed literal target",
                        self.file
                    ));
                }
            }
        } else {
            self.tokens(mac.tokens.clone());
        }
    }
}

pub(super) fn check(
    root: &Path,
    file: &str,
    source: &str,
    hashed: &BTreeSet<PathBuf>,
) -> std::result::Result<(), String> {
    let syntax =
        syn::parse_file(source).map_err(|error| format!("source closure: {file}: {error}"))?;
    let allowed = allowed(file);
    let modules = allowed
        .iter()
        .filter_map(|item| item.strip_prefix("mod ").map(str::to_owned))
        .collect();
    let mut dependencies = Dependencies {
        root,
        file,
        hashed,
        allowed,
        modules,
        errors: BTreeSet::new(),
    };
    dependencies.visit_file(&syntax);
    if dependencies.errors.is_empty() {
        Ok(())
    } else {
        Err(dependencies
            .errors
            .into_iter()
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

#[test]
fn new_runtime_dependencies_fail_with_a_closure_message() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap();
    let file = "tools/parity/src/generate/config_save.rs";
    for (source, item) in [
        (
            "use crate::cases::config_diff::ROOT_EXAMPLE;",
            "use crate::cases::config_diff::ROOT_EXAMPLE",
        ),
        ("mod fresh;", "mod fresh"),
        ("#[path = \"fresh.rs\"] mod fresh;", "#[path]"),
        ("include!(\"fresh.rs\");", "include!"),
        (
            "fn input() { std::fs::read_to_string(\"fresh.yaml\"); }",
            "file access std::fs::read_to_string",
        ),
        (
            "fn input() { json!({\"file\": std::fs::read(\"fresh.yaml\")}); }",
            "file access std::fs::read",
        ),
    ] {
        let error = check(root, file, source, &BTreeSet::new()).unwrap_err();
        assert!(
            error.contains("source closure") && error.contains(item),
            "{error}"
        );
        assert!(!error.contains("relabel"), "{error}");
    }
    check(
        root,
        file,
        "#[cfg(test)] mod tests { use std::fs; mod fresh; include!(\"fresh.rs\"); }",
        &BTreeSet::new(),
    )
    .unwrap();
}
