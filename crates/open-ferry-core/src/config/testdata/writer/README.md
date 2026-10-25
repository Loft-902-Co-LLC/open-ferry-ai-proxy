# Writer regression fixtures

All inputs are synthetic. Each JSON array has one compact row per line.

- `writer_cases.json`: the 26 original operation controls.
- `writer_r2_cases.json`: additional review shapes and two separator controls. Sources already in `writer_cases.json` are not repeated here. The `wc:` aliases were identical and were removed.
- `writer_r2_main.json`: `[case, operation, normal_digest, old_switch_off_digest?]`. An omitted fourth element equals the normal digest. The four random probes retain their operation filters.
- `writer_go.json`: pinned Go result digests. Six document-marker results retain full output bytes because the test re-reads their comment owners.
- `writer_parent_foot.json`: one original input/operation fixture with full Go bytes, required by the independent ownership comparison.

Result digests are the first 16 hexadecimal SHA-256 characters of a compact JSON object with sorted keys: `error`, `files`, `refused`. `files` contains full SHA-256 hashes of successful UTF-8 snapshots, `error` is the full error-text SHA-256 or null, and `refused` is a boolean. These are change-detection receipts, not certification evidence. Historical Go refusals did not record error text; that comparison omits Rust's error text too, while the main receipts continue checking refusal wording.

`writer_before.json` was removed because the main receipts cover its comparisons. Owner, config-value and repeat-write assertions remain independent of the digests. The ignored capture tests can export full byte differences when needed.
