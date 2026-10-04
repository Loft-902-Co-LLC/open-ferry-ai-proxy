// The config-diff/details entry will run upstream's
// diff.BuildConfigChangeDetails (internal/watcher/diff/config_diff.go) on
// two YAML configs limited to the keys open-ferry types, and give the
// details. Not ported yet (P3 WP-B): it gives null, and no case is
// generated.
package main

func init() {
	translators["config-diff/details"] = configDiffDetails
}

func configDiffDetails(in input) []byte {
	return []byte("null")
}
