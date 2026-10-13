//! The Windows trust check: reparse points, owner, and the DACL's write-class allow entries.
//!
//! The judgement ([`judge`]) is a pure function over a parsed [`Descriptor`], so tests build
//! descriptors from SDDL and need no real directory with an unusual owner.
//!
//! This module also holds the SID and security-descriptor helpers the endpoint shares.

use std::ffi::{OsStr, c_void};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, GetTokenInformation,
    OWNER_SECURITY_INFORMATION, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
    OPEN_EXISTING, READ_CONTROL,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

use super::TrustError;
use crate::endpoint::{Identity, ServiceKind};

pub(crate) const SID_SYSTEM: &str = "S-1-5-18";
pub(crate) const SID_ADMINISTRATORS: &str = "S-1-5-32-544";
const SID_TRUSTED_INSTALLER: &str =
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";
const SID_CREATOR_OWNER: &str = "S-1-3-0";

/// The protected DACL of a system service's data directory: full control for SYSTEM and
/// Administrators, inherited by what is created inside.
const SYSTEM_DATA_DIR_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

const FILE_WRITE_DATA: u32 = 0x2;
const FILE_APPEND_DATA: u32 = 0x4;
const FILE_WRITE_EA: u32 = 0x10;
const FILE_DELETE_CHILD: u32 = 0x40;
const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
const DELETE: u32 = 0x1_0000;
const WRITE_DAC: u32 = 0x4_0000;
const WRITE_OWNER: u32 = 0x8_0000;
const GENERIC_ALL: u32 = 0x1000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;

/// Every right that lets an account change what a directory holds: the spec's write class.
const WRITE_CLASS: u32 = FILE_WRITE_DATA
    | FILE_APPEND_DATA
    | FILE_WRITE_EA
    | FILE_DELETE_CHILD
    | FILE_WRITE_ATTRIBUTES
    | DELETE
    | WRITE_DAC
    | WRITE_OWNER
    | GENERIC_WRITE
    | GENERIC_ALL;

/// The rights that let an account replace, rename or take over a directory's existing
/// children: what matters on an ancestor.
///
/// Stock Windows gives `Users` (and `Authenticated Users`) the rights to add files and
/// subdirectories under `C:\ProgramData` and `C:\`. Those create entries beside ours and are
/// harmless; they can't swap out, rename or delete the directory we run from, which needs
/// DELETE, DELETE_CHILD, WRITE_DAC or WRITE_OWNER (or a generic write or all). So an ancestor is
/// refused only on these. The directories we serve from, and their files, get the full class.
const REPLACE_CLASS: u32 =
    DELETE | FILE_DELETE_CHILD | WRITE_DAC | WRITE_OWNER | GENERIC_WRITE | GENERIC_ALL;

const ACE_ALLOWED: u8 = 0;
const ACE_ALLOWED_COMPOUND: u8 = 4;
const ACE_ALLOWED_OBJECT: u8 = 5;
const ACE_ALLOWED_CALLBACK: u8 = 9;
const ACE_ALLOWED_CALLBACK_OBJECT: u8 = 11;
const INHERIT_ONLY_ACE: u8 = 0x08;

/// What a trust check tolerates. Build one with [`TrustRules::system`] or
/// [`TrustRules::for_identity`].
#[derive(Debug, Clone)]
pub struct TrustRules {
    /// SIDs that may own a path and may hold write rights on it.
    trusted: Vec<String>,
}

impl TrustRules {
    /// A system service: SYSTEM, Administrators and TrustedInstaller only.
    pub fn system() -> Self {
        Self {
            trusted: vec![
                SID_SYSTEM.into(),
                SID_ADMINISTRATORS.into(),
                SID_TRUSTED_INSTALLER.into(),
            ],
        }
    }

    /// The rules for a service identity. A user service also trusts its own SID.
    pub fn for_identity(identity: &Identity) -> Self {
        let mut rules = Self::system();
        if identity.kind() == ServiceKind::User {
            rules.trusted.push(identity.sid().to_string());
        }
        rules
    }

    fn is_trusted(&self, sid: &str) -> bool {
        self.trusted.iter().any(|t| t == sid)
    }
}

