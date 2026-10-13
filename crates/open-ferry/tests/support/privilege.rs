//! Reads the test process's privilege; child binaries inherit the same token.

#[cfg(unix)]
pub fn privileged() -> bool {
    // SAFETY: geteuid only reads this process's credentials.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(windows)]
pub fn privileged() -> bool {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token = std::ptr::null_mut();
    // SAFETY: the pseudo-handle is valid and token is a writable out-pointer.
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
        0
    );
    // SAFETY: the successfully opened handle has no other owner.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut length = 0;
    // SAFETY: the buffer is writable and its size matches TOKEN_ELEVATION.
    assert_ne!(
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenElevation,
                (&raw mut elevation).cast(),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut length,
            )
        },
        0
    );
    elevation.TokenIsElevated != 0
        || open_ferry_supervisor::endpoint::Identity::current(
            open_ferry_supervisor::endpoint::ServiceKind::User,
        )
        .unwrap()
        .sid()
            == "S-1-5-18"
}
