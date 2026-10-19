//! Darwin ACL rights, kept separate from FFI so the policy is tested on every platform.

// Read/list, execute/search, read attributes/xattrs/security, synchronize, generic
// execute and generic read. Values checked against Apple's sys/acl.h and sys/kauth.h:
// https://github.com/apple-oss-distributions/Libc/blob/main/include/sys/acl.h
// https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/kauth.h
const READ_ONLY: u64 =
    (1 << 1) | (1 << 3) | (1 << 7) | (1 << 9) | (1 << 11) | (1 << 20) | (1 << 22) | (1 << 24);

pub(super) fn can_mutate(permissions: u64) -> bool {
    permissions & !READ_ONLY != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_rights_do_not_mutate() {
        // Not upstream's: the complete read-only set and its individual rights are safe.
        assert!(!can_mutate(0));
        assert!(!can_mutate(READ_ONLY));
        for bit in [1, 3, 7, 9, 11, 20, 22, 24] {
            assert!(!can_mutate(1 << bit), "bit {bit}");
        }
    }

    #[test]
    fn generic_all_and_generic_write_mutate() {
        // Not upstream's: Darwin expands these generic rights when checking access.
        for bit in [21, 23] {
            assert!(can_mutate(1 << bit), "bit {bit}");
            assert!(can_mutate(READ_ONLY | (1 << bit)), "bit {bit}");
        }
    }

    #[test]
    fn every_specific_mutation_right_mutates() {
        // Not upstream's: write/add, delete, append/add directory, delete child,
        // write attributes/xattrs/security and change owner.
        for bit in [2, 4, 5, 6, 8, 10, 12, 13] {
            assert!(can_mutate(1 << bit), "bit {bit}");
        }
    }

    #[test]
    fn unknown_rights_mutate() {
        // Not upstream's: future rights must not silently become read-only.
        for bit in [30, 63] {
            assert!(can_mutate(1 << bit), "bit {bit}");
        }
    }
}
