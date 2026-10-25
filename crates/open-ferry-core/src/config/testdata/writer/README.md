# Writer regression fixtures

All inputs are synthetic. Each JSON array has one compact row per line.

- `writer_cases.json`: the 26 original operation controls.
- `writer_r2_cases.json`: additional review shapes and two separator controls. Sources already in `writer_cases.json` are not repeated here. The `wc:` aliases were identical and were removed.
- `writer_r2_main.json`: `[case, operation, normal_digest, old_switch_off_digest?]`. An omitted fourth element equals the normal digest. The four random probes retain their operation filters.
- `writer_go.json`: pinned Go result digests. Six document-marker results retain full output bytes because the test re-reads their comment owners.
- `writer_parent_foot.json`: one original input/operation fixture with full Go bytes, required by the independent ownership comparison.

Result digests are the first 16 hexadecimal SHA-256 characters of a compact JSON object with sorted keys: `error`, `files`, `refused`. `files` contains full SHA-256 hashes of successful UTF-8 snapshots, `error` is the full error-text SHA-256 or null, and `refused` is a boolean. These are change-detection receipts, not certification evidence. Historical Go refusals did not record error text; that comparison omits Rust's error text too, while the main receipts continue checking refusal wording.

`writer_before.json` was removed because the main receipts cover its comparisons. Owner, config-value and repeat-write assertions remain independent of the digests. The ignored capture tests can export full byte differences when needed.

`writer_r4_cases.json` adds 14 source shapes for preceding-key feet and following heads. `writer_r4_main.json` has 182 rows `[case, operation, main_receipt, tail_only_receipt?]`. The first receipt is from main at `78c3beeb`; the optional fourth entry pins six already-faithful results after the separator tail check. These receipts use the same sorted-summary 16-hex SHA-256 format as the other comparison rows. Tests separately check comment owners, native values, complete paragraphs, and same-setting repeat bytes. Existing fixture rows are unchanged.

`writer_r5_cases.json` adds three duplicate-text comment boundaries and a legacy root-foot control. `writer_r5_owners.json` freezes the current owner inventory of `s3_root_deep_seq_col6` on nested2 and raw2 at `0138ba2e`; each row is `[path, slot, comment_line, count]`. These are partial owner inventories, not output-byte goldens, so a further loss or move fails the test.

After the loader update, all 1,355 R2/R4 main receipt rows were checked against main `80f6f4aa`. Only 58 R2 rows needed new normal and old-switch digests: 38 operations now succeed and 20 retain their refusal with new diagnostic wording. Matching rows and every other fixture stayed unchanged. The quoted-scalar tests now check source comment owners, native values and repeat stability for the newly accepted nested/raw writes.

- `owner_*.json`: compact source-owner probes, examples and occurrence maps; matrix and legacy OFF outcomes use the result digests above. Caller digests cover every result field, replacing file and error text with full SHA-256 hashes before hashing the sorted JSON. Examples, probes and repaired legacy files retain readable bytes; repeated sources reference the first row ID. `owner_round5*.json` adds header, whitespace, duplicate-comment and whole-file fallback controls.

`owner_round5_source_boundaries.json` adds two source-multiset controls for comments in later YAML documents.
`owner_round5_incoming_source.json` pairs an incoming YAML body with an unparsed comment and a fully covered body that still permits a repair.
