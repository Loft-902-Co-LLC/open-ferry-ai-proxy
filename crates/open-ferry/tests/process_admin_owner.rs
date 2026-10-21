//! Exercise the executable's startup hook under object-creator ownership.
#![cfg(windows)]

use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::process::{Command, Stdio};
use std::ptr;

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, IsWellKnownSid, OWNER_SECURITY_INFORMATION, SetTokenInformation,
    TOKEN_ADJUST_DEFAULT, TOKEN_ELEVATION, TOKEN_GROUPS, TOKEN_INFORMATION_CLASS, TOKEN_OWNER,
    TOKEN_QUERY, TOKEN_USER, TokenElevation, TokenGroups, TokenOwner, TokenUser,
    WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

// TOKEN_GROUPS attributes from winnt.h.
const SE_GROUP_ENABLED: u32 = 0x4;
const SE_GROUP_OWNER: u32 = 0x8;

fn information(token: &OwnedHandle, class: TOKEN_INFORMATION_CLASS) -> Vec<u64> {
    let mut needed = 0;
    // SAFETY: the initial query only obtains the required buffer size.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            class,
            ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    assert!(needed > 0);
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: this aligned buffer is large enough for the queried structure and SIDs.
    assert_ne!(
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                class,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        },
        0
    );
    buffer
}

#[test]
fn executable_startup_normalizes_an_object_creator_owner() {
    const CHILD: &str = "OFP_EXECUTABLE_OWNER_CHILD";
    const TEST: &str = "executable_startup_normalizes_an_object_creator_owner";
    if std::env::var_os(CHILD).is_none() {
        // A primary token change must never affect another test in this harness.
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "startup owner test failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;")),
            "startup owner child must run exactly one test"
        );
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        return;
    }

    let mut token = ptr::null_mut();
    // SAFETY: valid pseudo-handle and output pointer.
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
        0
    );
    // SAFETY: this fresh handle has no other owner.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let user = information(&token, TokenUser);
    let groups = information(&token, TokenGroups);
    let elevation = information(&token, TokenElevation);
    // SAFETY: all three buffers contain the successfully queried aligned structures;
    // their SID pointers and flexible group tail remain live throughout the test.
    let eligible = unsafe {
        let user = &*user.as_ptr().cast::<TOKEN_USER>();
        let groups = groups.as_ptr().cast::<TOKEN_GROUPS>();
        let groups =
            std::slice::from_raw_parts((*groups).Groups.as_ptr(), (*groups).GroupCount as usize);
        (*elevation.as_ptr().cast::<TOKEN_ELEVATION>()).TokenIsElevated != 0
            && IsWellKnownSid(user.User.Sid, WinLocalSystemSid) == 0
            && groups.iter().any(|group| {
                IsWellKnownSid(group.Sid, WinBuiltinAdministratorsSid) != 0
                    && group.Attributes & (SE_GROUP_ENABLED | SE_GROUP_OWNER)
                        == SE_GROUP_ENABLED | SE_GROUP_OWNER
            })
    };
    if !eligible {
        assert_ne!(
            std::env::var("OPEN_FERRY_REQUIRE_ELEVATED_OWNER_TESTS").as_deref(),
            Ok("1"),
            "CI requires an elevated non-SYSTEM token with an owner-eligible Administrators group"
        );
        eprintln!("startup owner assertions skipped: requires an eligible elevated administrator");
        return;
    }

    let mut writable = ptr::null_mut();
    // SAFETY: valid pseudo-handle and output pointer.
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_DEFAULT, &mut writable) },
        0
    );
    // SAFETY: this fresh handle has no other owner. The isolated child exits afterwards.
    let writable = unsafe { OwnedHandle::from_raw_handle(writable) };
    // SAFETY: the queried user SID remains live in `user`.
    let owner = TOKEN_OWNER {
        Owner: unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid },
    };
    // SAFETY: TOKEN_OWNER and its SID remain live for the call.
    assert_ne!(
        unsafe {
            SetTokenInformation(
                writable.as_raw_handle(),
                TokenOwner,
                (&raw const owner).cast(),
                size_of::<TOKEN_OWNER>() as u32,
            )
        },
        0
    );
    let before = information(&token, TokenOwner);
    // SAFETY: successful query and live user SID.
    assert_ne!(
        unsafe { EqualSid((*before.as_ptr().cast::<TOKEN_OWNER>()).Owner, owner.Owner) },
        0
    );

    let root = match std::env::var_os("OFP_TEST_SCRATCH") {
        Some(root) => tempfile::tempdir_in(root).unwrap(),
        None => tempfile::tempdir().unwrap(),
    };
    let config = root.path().join("config.yaml");
    // init only writes a scratch config. Discard both streams at the OS boundary:
    // they contain generated keys and must never enter test diagnostics.
    let status = Command::new(env!("CARGO_BIN_EXE_open-ferry"))
        .arg("init")
        .arg("-config")
        .arg(&config)
        .current_dir(root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "init exit status: {status}");
    let name: Vec<u16> = config.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: live, terminated path and constant/null remaining arguments.
    let file = unsafe {
        CreateFileW(
            name.as_ptr(),
            READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    assert_ne!(file, windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE);
    // SAFETY: this fresh handle has no other owner.
    let file = unsafe { OwnedHandle::from_raw_handle(file) };
    let mut actual_owner = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    // SAFETY: valid file handle and output pointers; unused outputs are null.
    assert_eq!(
        unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut actual_owner,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut descriptor,
            )
        },
        0
    );
    // SAFETY: the SID is valid until the descriptor is freed.
    let is_admin = unsafe { IsWellKnownSid(actual_owner, WinBuiltinAdministratorsSid) };
    // SAFETY: GetSecurityInfo allocated this descriptor with LocalAlloc.
    unsafe { LocalFree(descriptor) };
    assert_ne!(
        is_admin, 0,
        "the executable did not create an Administrators-owned config"
    );
    eprintln!(
        "startup owner assertions ran: object-creator init produced an Administrators-owned config"
    );
}
