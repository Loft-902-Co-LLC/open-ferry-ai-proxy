//! Catch implementations outside the hash that could extend a hashed type.
use super::*;

fn module(file: &str) -> String {
    let file = file.strip_prefix("tools/parity/src/").unwrap();
    let file = file.strip_suffix(".rs").unwrap();
    let mut parts: Vec<_> = file.split('/').collect();
    if matches!(parts.last(), Some(&"main" | &"mod")) {
        parts.pop();
    }
    std::iter::once("crate")
        .chain(parts)
        .collect::<Vec<_>>()
        .join("::")
}

fn resolve(path: &str, module: &str, bindings: &BTreeMap<String, String>) -> String {
    let mut parts = path.split("::").peekable();
    let mut prefix: Vec<_> = module.split("::").map(str::to_owned).collect();
    match parts.peek().copied() {
        Some("crate") => {
            prefix.clear();
        }
        Some("self") => {
            parts.next();
        }
        Some("super") => {
            while parts.peek() == Some(&"super") {
                prefix.pop();
                parts.next();
            }
        }
        Some(first) if bindings.contains_key(first) => {
            prefix = bindings[first].split("::").map(str::to_owned).collect();
            parts.next();
        }
        _ => {}
    }
    prefix.extend(parts.map(str::to_owned));
    prefix.join("::")
}

fn bindings(items: &[Item], module: &str) -> BTreeMap<String, String> {
    let mut names = BTreeMap::new();
    for item in items {
        let name = match item {
            Item::Struct(i) if !test_only(&i.attrs) => Some(&i.ident),
            Item::Enum(i) if !test_only(&i.attrs) => Some(&i.ident),
            Item::Union(i) if !test_only(&i.attrs) => Some(&i.ident),
            Item::Type(i) if !test_only(&i.attrs) => Some(&i.ident),
            _ => None,
        };
        if let Some(name) = name {
            names.insert(name.to_string(), format!("{module}::{name}"));
        }
    }
    for item in items {
        if let Item::Use(item) = item
            && !test_only(&item.attrs)
        {
            let mut paths = Vec::new();
            use_paths(&item.tree, "", &mut paths);
            for path in paths {
                let (path, alias) = path
                    .split_once(" as ")
                    .map_or((path.as_str(), None), |(p, a)| (p, Some(a)));
                let name = alias.unwrap_or_else(|| path.rsplit("::").next().unwrap());
                let target = resolve(path, module, &names);
                names.insert(
                    if name == "*" {
                        format!("*{target}")
                    } else {
                        name.to_owned()
                    },
                    target,
                );
            }
        }
    }
    names
}

struct Impls<'a> {
    file: &'a str,
    module: String,
    bindings: BTreeMap<String, String>,
    types: &'a BTreeSet<String>,
    errors: Vec<String>,
}

impl<'ast> Visit<'ast> for Impls<'_> {
    fn visit_item(&mut self, item: &'ast Item) {
        let attrs = match item {
            Item::Mod(i) => &i.attrs,
            Item::Impl(i) => &i.attrs,
            Item::Fn(i) => &i.attrs,
            _ => {
                visit::visit_item(self, item);
                return;
            }
        };
        if !test_only(attrs) {
            visit::visit_item(self, item);
        }
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if let Some((_, items)) = &item.content {
            let nested = format!("{}::{}", self.module, item.ident);
            let module = std::mem::replace(&mut self.module, nested);
            let names = std::mem::replace(&mut self.bindings, bindings(items, &self.module));
            for item in items {
                self.visit_item(item);
            }
            self.module = module;
            self.bindings = names;
        }
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if let syn::Type::Path(ty) = &*item.self_ty {
            let path = ty
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect::<Vec<_>>()
                .join("::");
            let mut target = resolve(&path, &self.module, &self.bindings);
            if !self.bindings.contains_key(path.split("::").next().unwrap()) {
                for glob in self
                    .bindings
                    .values()
                    .filter_map(|value| value.strip_suffix("::*"))
                {
                    let candidate = format!("{glob}::{path}");
                    if self.types.contains(&candidate) {
                        target = candidate;
                        break;
                    }
                }
            }
            if self.types.contains(&target) {
                // Existing suite helpers are private except the four reviewed
                // signature helpers below. The random path uses Generator::new
                // only; check_signature_boundary also protects that boundary.
                let private_suite = matches!(
                    self.file,
                    "tools/parity/src/generate/codex_models.rs"
                        | "tools/parity/src/generate/multi_agent.rs"
                        | "tools/parity/src/generate/quota_signals.rs"
                        | "tools/parity/src/generate/session.rs"
                        | "tools/parity/src/generate/signature.rs"
                        | "tools/parity/src/generate/ttft.rs"
                        | "tools/parity/src/generate/usage.rs"
                );
                let safe = private_suite && target == "crate::generate::Generator"
                    && item.trait_.is_none()
                    && item.items.iter().all(|member| matches!(member, syn::ImplItem::Fn(f) if
                        matches!(f.vis, syn::Visibility::Inherited)
                        || (self.file == "tools/parity/src/generate/signature.rs"
                            && SIGNATURE_HELPERS.contains(&f.sig.ident.to_string().as_str())
                            && matches!(&f.vis, syn::Visibility::Restricted(v) if v.path.is_ident("super")))));
                if !safe {
                    let methods = item
                        .items
                        .iter()
                        .filter_map(|member| match member {
                            syn::ImplItem::Fn(f) => Some(f.sig.ident.to_string()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    self.errors.push(format!("source closure: {} has an unreviewed impl {target} ({methods}); review and hash its input closure", self.file));
                }
            }
        }
        visit::visit_item_impl(self, item);
    }
}

const SIGNATURE_HELPERS: &[&str] = &[
    "signature_text",
    "damage",
    "gpt_signature",
    "gemini_contents",
];

fn check_signature_boundary(file: &str, syntax: &syn::File) -> std::result::Result<(), String> {
    struct Calls(Vec<String>);
    impl<'ast> Visit<'ast> for Calls {
        fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
            if SIGNATURE_HELPERS.contains(&call.method.to_string().as_str()) {
                self.0.push(call.method.to_string());
            }
            visit::visit_expr_method_call(self, call);
        }
    }
    let mut calls = Calls(Vec::new());
    if matches!(
        file,
        "tools/parity/src/generate/config_save.rs" | "tools/parity/src/generate/config_diff.rs"
    ) {
        calls.visit_file(syntax);
    } else {
        for item in &syntax.items {
            if let Item::Impl(item) = item
                && let syn::Type::Path(ty) = &*item.self_ty
            {
                if ty.path.is_ident("Rng") {
                    calls.visit_item_impl(item);
                }
                for member in &item.items {
                    if let syn::ImplItem::Fn(f) = member
                        && ((ty.path.is_ident("Generator") && f.sig.ident == "new")
                            || (ty.path.is_ident("Case")
                                && matches!(
                                    f.sig.ident.to_string().as_str(),
                                    "new" | "with_options"
                                )))
                    {
                        calls.visit_impl_item_fn(f);
                    }
                }
            }
        }
    }
    if calls.0.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "source closure: {file} random path calls unhashed signature helper: {}",
            calls.0.join(", ")
        ))
    }
}