/// Checks each directory in `paths` and every ancestor up to the volume root.
///
/// The directories themselves must pass the full write-class rule; ancestors are refused only
/// for the rights that replace what is below them (see `REPLACE_CLASS`). Every reparse point
/// in a chain is refused.
pub fn check_dirs(paths: &[&Path], rules: &TrustRules) -> Result<(), TrustError> {
    for path in paths {
        check_chain(path, rules, None)?;
    }
    Ok(())
}

/// Checks a file the same way as a directory, then its directory chain.
pub fn check_file(path: &Path, rules: &TrustRules) -> Result<(), TrustError> {
    let Some(parent) = path.parent() else {
        return Err(TrustError::new(path, "it has no parent directory"));
    };
    require_absolute(path)?;
    check_chain(parent, rules, None)?;
    check_one(path, rules, Tier::Full)
}

/// Refuses a path that depends on the current directory or drive: `foo\bar`, `\foo` (relative
/// to the current drive) and `C:foo` (relative to that drive's current directory).
fn require_absolute(path: &Path) -> Result<(), TrustError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(TrustError::new(
            path,
            "the path is not absolute (drive-relative and root-relative paths count)",
        ))
    }
}

/// Makes the last component of `path` a directory with the default descriptor it inherits (a
/// user's own profile directories grant only that user, SYSTEM and Administrators write). The
/// parent must exist. An existing directory is left as it is and only has to be a real
/// directory, not a link. The caller runs [`check_dirs`] afterwards for the verdict.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    match std::fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let meta = std::fs::symlink_metadata(path)?;
            if meta.is_dir() && !meta.file_type().is_symlink() {
                Ok(())
            } else {
                Err(e)
            }
        }
        Err(e) => Err(e),
    }
}

/// Creates a system service's data directory with the protected DACL (`SYSTEM` and
/// `Administrators` only). An existing directory is an error and is never repaired.
pub fn create_system_data_dir(path: &Path) -> io::Result<()> {
    create_dir_with_sddl(path, SYSTEM_DATA_DIR_SDDL)
}

fn create_dir_with_sddl(path: &Path, sddl: &str) -> io::Result<()> {
    let descriptor = SecurityDescriptor::from_sddl(sddl)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.as_ptr(),
        bInheritHandle: 0,
    };
    let name = wide(path.as_os_str());
    // SAFETY: `name` is NUL-terminated and `attributes` points to a live descriptor.
    if unsafe { CreateDirectoryW(name.as_ptr(), &attributes) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tier {
    /// The full write class.
    Full,
    /// Only the rights that replace children.
    Ancestor,
}

/// `below`, when given, skips every component that is `below` or above it (a test's way of
/// looking only at the part of a chain it built).
fn check_chain(path: &Path, rules: &TrustRules, below: Option<&Path>) -> Result<(), TrustError> {
    require_absolute(path)?;
    let mut chain: Vec<&Path> = path.ancestors().collect();
    chain.reverse();
    for (index, component) in chain.iter().enumerate() {
        if component.as_os_str().is_empty() || below.is_some_and(|base| base.starts_with(component))
        {
            continue;
        }
        let is_target = index + 1 == chain.len();
        let tier = if is_target {
            Tier::Full
        } else {
            Tier::Ancestor
        };
        check_one(component, rules, tier)?;
    }
    Ok(())
}

fn check_one(path: &Path, rules: &TrustRules, tier: Tier) -> Result<(), TrustError> {
    let name = wide(path.as_os_str());
    // Share DELETE as well as READ and WRITE so a short trust check does not block
    // atomic replacements. Root-first owner/DACL checks protect the namespace;
    // both reads still describe this one opened object.
    // SAFETY: `name` is NUL-terminated; the optional pointers are null.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            READ_CONTROL | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let e = io::Error::last_os_error();
        return Err(TrustError::new(path, format!("it can't be examined: {e}")));
    }
    // SAFETY: the freshly opened handle has no other owner and is valid until dropped.
    let _handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    // SAFETY: an all-zero information buffer is valid for the API to fill.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `handle` is live and `info` is a valid out-pointer.
    if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
        let e = io::Error::last_os_error();
        return Err(TrustError::new(path, format!("it can't be examined: {e}")));
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(TrustError::new(
            path,
            "it is a reparse point (a link, junction or mount point)",
        ));
    }
    let descriptor = read_descriptor(handle).map_err(|why| TrustError::new(path, why))?;
    judge(&descriptor, rules, tier).map_err(|why| TrustError::new(path, why))
}

