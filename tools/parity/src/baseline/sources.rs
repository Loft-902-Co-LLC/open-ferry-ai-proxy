//! The generator sources compiled into this binary, for the relabel guard.
pub(super) const EMBEDDED: &[(&str, &str)] = &[
    (
        "tools/parity/src/generate.rs",
        include_str!("../generate.rs"),
    ),
    (
        "tools/parity/src/generate/config_save.rs",
        include_str!("../generate/config_save.rs"),
    ),
    (
        "tools/parity/src/generate/config_diff.rs",
        include_str!("../generate/config_diff.rs"),
    ),
    ("tools/parity/src/cases.rs", include_str!("../cases.rs")),
];