fn check_file(
    file: &str,
    source: &str,
    types: &BTreeSet<String>,
) -> std::result::Result<(), String> {
    let syntax = syn::parse_file(source).map_err(|e| format!("source closure: {file}: {e}"))?;
    let module = module(file);
    let mut scan = Impls {
        file,
        bindings: bindings(&syntax.items, &module),
        module,
        types,
        errors: Vec::new(),
    };
    scan.visit_file(&syntax);
    if scan.errors.is_empty() {
        Ok(())
    } else {
        Err(scan.errors.join("\n"))
    }
}

pub(in super::super) fn check_unhashed_impls(
    root: &Path,
    hashed: &BTreeSet<PathBuf>,
) -> std::result::Result<(), String> {
    let mut types = BTreeSet::new();
    for path in hashed {
        let syntax = syn::parse_file(&fs::read_to_string(path).unwrap()).unwrap();
        let file = path
            .strip_prefix(root.canonicalize().unwrap())
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let module = module(&file);
        check_signature_boundary(&file, &syntax)?;
        for item in syntax.items {
            let name = match item {
                Item::Struct(i) => Some(i.ident),
                Item::Enum(i) => Some(i.ident),
                Item::Union(i) => Some(i.ident),
                Item::Type(i) => Some(i.ident),
                _ => None,
            };
            if let Some(name) = name {
                types.insert(format!("{module}::{name}"));
            }
        }
    }
    fn scan(
        root: &Path,
        directory: &Path,
        hashed: &BTreeSet<PathBuf>,
        types: &BTreeSet<String>,
    ) -> std::result::Result<(), String> {
        for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.is_dir() {
                scan(root, &path, hashed, types)?;
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && !hashed.contains(&path.canonicalize().unwrap())
            {
                let file = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                check_file(
                    &file,
                    &fs::read_to_string(&path).map_err(|e| e.to_string())?,
                    types,
                )?;
            }
        }
        Ok(())
    }
    scan(root, &root.join("tools/parity/src"), hashed, &types)
}

#[test]
fn unhashed_methods_on_hashed_types_fail_with_the_item_name() {
    let types = BTreeSet::from(["crate::generate::Generator".into()]);
    for (file, source) in [
        (
            "codex_models",
            "use super::Generator; impl Generator { pub(super) fn root_example_line(&mut self) {} }",
        ),
        (
            "fresh",
            "impl crate::generate::Generator { fn root_example_line(&mut self) {} }",
        ),
        (
            "fresh",
            "use crate::generate::Generator as Alias; impl Alias { fn root_example_line(&mut self) {} }",
        ),
        (
            "fresh",
            "use super::*; impl Generator { fn root_example_line(&mut self) {} }",
        ),
    ] {
        let error = check_file(
            &format!("tools/parity/src/generate/{file}.rs"),
            source,
            &types,
        )
        .unwrap_err();
        assert!(
            error.contains("source closure")
                && error.contains("impl crate::generate::Generator")
                && error.contains("root_example_line"),
            "{error}"
        );
    }
    check_file(
        "tools/parity/src/generate/codex_models.rs",
        "use super::Generator; impl Generator { fn private_suite_method(&mut self) {} }",
        &types,
    )
    .unwrap();
    check_file(
        "tools/parity/src/generate/fresh.rs",
        "struct Generator; impl Generator { fn own_type(&mut self) {} }",
        &types,
    )
    .unwrap();
    let syntax =
        syn::parse_file("impl Generator { fn new() { generator.signature_text(); } }").unwrap();
    let error = check_signature_boundary("tools/parity/src/generate.rs", &syntax).unwrap_err();
    assert!(
        error.contains("source closure") && error.contains("signature_text"),
        "{error}"
    );
}