/// An owner and a parsed DACL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Descriptor {
    pub owner: Option<String>,
    /// `None` is a NULL DACL: everyone has full access.
    pub dacl: Option<Vec<Ace>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ace {
    Allow {
        sid: String,
        mask: u32,
        inherit_only: bool,
    },
    /// An allow entry of a kind this check can't read (object, callback, compound).
    UnreadableAllow { kind: u8, inherit_only: bool },
    /// A deny, audit or label entry: it grants nothing.
    Other,
}

/// Judges a descriptor. Fails closed on anything it can't read.
fn judge(descriptor: &Descriptor, rules: &TrustRules, tier: Tier) -> Result<(), String> {
    match &descriptor.owner {
        Some(owner) if rules.is_trusted(owner) => {}
        Some(owner) => {
            return Err(format!(
                "it is owned by {owner}, which is not a trusted account"
            ));
        }
        None => return Err("it has no owner".to_string()),
    }
    let Some(aces) = &descriptor.dacl else {
        return Err("it has no DACL, so everyone can write to it".to_string());
    };
    let class = match tier {
        Tier::Full => WRITE_CLASS,
        Tier::Ancestor => REPLACE_CLASS,
    };
    for ace in aces {
        match ace {
            Ace::Other => {}
            // An entry that only passes to children does not apply to an ancestor, whatever
            // its kind.
            Ace::UnreadableAllow {
                inherit_only: true, ..
            } if tier == Tier::Ancestor => {}
            Ace::UnreadableAllow { kind, .. } => {
                return Err(format!(
                    "its DACL has an allow entry of a kind (type {kind}) this check can't read"
                ));
            }
            Ace::Allow {
                sid,
                mask,
                inherit_only,
            } => {
                if rules.is_trusted(sid) || (*inherit_only && sid == SID_CREATOR_OWNER) {
                    continue;
                }
                // An entry that only passes to children does not apply to the directory
                // itself, which is what an ancestor is judged on.
                if *inherit_only && tier == Tier::Ancestor {
                    continue;
                }
                let hit = mask & class;
                if hit != 0 {
                    return Err(format!("{sid} is allowed to write to it (rights {hit:#x})"));
                }
            }
        }
    }
    Ok(())
}

fn read_descriptor(handle: HANDLE) -> Result<Descriptor, String> {
    let mut owner: PSID = ptr::null_mut();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut security: *mut c_void = ptr::null_mut();
    // SAFETY: `handle` remains live in `check_one` and the out-pointers are valid.
    let rc = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut security,
        )
    };
    if rc != 0 {
        return Err(format!(
            "its security descriptor can't be read: {}",
            io::Error::from_raw_os_error(rc as i32)
        ));
    }
    let _guard = LocalGuard(security);
    // SAFETY: the pointers were filled in by `GetSecurityInfo` and stay valid while
    // `_guard` holds the descriptor.
    unsafe { describe(owner, dacl, !dacl.is_null()) }
}

