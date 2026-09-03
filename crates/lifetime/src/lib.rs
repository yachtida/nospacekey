//! Process/DLL lifetime publication used by version-tree reclamation.

#[cfg(windows)]
mod windows_impl {
    use std::io::Read;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::sync::atomic::{AtomicIsize, Ordering};
    use std::time::{Duration, Instant};
    use windows::core::{PCSTR, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, E_FAIL, GENERIC_READ, HANDLE, HMODULE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, LockFileEx, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, OPEN_EXISTING,
    };
    use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::{CreateMutexA, GetCurrentProcessId};

    const TASK_TRANSACTION_GATE_BYTES: &[u8] =
        include_bytes!("../../../installer/task-transaction-gate");
    use windows::Win32::System::IO::OVERLAPPED;

    const LEASE_PREFIX: &[u8] = concat!(
        "Global\\nospacekey-version-lease-",
        env!("CARGO_PKG_VERSION"),
        "-s"
    )
    .as_bytes();
    static DLL_LEASE: AtomicIsize = AtomicIsize::new(0);
    static DLL_TREE_LEASE: AtomicIsize = AtomicIsize::new(0);

    pub struct VersionLease {
        named: HANDLE,
        tree: HANDLE,
    }

    pub struct TaskTransactionLease {
        _gate: std::fs::File,
        _journal: Option<std::fs::File>,
    }

    impl TaskTransactionLease {
        pub fn acquire() -> Result<Self, String> {
            let exe = std::env::current_exe()
                .map_err(|error| format!("task transaction executable path failed: {error}"))?;
            let program_files = std::env::var_os("ProgramFiles")
                .ok_or_else(|| "Program Files path is unavailable".to_string())?;
            let root = installed_root_for_executable(&exe, std::path::Path::new(&program_files))?;
            Self::acquire_at(root, Duration::from_secs(4), true)
        }

        fn acquire_at(
            root: &std::path::Path,
            timeout: Duration,
            validate_acl: bool,
        ) -> Result<Self, String> {
            use std::os::windows::io::AsRawHandle;
            use windows::Win32::Storage::FileSystem::{
                LockFileEx, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
                FILE_SHARE_READ, FILE_SHARE_WRITE, LOCKFILE_EXCLUSIVE_LOCK,
                LOCKFILE_FAIL_IMMEDIATELY,
            };
            use windows::Win32::System::IO::OVERLAPPED;

            let path = root.join(".nospacekey-task-transaction");
            let mut gate = std::fs::OpenOptions::new()
                .read(true)
                .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
                .open(&path)
                .map_err(|error| format!("task transaction gate open failed: {error}"))?;
            let metadata = gate
                .metadata()
                .map_err(|error| format!("task transaction gate metadata failed: {error}"))?;
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
                return Err("task transaction gate is a reparse point".into());
            }
            let mut contents = Vec::new();
            gate.read_to_end(&mut contents)
                .map_err(|error| format!("task transaction gate read failed: {error}"))?;
            if contents != TASK_TRANSACTION_GATE_BYTES {
                return Err("task transaction gate content mismatch".into());
            }
            if validate_acl {
                validate_task_artifact_acl(
                    &gate,
                    "O:BAG:SYD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;GR;;;BU)",
                )?;
            }
            let started = Instant::now();
            loop {
                let mut overlapped = OVERLAPPED::default();
                let result = unsafe {
                    LockFileEx(
                        windows::Win32::Foundation::HANDLE(gate.as_raw_handle()),
                        LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                        None,
                        1,
                        0,
                        &mut overlapped,
                    )
                };
                if result.is_ok() {
                    break;
                }
                if started.elapsed() >= timeout {
                    return Err("task transaction gate timed out".into());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let transaction_root = root.join(".nospacekey-uninstall");
            let journal = if transaction_root.exists() {
                use windows::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
                let directory = std::fs::OpenOptions::new()
                    .read(true)
                    .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
                    .custom_flags((FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS).0)
                    .open(&transaction_root)
                    .map_err(|error| {
                        format!("task transaction journal directory failed: {error}")
                    })?;
                let metadata = directory.metadata().map_err(|error| {
                    format!("task transaction journal directory metadata failed: {error}")
                })?;
                if !metadata.is_dir()
                    || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
                {
                    return Err("task transaction journal directory is unsafe".into());
                }
                if validate_acl {
                    validate_task_artifact_acl(
                        &directory,
                        "O:BAG:SYD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GR;;;BU)",
                    )?;
                }
                let mut entries = std::fs::read_dir(&transaction_root)
                    .map_err(|error| format!("task transaction journal scan failed: {error}"))?;
                if entries
                    .next()
                    .transpose()
                    .map_err(|error| format!("task transaction journal entry failed: {error}"))?
                    .is_some()
                {
                    return Err("an interrupted uninstall blocks scheduled task mutation".into());
                }
                Some(directory)
            } else {
                None
            };
            Ok(Self {
                _gate: gate,
                _journal: journal,
            })
        }
    }

