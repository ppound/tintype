//! Handing a photo to the operating system: open it in the default app, or
//! (on Windows) print it with the system's print dialog.

use std::path::Path;

/// Whether [`print`] shows a print dialog here. Elsewhere, open the photo
/// and print from the viewer.
pub const CAN_PRINT: bool = cfg!(windows);

/// Opens `path` in the app the system uses for that kind of file.
#[cfg(windows)]
pub fn open(path: &Path) -> Result<(), String> {
    shell_execute(None, path)
}

#[cfg(target_os = "macos")]
pub fn open(path: &Path) -> Result<(), String> {
    spawn("open", path)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn open(path: &Path) -> Result<(), String> {
    spawn("xdg-open", path)
}

/// Shows the system's print dialog for `path`.
#[cfg(windows)]
pub fn print(path: &Path) -> Result<(), String> {
    shell_execute(Some("print"), path)
}

#[cfg(not(windows))]
pub fn print(path: &Path) -> Result<(), String> {
    open(path)
}

#[cfg(not(windows))]
fn spawn(program: &str, path: &Path) -> Result<(), String> {
    std::process::Command::new(program).arg(path).spawn().map(drop).map_err(|e| format!("{program}: {e}"))
}

#[cfg(windows)]
fn shell_execute(verb: Option<&str>, path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let wide = |s: &std::ffi::OsStr| s.encode_wide().chain([0]).collect::<Vec<u16>>();
    let file = wide(path.as_os_str());
    let verb = verb.map(|v| wide(v.as_ref()));
    let verb_ptr = verb.as_ref().map_or(std::ptr::null(), |v| v.as_ptr());
    // SAFETY: both strings are NUL-terminated and outlive the call.
    let code = unsafe {
        ShellExecuteW(std::ptr::null_mut(), verb_ptr, file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL)
    } as isize;
    // Values above 32 mean success.
    match code {
        c if c > 32 => Ok(()),
        // SE_ERR_NOASSOC: nothing is set up to open (or print) this kind of file.
        31 if verb.is_some() => {
            Err("Windows has no app set up to print this kind of file. Open it, and print from that app.".into())
        }
        31 => Err("Windows has no app set up to open this kind of file.".into()),
        2 | 3 => Err("The file isn't there any more.".into()),
        c => Err(format!("Windows could not do it (error {c}).")),
    }
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn a_missing_file_is_an_error_not_a_dialog() {
        let dir = tempfile::tempdir().unwrap();
        let err = super::print(&dir.path().join("gone.jpg")).unwrap_err();
        assert_eq!(err, "The file isn't there any more.");
    }
}
