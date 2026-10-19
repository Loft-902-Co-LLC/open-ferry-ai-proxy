//! Darwin extended ACLs, read from the same descriptor as the ownership and mode.

use std::ffi::c_void;
use std::io;
use std::os::fd::RawFd;
use std::ptr;

// sys/acl.h: Darwin's ACL iterator returns 0 for an entry and EINVAL at the end.
const ACL_TYPE_EXTENDED: libc::c_int = 0x100;
const ACL_FIRST_ENTRY: libc::c_int = 0;
const ACL_NEXT_ENTRY: libc::c_int = -1;
const ACL_EXTENDED_ALLOW: libc::c_int = 1;
const ACL_EXTENDED_DENY: libc::c_int = 2;
const ID_TYPE_UID: libc::c_int = 0;
// Write/add data, delete, append/add directory, delete child, write attributes,
// write extended attributes, write security and change owner.
const MUTATION: u64 =
    (1 << 2) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 8) | (1 << 10) | (1 << 12) | (1 << 13);

unsafe extern "C" {
    fn acl_get_fd_np(fd: libc::c_int, kind: libc::c_int) -> *mut c_void;
    fn acl_free(allocation: *mut c_void) -> libc::c_int;
    fn acl_get_entry(
        acl: *mut c_void,
        entry_id: libc::c_int,
        entry: *mut *mut c_void,
    ) -> libc::c_int;
    fn acl_get_tag_type(entry: *mut c_void, tag: *mut libc::c_int) -> libc::c_int;
    fn acl_get_permset_mask_np(entry: *mut c_void, mask: *mut u64) -> libc::c_int;
    fn acl_get_qualifier(entry: *mut c_void) -> *mut c_void;
    fn mbr_uuid_to_id(uuid: *const u8, id: *mut libc::id_t, kind: *mut libc::c_int) -> libc::c_int;
}

struct Allocation(*mut c_void);

impl Drop for Allocation {
    fn drop(&mut self) {
        // SAFETY: both ACLs and qualifiers returned by the ACL API use acl_free.
        unsafe { acl_free(self.0) };
    }
}

fn unreadable() -> String {
    "its extended ACL can't be read".to_string()
}

pub(super) fn check(fd: RawFd, owner: u32) -> Result<(), String> {
    // SAFETY: the caller holds an open fd. This returns a separately owned ACL.
    let acl = unsafe { acl_get_fd_np(fd, ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        // Darwin reports ENOENT when the file security has no ACL property.
        return if io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
            Ok(())
        } else {
            Err(unreadable())
        };
    }
    let acl = Allocation(acl);
    let mut which = ACL_FIRST_ENTRY;
    loop {
        let mut entry = ptr::null_mut();
        // SAFETY: the ACL is owned and valid, and entry is an out-pointer.
        if unsafe { acl_get_entry(acl.0, which, &mut entry) } != 0 {
            return if io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL) {
                Ok(())
            } else {
                Err(unreadable())
            };
        }
        which = ACL_NEXT_ENTRY;
        let mut tag = 0;
        let mut permissions = 0;
        // SAFETY: entry belongs to the still-owned ACL; outputs are valid.
        if entry.is_null()
            || unsafe { acl_get_tag_type(entry, &mut tag) } != 0
            || unsafe { acl_get_permset_mask_np(entry, &mut permissions) } != 0
        {
            return Err(unreadable());
        }
        match tag {
            ACL_EXTENDED_DENY => continue,
            ACL_EXTENDED_ALLOW => {}
            _ => return Err(unreadable()),
        }
        if permissions & MUTATION == 0 {
            continue;
        }
        // Do not infer effective access from deny ordering or group membership. Even an
        // inherit-only write grant can give another account control over future children.
        // SAFETY: entry is valid; the qualifier is a separately owned 16-byte UUID.
        let qualifier = unsafe { acl_get_qualifier(entry) };
        if qualifier.is_null() {
            return Err(unreadable());
        }
        let qualifier = Allocation(qualifier);
        let mut id = 0;
        let mut kind = -1;
        // SAFETY: the qualifier holds the UUID, and both outputs are valid.
        if unsafe { mbr_uuid_to_id(qualifier.0.cast(), &mut id, &mut kind) } != 0 {
            return Err(unreadable());
        }
        if kind != ID_TYPE_UID || (id != owner && id != 0) {
            return Err("its extended ACL lets another account write or change it".to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::os::fd::AsRawFd;

    #[test]
    fn an_invalid_descriptor_fails_closed() {
        // Not upstream's: a failed ACL read is never treated as an empty ACL.
        assert!(check(-1, 0).is_err());
    }

    #[test]
    fn an_acl_stays_with_the_opened_object_after_its_name_is_replaced() {
        // Not upstream's: no path lookup after the fd was opened by the trust walk.
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        let original = File::create(&path).unwrap();
        std::fs::rename(&path, root.path().join("old")).unwrap();
        File::create(&path).unwrap();
        assert!(
            std::process::Command::new("chmod")
                .args(["+a", "everyone allow write"])
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        check(original.as_raw_fd(), uid).expect("original stays safe");
        assert!(check(File::open(path).unwrap().as_raw_fd(), uid).is_err());
    }
}
