//! Authenticate the server before any production IPC frame (including StartSession).
use std::fs::File;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{HANDLE, HMODULE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetFinalPathNameByHandleW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, VOLUME_NAME_NT,
};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

const ENGINE_EXE: &str = "NospacekeyEngineHost.exe";

pub(crate) fn is_engine_pipe(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == r"\\.\pipe\nospacekey-engine" || name.starts_with(r"\\.\pipe\nospacekey-engine.")
}

pub(crate) fn verify_engine_server(pipe: &File) -> io::Result<()> {
    // This code is statically linked into the TIP DLL and config EXE. current_exe()
    // in a TIP would point at the editor/browser hosting it instead.
    let expected = module_engine_path()?;
    verify_server_at(pipe, &expected)
}

pub(crate) fn verify_server_at(pipe: &File, expected: &Path) -> io::Result<()> {
    let actual = server_image_path(pipe)?;
    let actual = canonical_image_path(&actual)?;
    let expected = canonical_image_path(expected)?;
    if same_path(&actual, &expected) {
        return Ok(());
    }
    let program_files = std::env::var_os("ProgramFiles")
        .ok_or_else(|| io::Error::other("Program Files path unavailable"))?;
    let program_files = canonical_image_path(Path::new(&program_files))?;
    if trusted_image_paths_match(&actual, &expected, &program_files) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "engine server image mismatch: {} (expected {})",
                actual.display(),
                expected.display(),
            ),
        ))
    }
}

fn canonical_image_path(path: &Path) -> io::Result<PathBuf> {
    // AppContainers cannot query the Mount Manager to obtain a DOS volume name.
    // Keep normalized NT paths throughout, still resolving junctions/symlinks.
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    }
    .map_err(io::Error::other)?;
    let _handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
    let mut buffer = vec![0u16; 32768];
    let length = unsafe { GetFinalPathNameByHandleW(handle, &mut buffer, VOLUME_NAME_NT) } as usize;
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    if length >= buffer.len() {
        return Err(io::Error::other("engine image path too long"));
    }
    Ok(PathBuf::from(std::ffi::OsString::from_wide(
        &buffer[..length],
    )))
}

fn module_engine_path() -> io::Result<PathBuf> {
    let mut module = HMODULE::default();
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(module_engine_path as *const () as *const u16),
            &mut module,
        )
        .map_err(io::Error::other)?;
    }
    let mut buffer = vec![0u16; 32768];
    let length = unsafe { GetModuleFileNameW(Some(module), &mut buffer) } as usize;
    if length == 0 || length >= buffer.len() {
        return Err(io::Error::last_os_error());
    }
    let module = PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length]));
    let directory = module
        .parent()
        .ok_or_else(|| io::Error::other("engine module directory missing"))?;
    Ok(directory.join(ENGINE_EXE))
}

pub(crate) fn server_image_path(pipe: &File) -> io::Result<PathBuf> {
    let mut pid = 0;
    unsafe { GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut pid) }
        .map_err(io::Error::other)?;
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .map_err(io::Error::other)?;
    let _process = unsafe { OwnedHandle::from_raw_handle(process.0) };
    let mut buffer = vec![0u16; 32768];
    let mut length = buffer.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    }
    .map_err(io::Error::other)?;
    Ok(PathBuf::from(std::ffi::OsString::from_wide(
        &buffer[..length as usize],
    )))
}

fn same_path(a: &Path, b: &Path) -> bool {
    a.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
}

// Inputs are canonical paths: a junction or symlink cannot turn an untrusted
// executable into an installed one. Version equality is intentionally not required
// inside the protected install root; the wire handshake checks protocol compatibility.
fn trusted_image_paths_match(actual: &Path, expected: &Path, program_files: &Path) -> bool {
    if same_path(actual, expected) {
        return true;
    }
    let versions = program_files.join("nospacekey").join("versions");
    let installed = |path: &Path| {
        path.file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case(ENGINE_EXE))
            && path
                .parent()
                .and_then(Path::parent)
                .is_some_and(|parent| same_path(parent, &versions))
    };
    installed(expected) && installed(actual)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_engines_can_cross_product_versions_but_not_install_roots() {
        let pf = Path::new(r"C:\Program Files");
        let expected =
            Path::new(r"C:\Program Files\nospacekey\versions\1.6.0\NospacekeyEngineHost.exe");
        for actual in [
            r"C:\Program Files\nospacekey\versions\1.5.0\NospacekeyEngineHost.exe",
            r"C:\Program Files\nospacekey\versions\1.6.1\NospacekeyEngineHost.exe",
        ] {
            assert!(trusted_image_paths_match(Path::new(actual), expected, pf));
        }
        for actual in [
            r"C:\Users\attacker\NospacekeyEngineHost.exe",
            r"C:\Program Files\nospacekey-fake\versions\1.6.0\NospacekeyEngineHost.exe",
            r"C:\Program Files\nospacekey\versions\1.6.0\impostor.exe",
            r"C:\Program Files\nospacekey\versions\1.6.0\subdir\NospacekeyEngineHost.exe",
        ] {
            assert!(!trusted_image_paths_match(Path::new(actual), expected, pf));
        }
    }

    #[test]
    fn development_layout_only_accepts_its_exact_sibling_engine() {
        let expected = Path::new(r"D:\dev\NospacekeyEngineHost.exe");
        let pf = Path::new(r"C:\Program Files");
        assert!(trusted_image_paths_match(expected, expected, pf));
        assert!(!trusted_image_paths_match(
            Path::new(r"D:\other\NospacekeyEngineHost.exe"),
            expected,
            pf
        ));
    }

    #[test]
    fn production_pipe_aliases_all_require_server_verification() {
        assert!(is_engine_pipe(&crate::client::stable_pipe_name()));
        assert!(is_engine_pipe(r"\\.\pipe\NOSPACEKEY-ENGINE.v10.s42"));
        assert!(is_engine_pipe(r"\\.\pipe\nospacekey-engine.s42"));
        assert!(is_engine_pipe(r"\\.\pipe\nospacekey-engine"));
        assert!(!is_engine_pipe(r"\\.\pipe\nospacekey-test-42"));
    }
}