/// Parses an owner SID and a DACL.
///
/// # Safety
/// `owner` must be null or a valid SID, and `dacl` null or a valid ACL, for the whole call.
pub(crate) unsafe fn describe(
    owner: PSID,
    dacl: *const ACL,
    dacl_present: bool,
) -> Result<Descriptor, String> {
    let owner = if owner.is_null() {
        None
    } else {
        Some(sid_to_string(owner).map_err(|e| e.to_string())?)
    };
    if !dacl_present || dacl.is_null() {
        return Ok(Descriptor { owner, dacl: None });
    }
    // SAFETY: an all-zero `ACL_SIZE_INFORMATION` is valid, and `dacl` is a valid ACL.
    let mut info: ACL_SIZE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a valid buffer of the size passed.
    let ok = unsafe {
        GetAclInformation(
            dacl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    if ok == 0 {
        return Err(format!(
            "its DACL can't be read: {}",
            io::Error::last_os_error()
        ));
    }
    let mut aces = Vec::new();
    for index in 0..info.AceCount {
        let mut raw: *mut c_void = ptr::null_mut();
        // SAFETY: `index` is below the ACL's entry count; `raw` is a valid out-pointer.
        if unsafe { GetAce(dacl, index, &mut raw) } == 0 || raw.is_null() {
            return Err(format!("entry {index} of its DACL can't be read"));
        }
        // SAFETY: every ACE starts with a header.
        let header: ACE_HEADER = unsafe { ptr::read_unaligned(raw.cast()) };
        match header.AceType {
            ACE_ALLOWED => {
                let sid_offset = std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart);
                let mask_offset = std::mem::offset_of!(ACCESS_ALLOWED_ACE, Mask);
                if usize::from(header.AceSize) < sid_offset + 8 {
                    return Err(format!("entry {index} of its DACL is malformed"));
                }
                // SAFETY: the entry is at least `sid_offset + 8` bytes (checked above), so
                // both the mask and the start of the SID lie inside it.
                let (mask, sid) = unsafe {
                    let base = raw.cast::<u8>();
                    (
                        ptr::read_unaligned(base.add(mask_offset).cast::<u32>()),
                        base.add(sid_offset).cast::<c_void>(),
                    )
                };
                let sid = sid_to_string(sid)
                    .map_err(|e| format!("entry {index} of its DACL has an unreadable SID: {e}"))?;
                aces.push(Ace::Allow {
                    sid,
                    mask,
                    inherit_only: header.AceFlags & INHERIT_ONLY_ACE != 0,
                });
            }
            kind @ (ACE_ALLOWED_COMPOUND
            | ACE_ALLOWED_OBJECT
            | ACE_ALLOWED_CALLBACK
            | ACE_ALLOWED_CALLBACK_OBJECT) => {
                aces.push(Ace::UnreadableAllow {
                    kind,
                    inherit_only: header.AceFlags & INHERIT_ONLY_ACE != 0,
                });
            }
            _ => aces.push(Ace::Other),
        }
    }
    Ok(Descriptor {
        owner,
        dacl: Some(aces),
    })
}

/// Frees a `LocalAlloc` block on drop.
pub(crate) struct LocalGuard(pub *mut c_void);

impl Drop for LocalGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from an API documented to return `LocalAlloc` memory.
            unsafe { LocalFree(self.0) };
        }
    }
}

/// A self-relative security descriptor made from SDDL.
pub(crate) struct SecurityDescriptor {
    ptr: *mut c_void,
}

// SAFETY: the descriptor is an immutable block of memory with no thread affinity.
unsafe impl Send for SecurityDescriptor {}
// SAFETY: see `Send`.
unsafe impl Sync for SecurityDescriptor {}

impl SecurityDescriptor {
    pub(crate) fn from_sddl(sddl: &str) -> io::Result<Self> {
        let text = wide(OsStr::new(sddl));
        let mut ptr: *mut c_void = ptr::null_mut();
        // SAFETY: `text` is NUL-terminated and `ptr` is a valid out-pointer. Revision 1 is
        // SDDL_REVISION_1.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                1,
                &mut ptr,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { ptr })
    }

    pub(crate) fn as_ptr(&self) -> *mut c_void {
        self.ptr
    }

    /// Owner and DACL of the descriptor, for tests.
    #[cfg(test)]
    pub(crate) fn describe(&self) -> Result<Descriptor, String> {
        use windows_sys::Win32::Security::{GetSecurityDescriptorDacl, GetSecurityDescriptorOwner};
        let mut owner: PSID = ptr::null_mut();
        let mut defaulted = 0;
        // SAFETY: `self.ptr` is a valid descriptor and the out-pointers are valid.
        if unsafe { GetSecurityDescriptorOwner(self.ptr, &mut owner, &mut defaulted) } == 0 {
            return Err(format!(
                "the owner can't be read: {}",
                io::Error::last_os_error()
            ));
        }
        let mut present = 0;
        let mut dacl: *mut ACL = ptr::null_mut();
        // SAFETY: as above.
        if unsafe { GetSecurityDescriptorDacl(self.ptr, &mut present, &mut dacl, &mut defaulted) }
            == 0
        {
            return Err(format!(
                "the DACL can't be read: {}",
                io::Error::last_os_error()
            ));
        }
        // SAFETY: the pointers point into `self.ptr`, which is alive for the call.
        unsafe { describe(owner, dacl, present != 0) }
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: `ConvertStringSecurityDescriptorToSecurityDescriptorW` allocated it with `LocalAlloc`.
        unsafe { LocalFree(self.ptr) };
    }
}

