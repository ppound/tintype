//! Handing a photo to the operating system: open it in the default app,
//! (on Windows) print it with the system's print dialog, or show it in the
//! file manager.

use std::path::{Path, PathBuf};

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

/// Opens `folder` in the file manager with `files` (all in `folder`)
/// selected. Can take a while (a network folder), so call it off the UI thread.
#[cfg(windows)]
pub fn reveal(folder: &Path, files: &[PathBuf]) -> Result<(), String> {
    use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
    use windows_sys::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows_sys::Win32::UI::Shell::{ILCreateFromPathW, ILFree, SHOpenFolderAndSelectItems};

    let pidl = |p: &Path| {
        let wide = wide(p.as_os_str());
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        unsafe { ILCreateFromPathW(wide.as_ptr()) as *const ITEMIDLIST }
    };
    // SAFETY: COM is set up for this thread before the shell calls (and only
    // torn down if this call set it up); every item list is freed once.
    unsafe {
        let com = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let dir = pidl(folder);
        let result = if dir.is_null() {
            Err("The folder isn't there any more.".to_string())
        } else {
            let items: Vec<*const ITEMIDLIST> = files.iter().map(|f| pidl(f)).filter(|p| !p.is_null()).collect();
            let hr = SHOpenFolderAndSelectItems(dir, items.len() as u32, items.as_ptr(), 0);
            for item in items {
                ILFree(item);
            }
            ILFree(dir);
            if hr < 0 { Err(format!("Windows could not open the folder (error {hr:#x}).")) } else { Ok(()) }
        };
        if com >= 0 {
            CoUninitialize();
        }
        result
    }
}

#[cfg(target_os = "macos")]
pub fn reveal(_folder: &Path, files: &[PathBuf]) -> Result<(), String> {
    std::process::Command::new("open").arg("-R").args(files).spawn().map(drop).map_err(|e| format!("open: {e}"))
}

/// Linux file managers differ in how to select files, so this opens the folder.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn reveal(folder: &Path, _files: &[PathBuf]) -> Result<(), String> {
    spawn("xdg-open", folder)
}

/// Brings the window of another running Tintype to the front: process
/// `pid`'s, or any but this one's if `None`. False if there's none to be found
/// (the library may be open in `tintype-cli`).
#[cfg(windows)]
pub fn focus_other_instance(pid: Option<u32>) -> bool {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, SW_RESTORE,
        SetForegroundWindow, ShowWindow,
    };

    struct Search {
        pid: Option<u32>,
        found: HWND,
    }

    /// Stops at the first visible window titled "Tintype" of another tintype.exe.
    unsafe extern "system" fn visit(hwnd: HWND, search: LPARAM) -> windows_sys::core::BOOL {
        // SAFETY: `search` points to the Search in `focus_other_instance`, alive during EnumWindows.
        let search = unsafe { &mut *(search as *mut Search) };
        let mut title = [0u16; 16];
        // SAFETY: `hwnd` comes from EnumWindows, and the buffer's length is passed.
        let (n, visible, pid) = unsafe {
            let n = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32);
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            (n, IsWindowVisible(hwnd) != 0, pid)
        };
        let named = String::from_utf16_lossy(&title[..n.max(0) as usize]) == "Tintype";
        // SAFETY: no preconditions.
        let other = pid != unsafe { GetCurrentProcessId() };
        let wanted = search.pid.is_none_or(|p| p == pid);
        if visible && named && other && wanted && process_is_tintype(pid) {
            search.found = hwnd;
            return 0;
        }
        1
    }

    let mut search = Search { pid, found: std::ptr::null_mut() };
    // SAFETY: the callback only uses the pointer passed here, during the call.
    unsafe { EnumWindows(Some(visit), &mut search as *mut Search as LPARAM) };
    let found = search.found;
    if found.is_null() {
        return false;
    }
    // SAFETY: `found` is a window handle EnumWindows gave us.
    unsafe {
        if IsIconic(found) != 0 {
            ShowWindow(found, SW_RESTORE);
        }
        SetForegroundWindow(found);
    }
    true
}

/// Whether process `pid` runs a file named tintype.exe.
#[cfg(windows)]
fn process_is_tintype(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };

    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: the handle is closed once; the buffer's length is passed in `len`.
    let ok = unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return false;
        }
        let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) != 0;
        CloseHandle(process);
        ok
    };
    let path = PathBuf::from(String::from_utf16_lossy(&buf[..len as usize]));
    ok && path.file_name().is_some_and(|n| n.eq_ignore_ascii_case("tintype.exe"))
}

/// Elsewhere, the caller says Tintype is open already.
#[cfg(not(windows))]
pub fn focus_other_instance(_pid: Option<u32>) -> bool {
    false
}

#[cfg(not(windows))]
fn spawn(program: &str, path: &Path) -> Result<(), String> {
    std::process::Command::new(program).arg(path).spawn().map(drop).map_err(|e| format!("{program}: {e}"))
}

#[cfg(windows)]
fn shell_execute(verb: Option<&str>, path: &Path) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

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

/// A NUL-terminated UTF-16 copy of `s`, for Windows calls.
#[cfg(windows)]
fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain([0]).collect()
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
