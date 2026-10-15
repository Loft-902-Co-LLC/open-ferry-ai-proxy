// Shared Windows directory-alias fixture. Never opens a browser or console.
fn junction(target: &std::path::Path, link: &std::path::Path) {
    use std::os::windows::process::CommandExt;
    let status = std::process::Command::new("cmd.exe")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .creation_flags(0x0800_0000)
        .output()
        .unwrap();
    assert!(status.status.success(), "could not create test junction");
}