    fn installed_root_for_executable<'a>(
        exe: &'a std::path::Path,
        program_files: &std::path::Path,
    ) -> Result<&'a std::path::Path, String> {
        let bin = exe
            .parent()
            .ok_or_else(|| "task transaction executable directory is missing".to_string())?;
        if bin.parent().and_then(|path| path.file_name()) != Some(std::ffi::OsStr::new("versions"))
        {
            return Err("scheduled task mutation is unavailable from a direct development layout; stage the build elevated under the installed versions directory".into());
        }
        let root = bin
            .parent()
            .and_then(|path| path.parent())
            .ok_or_else(|| "task transaction installation root is missing".to_string())?;
        let expected = program_files.join("nospacekey");
        if !root
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&expected.as_os_str().to_string_lossy())
        {
            return Err("scheduled task mutation is unavailable from a direct development layout; stage the build elevated under the installed versions directory".into());
        }
        Ok(root)
    }

    fn validate_task_artifact_acl(file: &std::fs::File, expected: &str) -> Result<(), String> {
        use std::os::windows::io::AsRawHandle;
        use windows::core::PWSTR;
        use windows::Win32::Foundation::{LocalFree, HLOCAL, WIN32_ERROR};
        use windows::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
            SE_FILE_OBJECT,
        };
        use windows::Win32::Security::{
            DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR,
        };

        let information =
            OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        let status = unsafe {
            GetSecurityInfo(
                windows::Win32::Foundation::HANDLE(file.as_raw_handle()),
                SE_FILE_OBJECT,
                information,
                None,
                None,
                None,
                None,
                Some(&mut descriptor),
            )
        };
        if status != WIN32_ERROR(0) {
            return Err(format!("task transaction ACL query failed: {}", status.0));
        }
        let mut text = PWSTR::null();
        let converted = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor,
                SDDL_REVISION_1,
                information,
                &mut text,
                None,
            )
        };
        let actual = match converted {
            Ok(()) => unsafe { text.to_string() }
                .map_err(|error| format!("task transaction ACL text failed: {error}")),
            Err(error) => Err(format!("task transaction ACL conversion failed: {error}")),
        };
        unsafe {
            let _ = LocalFree(Some(HLOCAL(text.0.cast())));
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        }
        if actual? != expected {
            return Err("task transaction artifact ACL mismatch".into());
        }
        Ok(())
    }

    impl VersionLease {
        pub fn acquire() -> Result<Self, windows::core::Error> {
            let name = lease_name()?;
            let named = unsafe { CreateMutexA(None, false, PCSTR(name.as_ptr())) }?;
            let tree = match unsafe { open_module_lifetime_sentinel(None) } {
                Ok(tree) => tree,
                Err(error) => {
                    unsafe { CloseHandle(named).ok() };
                    return Err(error);
                }
            };
            Ok(Self { named, tree })
        }
    }

    fn lease_name() -> Result<[u8; 128], windows::core::Error> {
        let mut session = 0u32;
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }?;
        let mut name = [0u8; 128];
        if LEASE_PREFIX.len() + 10 >= name.len() {
            return Err(windows::core::Error::from_hresult(E_FAIL));
        }
        name[..LEASE_PREFIX.len()].copy_from_slice(LEASE_PREFIX);
        let mut digits = [0u8; 10];
        let mut value = session;
        let mut start = digits.len();
        loop {
            start -= 1;
            digits[start] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        let count = digits.len() - start;
        name[LEASE_PREFIX.len()..LEASE_PREFIX.len() + count].copy_from_slice(&digits[start..]);
        Ok(name)
    }

    impl Drop for VersionLease {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.tree).ok();
                CloseHandle(self.named).ok();
            }
        }
    }

    unsafe fn open_module_lifetime_sentinel(
        module: Option<HMODULE>,
    ) -> Result<HANDLE, windows::core::Error> {
        const SENTINEL: &[u16] = &[
            b'.' as u16,
            b'n' as u16,
            b'o' as u16,
            b's' as u16,
            b'p' as u16,
            b'a' as u16,
            b'c' as u16,
            b'e' as u16,
            b'k' as u16,
            b'e' as u16,
            b'y' as u16,
            b'-' as u16,
            b'l' as u16,
            b'i' as u16,
            b'f' as u16,
            b'e' as u16,
            b't' as u16,
            b'i' as u16,
            b'm' as u16,
            b'e' as u16,
            0,
        ];
        let mut path = [0u16; 1024];
        let length = GetModuleFileNameW(module, &mut path) as usize;
        if length == 0 || length >= path.len() {
            return Err(windows::core::Error::from_hresult(E_FAIL));
        }
        let Some(separator) = path[..length]
            .iter()
            .rposition(|unit| *unit == b'\\' as u16)
        else {
            return Err(windows::core::Error::from_hresult(E_FAIL));
        };
        if separator + 1 + SENTINEL.len() > path.len() {
            return Err(windows::core::Error::from_hresult(E_FAIL));
        }
        path[separator + 1..separator + 1 + SENTINEL.len()].copy_from_slice(SENTINEL);
        open_lifetime_sentinel_path(PCWSTR(path.as_ptr()), false)
    }

    unsafe fn open_lifetime_sentinel_path(
        path: PCWSTR,
        exclusive: bool,
    ) -> Result<HANDLE, windows::core::Error> {
        let handle = CreateFileW(
            path,
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )?;
        let mut overlapped = OVERLAPPED::default();
        let flags = if exclusive {
            LOCKFILE_FAIL_IMMEDIATELY | LOCKFILE_EXCLUSIVE_LOCK
        } else {
            LOCKFILE_FAIL_IMMEDIATELY
        };
        if let Err(error) = LockFileEx(handle, flags, None, 1, 0, &mut overlapped) {
            CloseHandle(handle).ok();
            return Err(error);
        }
        Ok(handle)
    }

    /// Called only from DLL_PROCESS_ATTACH. It creates/probes kernel objects but never waits.
    pub unsafe fn dll_process_attach(module: HMODULE) -> bool {
        let Ok(name) = lease_name() else {
            return false;
        };
        let Ok(handle) = CreateMutexA(None, false, PCSTR(name.as_ptr())) else {
            return false;
        };
        let Ok(tree) = open_module_lifetime_sentinel(Some(module)) else {
            CloseHandle(handle).ok();
            return false;
        };
        DLL_TREE_LEASE.store(tree.0 as isize, Ordering::Release);
        DLL_LEASE.store(handle.0 as isize, Ordering::Release);
        true
    }

    /// Called only from DLL_PROCESS_DETACH and never waits.
    pub unsafe fn dll_process_detach() {
        let raw = DLL_LEASE.swap(0, Ordering::AcqRel);
        if raw != 0 {
            CloseHandle(HANDLE(raw as *mut _)).ok();
        }
        let tree = DLL_TREE_LEASE.swap(0, Ordering::AcqRel);
        if tree != 0 {
            CloseHandle(HANDLE(tree as *mut _)).ok();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows::core::HSTRING;

        #[test]
        fn task_transaction_gate_rejects_pending_uninstall_and_tamper() {
            let fixture = std::env::temp_dir().join(format!(
                "nospacekey-task-gate-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&fixture).unwrap();
            let gate = fixture.join(".nospacekey-task-transaction");
            std::fs::write(&gate, TASK_TRANSACTION_GATE_BYTES).unwrap();
            let held =
                TaskTransactionLease::acquire_at(&fixture, Duration::from_millis(100), false)
                    .unwrap();
            assert!(
                TaskTransactionLease::acquire_at(&fixture, Duration::from_millis(100), false)
                    .is_err()
            );
            assert!(std::fs::rename(&gate, fixture.join("replacement")).is_err());
            drop(held);
            let journal = fixture.join(".nospacekey-uninstall");
            std::fs::create_dir(&journal).unwrap();
            std::fs::write(journal.join("pending-1.0.0.json"), b"pending").unwrap();
            assert!(
                TaskTransactionLease::acquire_at(&fixture, Duration::from_millis(100), false)
                    .is_err()
            );
            std::fs::remove_dir_all(&journal).unwrap();
            std::fs::write(&gate, b"foreign").unwrap();
            assert!(
                TaskTransactionLease::acquire_at(&fixture, Duration::from_millis(100), false)
                    .is_err()
            );
            std::fs::remove_dir_all(fixture).unwrap();
        }

        #[test]
        fn direct_development_layout_explicitly_refuses_scheduled_task_mutation() {
            let direct = std::path::Path::new(r"D:\dev\target\debug\NospacekeyConfig.exe");
            let program_files = std::path::Path::new(r"C:\Program Files");
            let error = installed_root_for_executable(direct, program_files).unwrap_err();
            assert!(error.contains("direct development layout"));
            assert!(installed_root_for_executable(
                std::path::Path::new(r"C:\Temp\nospacekey\NospacekeyConfig.exe"),
                program_files
            )
            .is_err());
            assert!(installed_root_for_executable(
                std::path::Path::new(
                    r"C:\Program Files\nospacekey\versionz\1.2.3\NospacekeyConfig.exe"
                ),
                program_files
            )
            .is_err());
            assert!(installed_root_for_executable(
                std::path::Path::new(r"D:\scratch\versions\1.2.3\NospacekeyConfig.exe"),
                program_files
            )
            .is_err());

            let installed = std::path::Path::new(
                r"C:\Program Files\nospacekey\versions\1.2.3\NospacekeyConfig.exe",
            );
            assert_eq!(
                installed_root_for_executable(installed, program_files).unwrap(),
                std::path::Path::new(r"C:\Program Files\nospacekey")
            );
        }

        #[test]
        fn active_shared_claim_rejects_an_exclusive_cleanup_claim() {
            let fixture = std::env::temp_dir().join(format!(
                "nospacekey-lifetime-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&fixture).unwrap();
            let sentinel = fixture.join(".nospacekey-lifetime");
            std::fs::write(&sentinel, b"fixture").unwrap();
            let wide = HSTRING::from(sentinel.as_os_str());
            let lease =
                unsafe { open_lifetime_sentinel_path(PCWSTR(wide.as_ptr()), false) }.unwrap();
            assert!(unsafe { open_lifetime_sentinel_path(PCWSTR(wide.as_ptr()), true) }.is_err());
            unsafe { CloseHandle(lease).unwrap() };
            let claim =
                unsafe { open_lifetime_sentinel_path(PCWSTR(wide.as_ptr()), true) }.unwrap();
            unsafe { CloseHandle(claim).unwrap() };
            std::fs::remove_dir_all(fixture).unwrap();
        }

        #[test]
        fn lease_name_contains_only_the_exact_build_identity() {
            let name = lease_name().unwrap();
            let end = name.iter().position(|byte| *byte == 0).unwrap();
            let name = std::str::from_utf8(&name[..end]).unwrap();
            assert_eq!(
                name.rsplit_once("-s").unwrap().0,
                format!(
                    r"Global\nospacekey-version-lease-{}",
                    env!("CARGO_PKG_VERSION")
                )
            );
        }
    }
}

#[cfg(windows)]
pub use windows_impl::{
    dll_process_attach, dll_process_detach, TaskTransactionLease, VersionLease,
};

#[cfg(not(windows))]
pub struct VersionLease;

#[cfg(not(windows))]
pub struct TaskTransactionLease;

#[cfg(not(windows))]
impl VersionLease {
    pub fn acquire() -> Result<Self, std::io::Error> {
        Ok(Self)
    }
}

#[cfg(not(windows))]
impl TaskTransactionLease {
    pub fn acquire() -> Result<Self, String> {
        Ok(Self)
    }
}