/// A NUL-terminated UTF-16 copy of `s`.
pub(crate) fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// The string form (`S-1-5-...`) of a SID.
pub(crate) fn sid_to_string(sid: PSID) -> io::Result<String> {
    let mut text: *mut u16 = ptr::null_mut();
    // SAFETY: `sid` is a valid SID (the caller's contract) and `text` is a valid out-pointer.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _guard = LocalGuard(text.cast());
    let mut len = 0;
    // SAFETY: `text` is NUL-terminated, so the scan stops at the terminator.
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` code units were just read from `text`.
    let units = unsafe { std::slice::from_raw_parts(text, len) };
    Ok(String::from_utf16_lossy(units))
}

/// The user SID, as a string, of the token of `process`.
pub(crate) fn process_user_sid(process: HANDLE) -> io::Result<String> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: `process` is a valid process handle and `token` a valid out-pointer.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `token` is a freshly opened handle that nothing else owns.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let raw = std::os::windows::io::AsRawHandle::as_raw_handle(&token);
    let mut needed = 0u32;
    // SAFETY: a null buffer of length 0 asks for the size.
    let probe = unsafe { GetTokenInformation(raw, TokenUser, ptr::null_mut(), 0, &mut needed) };
    if probe != 0
        || needed == 0
        || io::Error::last_os_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32)
    {
        return Err(io::Error::last_os_error());
    }
    // A u64 buffer keeps the TOKEN_USER (pointers inside) aligned.
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: the buffer holds at least `needed` bytes.
    if unsafe {
        GetTokenInformation(
            raw,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the buffer now holds a TOKEN_USER, whose SID points inside the buffer.
    let user: TOKEN_USER = unsafe { ptr::read(buffer.as_ptr().cast()) };
    sid_to_string(user.User.Sid)
}

/// The user SID of the running process.
pub(crate) fn current_user_sid() -> io::Result<String> {
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no closing.
    process_user_sid(unsafe { GetCurrentProcess() })
}

/// The user SID of the process with id `pid`, opened with the least access that reads it.
pub(crate) fn user_sid_of_pid(pid: u32) -> io::Result<String> {
    // SAFETY: plain call; a null result is handled below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `handle` is a freshly opened handle that nothing else owns.
    let owned = unsafe { OwnedHandle::from_raw_handle(handle) };
    process_user_sid(std::os::windows::io::AsRawHandle::as_raw_handle(&owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(sddl: &str) -> Descriptor {
        SecurityDescriptor::from_sddl(sddl)
            .expect("sddl")
            .describe()
            .expect("describe")
    }

    fn verdict(sddl: &str, tier: Tier) -> Result<(), String> {
        judge(&descriptor(sddl), &TrustRules::system(), tier)
    }

    const CLEAN: &str = "O:SYD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

    #[test]
    fn the_opened_junction_is_refused_before_its_target_descriptor_is_read() {
        let scratch = std::env::var_os("OFP_TEST_SCRATCH").expect("OFP_TEST_SCRATCH is set");
        let root = tempfile::tempdir_in(scratch).expect("scratch dir");
        let real = root.path().join("real");
        std::fs::create_dir(&real).expect("target");
        let link = root.path().join("link");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&real)
            .output()
            .expect("cmd");
        assert!(made.status.success(), "{made:?}");
        let err = check_one(&link, &TrustRules::system(), Tier::Full).unwrap_err();
        assert_eq!(err.path, link);
        assert_eq!(
            err.reason,
            "it is a reparse point (a link, junction or mount point)"
        );
    }

    #[test]
    fn an_open_that_cannot_find_the_path_reports_it_cannot_be_examined() {
        let scratch = std::env::var_os("OFP_TEST_SCRATCH").expect("OFP_TEST_SCRATCH is set");
        let root = tempfile::tempdir_in(scratch).expect("scratch dir");
        let missing = root.path().join("missing");
        let err = check_one(&missing, &TrustRules::system(), Tier::Full).unwrap_err();
        assert_eq!(err.path, missing);
        assert!(err.reason.starts_with("it can't be examined: "), "{err}");
    }

    #[test]
    fn create_private_dir_makes_a_directory_and_accepts_an_existing_one_but_not_a_file() {
        // Not upstream's: the default descriptor is allowed; an existing directory is untouched.
        let root = tempfile::tempdir().expect("temp dir");
        let fresh = root.path().join("fresh");
        create_private_dir(&fresh).expect("create");
        assert!(fresh.is_dir());
        create_private_dir(&fresh).expect("a second call is fine");

        let file = root.path().join("file");
        std::fs::write(&file, b"x").expect("file");
        assert!(create_private_dir(&file).is_err());
        assert!(create_private_dir(&root.path().join("no").join("parent")).is_err());
    }

    #[test]
    fn the_data_directory_descriptor_we_create_passes_the_check_we_run() {
        // Not upstream's: install and check agree.
        let sddl = format!("O:BA{SYSTEM_DATA_DIR_SDDL}");
        assert_eq!(verdict(&sddl, Tier::Full), Ok(()));
        assert_eq!(verdict(CLEAN, Tier::Full), Ok(()));
        assert_eq!(
            verdict(
                "O:S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464D:P(A;;FA;;;SY)",
                Tier::Full
            ),
            Ok(())
        );
    }

    #[test]
    fn an_owner_outside_system_administrators_and_trustedinstaller_is_refused() {
        // Not upstream's: v3 2.5's owner rule.
        let err = verdict("O:BUD:P(A;OICI;FA;;;SY)", Tier::Full).unwrap_err();
        assert!(err.contains("owned by S-1-5-32-545"), "{err}");
    }

    #[test]
    fn each_write_class_right_for_an_untrusted_account_is_refused() {
        // Not upstream's: every right in v3 2.5's list, one at a time.
        let rights = [
            0x2u32,
            0x4,
            0x10,
            0x40,
            0x100,
            0x1_0000,
            0x4_0000,
            0x8_0000,
            0x4000_0000,
            0x1000_0000,
        ];
        for right in rights {
            let sddl = format!("O:SYD:P(A;OICI;FA;;;SY)(A;;{right:#x};;;BU)");
            let err = verdict(&sddl, Tier::Full).unwrap_err();
            assert!(err.contains("S-1-5-32-545"), "{right:#x}: {err}");
        }
    }

    #[test]
    fn read_and_execute_for_users_is_fine() {
        // Not upstream's: only write-class rights count.
        assert_eq!(
            verdict("O:SYD:P(A;OICI;FA;;;SY)(A;OICI;0x1200a9;;;BU)", Tier::Full),
            Ok(())
        );
    }

    #[test]
    fn a_deny_entry_grants_nothing() {
        // Not upstream's: deny entries are skipped.
        assert_eq!(
            verdict("O:SYD:P(D;;FA;;;BU)(A;OICI;FA;;;SY)", Tier::Full),
            Ok(())
        );
    }

    #[test]
    fn creator_owner_inherit_only_is_ignored_but_other_inherit_only_entries_are_not() {
        // Not upstream's: v3 2.5's CREATOR OWNER exception.
        assert_eq!(
            verdict("O:SYD:P(A;OICI;FA;;;SY)(A;OICIIO;FA;;;CO)", Tier::Full),
            Ok(())
        );
        assert!(verdict("O:SYD:P(A;OICI;FA;;;SY)(A;OICIIO;FA;;;BU)", Tier::Full).is_err());
    }

    #[test]
    fn a_missing_dacl_means_everyone_can_write() {
        // Not upstream's: a NULL or absent DACL is refused.
        let err = verdict("O:BA", Tier::Full).unwrap_err();
        assert!(err.contains("no DACL"), "{err}");
    }

    #[test]
    fn an_allow_entry_we_cannot_read_fails_closed() {
        // Not upstream's: object and callback allow entries are refused.
        let descriptor = Descriptor {
            owner: Some(SID_SYSTEM.to_string()),
            dacl: Some(vec![Ace::UnreadableAllow {
                kind: ACE_ALLOWED_OBJECT,
                inherit_only: false,
            }]),
        };
        assert!(judge(&descriptor, &TrustRules::system(), Tier::Full).is_err());
        assert!(judge(&descriptor, &TrustRules::system(), Tier::Ancestor).is_err());
        // An inherit-only one never applies to an ancestor, but it does to a target, where
        // what it passes down could matter.
        let passes_down = Descriptor {
            owner: Some(SID_SYSTEM.to_string()),
            dacl: Some(vec![Ace::UnreadableAllow {
                kind: ACE_ALLOWED_CALLBACK,
                inherit_only: true,
            }]),
        };
        assert_eq!(
            judge(&passes_down, &TrustRules::system(), Tier::Ancestor),
            Ok(())
        );
        assert!(judge(&passes_down, &TrustRules::system(), Tier::Full).is_err());
    }

    #[test]
    fn ancestors_are_judged_on_replace_rights_only() {
        // Not upstream's: users may add entries under C:\ProgramData, and the ancestor tier
        // allows that but not deleting children or changing the ACL.
        let adds = "O:SYD:P(A;OICI;FA;;;SY)(A;;0x6;;;BU)";
        assert!(verdict(adds, Tier::Full).is_err());
        assert_eq!(verdict(adds, Tier::Ancestor), Ok(()));
        for right in [
            0x40u32,
            0x1_0000,
            0x4_0000,
            0x8_0000,
            0x4000_0000,
            0x1000_0000,
        ] {
            let sddl = format!("O:SYD:P(A;OICI;FA;;;SY)(A;;{right:#x};;;BU)");
            assert!(verdict(&sddl, Tier::Ancestor).is_err(), "{right:#x}");
        }
        let inherit_only = "O:SYD:P(A;OICI;FA;;;SY)(A;OICIIO;FA;;;BU)";
        assert_eq!(verdict(inherit_only, Tier::Ancestor), Ok(()));
    }

    #[test]
    fn a_user_service_also_trusts_its_own_sid() {
        // Not upstream's: for_identity adds the user's SID for a user service.
        let identity = Identity::current(ServiceKind::User).expect("identity");
        let sid = identity.sid().to_string();
        let sddl = format!("O:{sid}D:P(A;OICI;FA;;;{sid})");
        let d = descriptor(&sddl);
        assert!(judge(&d, &TrustRules::system(), Tier::Full).is_err());
        assert_eq!(
            judge(&d, &TrustRules::for_identity(&identity), Tier::Full),
            Ok(())
        );
    }

    // The tests below make real directories with real DACLs under a temp dir. The temp dir's own
    // ancestors carry whatever the host's ACLs are, so every chain check starts below it.

    struct Tree {
        base: tempfile::TempDir,
        me: String,
        rules: TrustRules,
    }

    impl Tree {
        fn new() -> Self {
            let identity = Identity::current(ServiceKind::User).expect("identity");
            Self {
                base: tempfile::tempdir().expect("temp dir"),
                me: identity.sid().to_string(),
                rules: TrustRules::for_identity(&identity),
            }
        }

        /// A directory with a protected DACL: the test user and SYSTEM in full, then `extra`.
        fn dir(&self, name: &str, extra: &str) -> std::path::PathBuf {
            let path = self.base.path().join(name);
            let sddl = format!("D:P(A;OICI;FA;;;{})(A;OICI;FA;;;SY){extra}", self.me);
            create_dir_with_sddl(&path, &sddl).expect("create directory");
            path
        }

        fn chain(&self, path: &Path) -> Result<(), TrustError> {
            check_chain(path, &self.rules, Some(self.base.path()))
        }
    }

    #[test]
    fn stock_style_users_entries_on_an_ancestor_pass_and_on_the_target_do_not() {
        // Not upstream's: Users may add files and subdirectories (0x2, 0x4) to C:\ProgramData.
        let tree = Tree::new();
        let ancestor = tree.dir("stock", "(A;;0x1200a9;;;BU)(A;;0x6;;;BU)");
        let target = tree.dir("stock\\target", "");
        assert!(tree.chain(&target).is_ok(), "{:?}", tree.chain(&target));
        // The same entries on the directory we run from are refused.
        let err = tree.chain(&ancestor).unwrap_err();
        assert!(err.reason.contains("S-1-5-32-545"), "{err}");
    }

    #[test]
    fn each_replace_right_for_users_on_an_ancestor_is_refused() {
        // Not upstream's: DELETE, DELETE_CHILD, WRITE_DAC and WRITE_OWNER, one at a time.
        let tree = Tree::new();
        for (index, right) in [0x1_0000u32, 0x40, 0x4_0000, 0x8_0000]
            .into_iter()
            .enumerate()
        {
            let ancestor = tree.dir(&format!("a{index}"), &format!("(A;;{right:#x};;;BU)"));
            let target = tree.dir(&format!("a{index}\\target"), "");
            let err = tree.chain(&target).unwrap_err();
            assert_eq!(err.path, ancestor, "{right:#x}: {err}");
            assert!(err.reason.contains("S-1-5-32-545"), "{right:#x}: {err}");
        }
    }

    #[test]
    fn inherit_only_entries_on_an_ancestor_are_ignored() {
        // Not upstream's: an entry that only passes down does not apply to the ancestor.
        let tree = Tree::new();
        tree.dir("io", "(A;OICIIO;FA;;;BU)");
        let target = tree.dir("io\\target", "");
        assert!(tree.chain(&target).is_ok(), "{:?}", tree.chain(&target));
    }

    #[test]
    fn a_target_that_users_can_add_to_is_refused() {
        // Not upstream's: the target gets the full write class.
        let tree = Tree::new();
        let target = tree.dir("t", "(A;;0x4;;;BU)");
        let err = tree.chain(&target).unwrap_err();
        assert!(err.reason.contains("S-1-5-32-545"), "{err}");
    }

    #[test]
    fn an_untrusted_owner_is_refused() {
        // Not upstream's: the owner (this user, or Administrators when elevated) is not trusted
        // by rules that name only SYSTEM.
        let tree = Tree::new();
        let target = tree.dir("owned", "");
        let rules = TrustRules {
            trusted: vec![SID_SYSTEM.to_string()],
        };
        let err = check_chain(&target, &rules, Some(tree.base.path())).unwrap_err();
        assert!(
            err.reason.contains("which is not a trusted account"),
            "{err}"
        );
    }

    #[test]
    fn a_junction_anywhere_in_the_chain_is_refused() {
        // Not upstream's: every reparse point is refused, here a junction made with mklink.
        let tree = Tree::new();
        let real = tree.dir("real", "");
        tree.dir("real\\child", "");
        let link = tree.base.path().join("link");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&real)
            .output()
            .expect("cmd");
        assert!(
            made.status.success(),
            "{}",
            String::from_utf8_lossy(&made.stdout)
        );
        let err = tree.chain(&link.join("child")).unwrap_err();
        assert_eq!(err.path, link, "{err}");
        assert!(err.reason.contains("reparse point"), "{err}");
    }

    #[test]
    fn relative_drive_relative_and_root_relative_paths_are_refused() {
        // Not upstream's: only a fully absolute path names one place regardless of the
        // process's current directory and drive.
        let rules = TrustRules::system();
        for path in [r"relative\dir", r"\rooted", "C:drive-relative", r".\here"] {
            let err = check_dirs(&[Path::new(path)], &rules).unwrap_err();
            assert!(err.reason.contains("not absolute"), "{path}: {err}");
            let err = check_file(Path::new(path), &rules).unwrap_err();
            assert!(!err.reason.is_empty(), "{path}");
        }
    }

    #[test]
    fn a_missing_path_is_refused() {
        // Not upstream's: a path that can't be examined fails closed.
        let tree = Tree::new();
        let err = tree.chain(&tree.base.path().join("missing")).unwrap_err();
        assert!(err.reason.contains("can't be examined"), "{err}");
    }

    #[test]
    fn a_host_ancestor_with_untrusted_writers_is_refused_by_the_public_check() {
        // Explicit foreign writers keep this assertion true with an elevated CI token too.
        let tree = Tree::new();
        tree.dir("foreign", "(A;;FA;;;WD)");
        let target = tree.dir("foreign\\pub", "");
        let err = check_dirs(&[&target], &tree.rules).unwrap_err();
        assert!(!err.reason.is_empty(), "{err}");
        assert!(
            !err.path.starts_with(&target),
            "the refusal is above the target: {err}"
        );
    }
}
