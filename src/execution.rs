//! Cooperative exclusion between Service supervision and local Sidecar execution.
#[cfg(unix)]
use anyhow::bail;
use anyhow::{Context as _, Result};
#[cfg(unix)]
use std::fs::OpenOptions;
use std::{
    fs::File,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub struct CoreExecutionGuard {
    #[cfg(unix)]
    _file: File,
    #[cfg(windows)]
    _reservation: windows_lock::Reservation,
}

#[cfg(unix)]
impl Drop for CoreExecutionGuard {
    fn drop(&mut self) {
        // fork can briefly inherit this open file description before exec closes it.
        let _ = self._file.unlock();
    }
}

impl CoreExecutionGuard {
    pub fn acquire() -> Result<Self> {
        #[cfg(unix)]
        return Self::acquire_at(&coordination_path()?);
        #[cfg(windows)]
        {
            Self::acquire_windows_at(
                windows_lock::NAME,
                &coordination_path()?,
                windows_fallback::require_no_registered_services,
                || windows_fallback::require_no_core_process(false),
            )
        }
    }

    #[cfg(windows)]
    fn acquire_windows_at(
        name: &str,
        path: &Path,
        require_services_absent: impl FnOnce() -> Result<()>,
        require_cores_absent: impl FnOnce() -> Result<()>,
    ) -> Result<Self> {
        let reservation = windows_lock::Reservation::try_acquire(name, path)?
            .context("another Service or Sidecar owns core execution")?;
        if !reservation.has_legacy_lock() {
            require_services_absent()?;
        }
        require_cores_absent()?;
        Ok(Self {
            _reservation: reservation,
        })
    }

    #[cfg(any(unix, test))]
    fn acquire_at(path: &Path) -> Result<Self> {
        #[cfg(windows)]
        {
            Self::acquire_windows_at(&test_coordination_name(path), path, || Ok(()), || Ok(()))
        }
        #[cfg(unix)]
        {
            let file = open_coordination_file(path)?;
            match file.try_lock() {
                Ok(()) => Ok(Self { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) => bail!("another Service or Sidecar owns core execution"),
                Err(std::fs::TryLockError::Error(error)) => Err(error).context("could not reserve core execution"),
            }
        }
    }

    /// Keep the reservation until the OS confirms exit, even if killing or event delivery fails.
    pub fn release_after_exit(
        self,
        pid: u32,
        mut terminated: tokio::sync::oneshot::Receiver<()>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut events_open = true;
            while !process_has_exited(pid) {
                tokio::select! {
                    result = &mut terminated, if events_open => {
                        if result.is_ok() { break; }
                        events_open = false;
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
                }
            }
            drop(self);
        })
    }

    pub fn is_held() -> Result<bool> {
        #[cfg(windows)]
        {
            Ok(windows_lock::Reservation::try_acquire(windows_lock::NAME, &coordination_path()?)?.is_none())
        }
        #[cfg(unix)]
        {
            let file = open_coordination_file(&coordination_path()?)?;
            match file.try_lock() {
                Ok(()) => {
                    file.unlock()?;
                    Ok(false)
                }
                Err(std::fs::TryLockError::WouldBlock) => Ok(true),
                Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
            }
        }
    }
}

#[cfg(all(windows, test))]
fn test_coordination_name(path: &Path) -> String {
    use sha2::{Digest as _, Sha256};
    let digest: String = Sha256::digest(path.as_os_str().as_encoded_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!(r"Global\clash-verge-execution-test-{digest}")
}

#[cfg(unix)]
fn process_has_exited(pid: u32) -> bool {
    (unsafe { platform_lib::kill(pid as i32, 0) }) == -1
        && std::io::Error::last_os_error().raw_os_error() == Some(platform_lib::ESRCH)
}

#[cfg(windows)]
fn process_has_exited(pid: u32) -> bool {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::{ERROR_INVALID_PARAMETER, WAIT_OBJECT_0},
        System::Threading::{OpenProcess, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject},
    };
    let raw = unsafe { OpenProcess(SYNCHRONIZATION_SYNCHRONIZE, 0, pid) };
    if raw.is_null() {
        return std::io::Error::last_os_error().raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32);
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    (unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) }) == WAIT_OBJECT_0
}

fn coordination_path() -> Result<PathBuf> {
    #[cfg(unix)]
    let root = PathBuf::from("/tmp");
    #[cfg(windows)]
    let root = {
        use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;
        let mut buffer = vec![0u16; 32768];
        let size = unsafe { GetWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
        anyhow::ensure!(
            size > 0 && size < buffer.len(),
            "could not locate Windows coordination directory"
        );
        PathBuf::from(String::from_utf16(&buffer[..size])?).join("Temp")
    };
    // Both channels may keep idle helpers, but core execution is shared.
    let name = format!("{}.lock", crate::channel::CORE_LOCK_NAME);
    Ok(root.join(name))
}

#[cfg(unix)]
fn open_coordination_file(path: &Path) -> Result<File> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o444)
        .open(path)
    {
        Ok(file) => {
            file.set_permissions(std::fs::Permissions::from_mode(0o444))?;
            Ok(file)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(platform_lib::O_NOFOLLOW | platform_lib::O_NONBLOCK)
                .open(path)?;
            anyhow::ensure!(file.metadata()?.is_file(), "coordination entry is not a regular file");
            Ok(file)
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
fn open_coordination_file(path: &Path) -> Result<File> {
    use std::os::windows::{ffi::OsStrExt as _, io::FromRawHandle as _};
    use windows_sys::Win32::{
        Foundation::{INVALID_HANDLE_VALUE, LocalFree},
        Security::{
            Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
            SECURITY_ATTRIBUTES,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_ALWAYS,
        },
    };
    let sddl: Vec<u16> = "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;OW)(A;;GR;;;AU)\0"
        .encode_utf16()
        .collect();
    let mut descriptor = std::ptr::null_mut();
    anyhow::ensure!(
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } != 0,
        "could not create coordination permissions"
    );
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0x80000000,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes,
            OPEN_ALWAYS,
            FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    let error = std::io::Error::last_os_error();
    unsafe {
        LocalFree(descriptor);
    }
    if handle == INVALID_HANDLE_VALUE {
        return Err(error).with_context(|| format!("could not open or create legacy core execution lock {path:?}"));
    }
    let file = unsafe { File::from_raw_handle(handle) };
    anyhow::ensure!(
        file.metadata()?.is_file() && !std::fs::symlink_metadata(path)?.file_type().is_symlink(),
        "coordination entry is not a regular file"
    );
    Ok(file)
}

#[cfg(feature = "client")]
pub async fn check_sidecar_available() -> Result<()> {
    // GUI lifecycle requests can retry for minutes; occupancy checks need their own deadline.
    let inspection = tokio::time::timeout(std::time::Duration::from_secs(1), crate::inspect_installation(&[])).await;
    let ipc_failed = match inspection {
        Ok(Ok(status)) => {
            anyhow::ensure!(
                status
                    .protocol
                    .supports_client(crate::ProtocolVersion::current(), crate::MIN_REQUIRED_SERVICE_REVISION),
                "service upgrade is required before Sidecar handoff"
            );
            anyhow::ensure!(
                !status.core_busy,
                "the service has an active or recovering core session"
            );
            false
        }
        Ok(Err(_)) | Err(_) => true,
    };
    tokio::task::spawn_blocking(move || -> Result<()> {
        #[cfg(windows)]
        {
            if ipc_failed {
                windows_fallback::require_stopped_service()?;
            }
            windows_fallback::require_no_core_process(false)?;
        }
        #[cfg(unix)]
        {
            let mut command = std::process::Command::new("ps");
            #[cfg(target_os = "linux")]
            command.args(["-axo", "pid=,comm="]);
            #[cfg(not(target_os = "linux"))]
            command.args(["-axo", "comm="]);
            let output = command.output().context("could not inspect remaining cores")?;
            anyhow::ensure!(output.status.success(), "could not inspect remaining cores");
            require_no_unix_core_processes(&String::from_utf8_lossy(&output.stdout), ipc_failed)?;
        }
        Ok(())
    })
    .await
    .context("core process inspection failed")?
}

#[cfg(all(unix, feature = "client"))]
fn require_no_unix_core_processes(processes: &str, include_service: bool) -> Result<()> {
    #[cfg(target_os = "linux")]
    let executables = linux_process_executables(processes, include_service)?;
    #[cfg(not(target_os = "linux"))]
    let executables = processes
        .lines()
        .map(|line| PathBuf::from(line.trim()))
        .collect::<Vec<_>>();
    require_no_core_executables(&executables, include_service)
}

#[cfg(all(target_os = "linux", feature = "client"))]
fn resolve_linux_process_executable(executable: std::io::Result<PathBuf>, cmdline: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt as _;
    match executable {
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            // Root helpers may deny ptrace access to exe while their cmdline remains readable.
            let arguments = std::fs::read(cmdline)
                .map_err(|error| std::io::Error::other(format!("could not read process command line: {error}")))?;
            let end = arguments
                .iter()
                .position(|byte| *byte == 0)
                .filter(|end| *end != 0)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "process command line has no complete argv[0]",
                    )
                })?;
            Ok(PathBuf::from(std::ffi::OsString::from_vec(arguments[..end].to_vec())))
        }
        result => result,
    }
}

#[cfg(all(target_os = "linux", feature = "client"))]
fn linux_process_executables(processes: &str, include_service: bool) -> Result<Vec<PathBuf>> {
    use std::os::unix::ffi::OsStringExt as _;
    let mut executables = Vec::new();
    for process in processes.lines() {
        let (pid, name) = process
            .trim()
            .split_once(char::is_whitespace)
            .context("process listing omitted its executable name")?;
        let name = name.trim();
        let is_core = [crate::CORE_NAME, crate::CORE_ALPHA_NAME]
            .iter()
            .any(|expected| process_name_matches(name, expected));
        let is_service = include_service && process_name_matches(name, crate::SERVICE_BINARY);
        if !is_core && !is_service {
            continue;
        }
        let pid = pid.parse::<u32>().context("process listing contains an invalid pid")?;
        match resolve_linux_process_executable(
            std::fs::read_link(format!("/proc/{pid}/exe")),
            &PathBuf::from(format!("/proc/{pid}/cmdline")),
        ) {
            Ok(executable) => {
                // Linux appends this marker when an upgrade unlinks a running executable.
                let executable =
                    if let Some(path) = executable.as_os_str().as_encoded_bytes().strip_suffix(b" (deleted)") {
                        PathBuf::from(std::ffi::OsString::from_vec(path.to_vec()))
                    } else {
                        executable
                    };
                executables.push(executable);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) if is_service => return Err(ResidualServiceError { process: name.into() }.into()),
            Err(error) => return Err(error).context("could not identify a potentially remaining core executable"),
        }
    }
    Ok(executables)
}

#[cfg(all(unix, feature = "client"))]
fn require_no_core_executables(executables: &[PathBuf], include_service: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    let other_helper = PathBuf::from("/Library/PrivilegedHelperTools")
        .join(format!("{}.bundle", crate::channel::OTHER_MACOS_SERVICE_ID))
        .join("Contents/MacOS")
        .join(crate::SERVICE_BINARY);
    #[cfg(target_os = "linux")]
    let other_helper = PathBuf::from("/var/lib")
        .join(crate::channel::OTHER_SERVICE_SLUG)
        .join("bin")
        .join(crate::SERVICE_BINARY);
    // A residual helper outranks a core so the refusal stays repairable in any ps order.
    let mut remaining_core = None;
    for executable in executables {
        let name = executable.file_name().and_then(|name| name.to_str()).unwrap_or("");
        if [crate::CORE_NAME, crate::CORE_ALPHA_NAME].contains(&name) {
            remaining_core.get_or_insert(name);
        }
        if include_service && name == crate::SERVICE_BINARY && executable != &other_helper {
            return Err(ResidualServiceError {
                process: executable.clone(),
            }
            .into());
        }
    }
    if let Some(name) = remaining_core {
        bail!("process {name} remains after IPC failure; refusing a second core");
    }
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "client"))]
fn process_name_matches(actual: &str, expected: &str) -> bool {
    actual == expected || actual == expected.chars().take(15).collect::<String>()
}

/// A Service helper still runs without answering IPC; only reinstalling the Service replaces it.
#[cfg(feature = "client")]
#[derive(Debug)]
pub struct ResidualServiceError {
    process: PathBuf,
}

#[cfg(feature = "client")]
impl std::fmt::Display for ResidualServiceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "service process {} is running without IPC; reinstall the service to replace it",
            self.process.display()
        )
    }
}

#[cfg(feature = "client")]
impl std::error::Error for ResidualServiceError {}

#[cfg(feature = "client")]
pub async fn reserve_sidecar() -> Result<CoreExecutionGuard> {
    check_sidecar_available().await?;
    CoreExecutionGuard::acquire()
}

#[cfg(windows)]
#[path = "execution_windows.rs"]
mod windows_fallback;

#[cfg(windows)]
#[path = "execution_windows_lock.rs"]
mod windows_lock;

#[cfg(all(windows, feature = "client", feature = "standalone", not(feature = "test")))]
pub(crate) fn reserve_legacy_install_repair() -> Result<CoreExecutionGuard> {
    anyhow::ensure!(
        unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() } != 0,
        "legacy service directory recovery requires an elevated installer"
    );
    let guard = CoreExecutionGuard::acquire()?;
    windows_fallback::require_stopped_service()
        .context("stop the service before repairing its legacy state directory")?;
    windows_fallback::require_no_core_process(true)
        .context("stop remaining service and core processes before repairing legacy state")?;
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    fn set_test_dacl(path: &Path, sddl: &str) {
        use std::os::windows::ffi::OsStrExt as _;
        use windows_sys::Win32::{
            Foundation::LocalFree,
            Security::{
                Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
                DACL_SECURITY_INFORMATION, SetFileSecurityW,
            },
        };
        let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut descriptor = std::ptr::null_mut();
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(converted, 0);
        let secured = unsafe { SetFileSecurityW(wide.as_ptr(), DACL_SECURITY_INFORMATION, descriptor) };
        unsafe {
            LocalFree(descriptor);
        }
        assert_ne!(secured, 0);
    }

    #[cfg(windows)]
    #[test]
    fn missing_legacy_lock_does_not_require_parent_write_access() -> Result<()> {
        let root = std::env::temp_dir().join(format!("execution-readonly-parent-{}", std::process::id()));
        std::fs::create_dir(&root)?;
        let path = root.join("core.lock");
        set_test_dacl(&root, "D:P(D;;0x2;;;WD)(A;;FA;;;AU)(A;;FA;;;SY)");
        let result = (|| -> Result<()> {
            let name = test_coordination_name(&path);
            for service_absent in [false, true] {
                let refused = CoreExecutionGuard::acquire_windows_at(
                    &name,
                    &path,
                    || {
                        assert!(CoreExecutionGuard::acquire_at(&path).is_err());
                        anyhow::ensure!(service_absent, "registered stopped service");
                        Ok(())
                    },
                    || {
                        assert!(service_absent);
                        assert!(CoreExecutionGuard::acquire_at(&path).is_err());
                        anyhow::bail!("surviving core after owner exit")
                    },
                );
                let message = refused.unwrap_err().to_string();
                assert!(message.contains(if service_absent {
                    "surviving core"
                } else {
                    "registered stopped service"
                }));
            }
            let first = CoreExecutionGuard::acquire_at(&path)?;
            assert!(CoreExecutionGuard::acquire_at(&path).is_err());
            drop(first);
            drop(CoreExecutionGuard::acquire_at(&path)?);
            Ok(())
        })();
        assert!(!path.exists());
        std::fs::remove_dir(&root)?;
        result
    }

    #[cfg(windows)]
    #[test]
    fn unreadable_legacy_file_fails_closed_without_leaking_the_mutex() -> Result<()> {
        let path = std::env::temp_dir().join(format!("execution-unreadable-{}", std::process::id()));
        std::fs::write(&path, b"")?;
        set_test_dacl(&path, "D:P(D;;GR;;;WD)(A;;FA;;;AU)(A;;FA;;;SY)");
        let refused = CoreExecutionGuard::acquire_at(&path);
        set_test_dacl(&path, "D:P(A;;FA;;;AU)(A;;FA;;;SY)");
        let message = refused.unwrap_err().to_string();
        assert!(message.contains("legacy core execution lock"));
        assert!(message.contains(path.file_name().unwrap().to_str().unwrap()));
        drop(CoreExecutionGuard::acquire_at(&path)?);
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn legacy_and_global_reservations_exclude_each_other() -> Result<()> {
        let path = std::env::temp_dir().join(format!("execution-legacy-bridge-{}", std::process::id()));
        let legacy = open_coordination_file(&path)?;
        legacy.try_lock()?;
        assert!(CoreExecutionGuard::acquire_at(&path).is_err());
        legacy.unlock()?;
        let modern = CoreExecutionGuard::acquire_at(&path)?;
        assert!(matches!(legacy.try_lock(), Err(std::fs::TryLockError::WouldBlock)));
        drop(modern);
        legacy.try_lock()?;
        drop(legacy);
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn child_reservation_holder() -> Result<()> {
        if let Some(path) = std::env::var_os("CLASH_VERGE_TEST_HOLD_LOCK") {
            let path = Path::new(&path);
            let _guard = CoreExecutionGuard::acquire_at(path)?;
            std::fs::write(path.with_extension("ready"), b"ready")?;
            loop {
                std::thread::park();
            }
        }
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn terminated_process_releases_both_reservations() -> Result<()> {
        use std::os::windows::io::{FromRawHandle as _, OwnedHandle};
        use windows_sys::Win32::System::Threading::OpenMutexW;
        let path = std::env::temp_dir().join(format!("execution-crash-{}", std::process::id()));
        let ready = path.with_extension("ready");
        let mut keep_mutex_alive = None;
        let mut child = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "execution::tests::child_reservation_holder"])
            .env("CLASH_VERGE_TEST_HOLD_LOCK", &path)
            .spawn()?;
        let result = (|| -> Result<()> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !ready.exists() {
                anyhow::ensure!(
                    std::time::Instant::now() < deadline && child.try_wait()?.is_none(),
                    "child did not reserve execution"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(CoreExecutionGuard::acquire_at(&path).is_err());
            let wide: Vec<u16> = test_coordination_name(&path).encode_utf16().chain(Some(0)).collect();
            let handle = unsafe { OpenMutexW(0x00100000, 0, wide.as_ptr()) };
            anyhow::ensure!(!handle.is_null(), "could not retain mutex for abandonment test");
            keep_mutex_alive = Some(unsafe { OwnedHandle::from_raw_handle(handle) });
            Ok(())
        })();
        let _ = child.kill();
        child.wait()?;
        result?;
        let checked = std::cell::Cell::new(false);
        let refused = CoreExecutionGuard::acquire_windows_at(
            &test_coordination_name(&path),
            &path,
            || panic!("the legacy lock exists"),
            || {
                checked.set(true);
                anyhow::bail!("residual core is still running");
            },
        );
        assert!(checked.get());
        assert!(refused.unwrap_err().to_string().contains("residual core"));
        drop(CoreExecutionGuard::acquire_at(&path)?);
        drop(keep_mutex_alive);
        std::fs::remove_file(path)?;
        std::fs::remove_file(ready)?;
        Ok(())
    }

    #[tokio::test]
    async fn termination_event_releases_reservation_even_when_pid_still_exists() -> Result<()> {
        let path = std::env::temp_dir().join(format!("execution-reused-pid-test-{}", std::process::id()));
        let guard = CoreExecutionGuard::acquire_at(&path).context("initial reservation")?;
        let (terminated, receiver) = tokio::sync::oneshot::channel();
        let waiter = guard.release_after_exit(std::process::id(), receiver);
        assert!(CoreExecutionGuard::acquire_at(&path).is_err());
        terminated
            .send(())
            .map_err(|_| anyhow::anyhow!("exit observer dropped"))?;
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter).await??;
        drop(CoreExecutionGuard::acquire_at(&path).context("reservation after termination event")?);
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[cfg(all(target_os = "linux", feature = "client"))]
    #[test]
    fn linux_idle_helper_from_other_channel_does_not_block_fallback() -> Result<()> {
        let other = crate::channel::OTHER_SERVICE_SLUG;
        let current = crate::SERVICE_SLUG;
        let service = crate::SERVICE_BINARY;
        require_no_core_executables(&[format!("/var/lib/{other}/bin/{service}").into()], true)?;
        assert!(require_no_core_executables(&[format!("/var/lib/{current}/bin/{service}").into()], true).is_err());
        assert!(require_no_core_executables(&[format!("/tmp/{service}").into()], true).is_err());
        assert!(
            require_no_core_executables(
                &[
                    format!("/var/lib/{other}/bin/{service}").into(),
                    format!("/tmp/{}", crate::CORE_ALPHA_NAME).into()
                ],
                true
            )
            .is_err()
        );
        Ok(())
    }

    #[cfg(all(unix, feature = "client"))]
    #[test]
    fn other_brand_processes_do_not_block_configured_core() -> Result<()> {
        let mut other = "unrelated-brand-core".to_owned();
        while [crate::CORE_NAME, crate::CORE_ALPHA_NAME, crate::SERVICE_BINARY].contains(&other.as_str()) {
            other.push('x');
        }
        require_no_core_executables(&[other.into()], true)?;
        assert!(require_no_core_executables(&[crate::CORE_NAME.into()], true).is_err());
        if ![crate::CORE_NAME, crate::CORE_ALPHA_NAME, crate::SERVICE_BINARY].contains(&"verge-mihomo") {
            require_no_core_executables(&["/tmp/verge-mihomo".into()], true)?;
        }
        if ![crate::CORE_NAME, crate::CORE_ALPHA_NAME, crate::SERVICE_BINARY].contains(&"clash-verge-service") {
            require_no_core_executables(&["/var/lib/clash-verge-service/bin/clash-verge-service".into()], true)?;
        }
        Ok(())
    }

    #[cfg(all(target_os = "linux", feature = "client"))]
    #[test]
    fn linux_process_identity_falls_back_to_nul_delimited_argv0_when_exe_is_denied() -> Result<()> {
        let cmdline = std::env::temp_dir().join(format!("cvr-proc-cmdline-{}", std::process::id()));
        let executable = format!("/tmp/cvr review/{}", crate::CORE_ALPHA_NAME);
        std::fs::write(&cmdline, format!("{executable}\0argument with spaces\0"))?;
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let resolved = super::resolve_linux_process_executable(Err(denied), &cmdline);
        let absent = super::resolve_linux_process_executable(Err(std::io::ErrorKind::NotFound.into()), &cmdline);
        let other = format!(
            "/var/lib/{}/bin/{}",
            crate::channel::OTHER_SERVICE_SLUG,
            crate::SERVICE_BINARY
        );
        std::fs::write(&cmdline, format!("{other}\0--debug\0"))?;
        let other = super::resolve_linux_process_executable(Err(std::io::ErrorKind::PermissionDenied.into()), &cmdline);
        std::fs::write(&cmdline, b"\0")?;
        let incomplete =
            super::resolve_linux_process_executable(Err(std::io::ErrorKind::PermissionDenied.into()), &cmdline);
        std::fs::remove_file(&cmdline)?;
        let unreadable =
            super::resolve_linux_process_executable(Err(std::io::ErrorKind::PermissionDenied.into()), &cmdline);
        let authoritative = super::resolve_linux_process_executable(Ok(PathBuf::from(&executable)), &cmdline);
        assert_eq!(resolved?, PathBuf::from(&executable));
        assert_eq!(authoritative?, PathBuf::from(executable));
        require_no_core_executables(&[other?], true)?;
        assert_eq!(absent.unwrap_err().kind(), std::io::ErrorKind::NotFound);
        assert_eq!(incomplete.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
        assert_ne!(unreadable.unwrap_err().kind(), std::io::ErrorKind::NotFound);
        Ok(())
    }

    #[cfg(all(target_os = "linux", feature = "client"))]
    fn probe_copied_linux_process(name: &str, expect_residual: bool) -> Result<()> {
        let directory = std::env::temp_dir().join(format!("cvr review {name} {}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let executable = directory.join(name);
        std::fs::copy("/bin/sleep", &executable)?;
        let mut child = std::process::Command::new(&executable).arg("30").spawn()?;
        let result = (|| -> Result<bool> {
            let output = std::process::Command::new("ps")
                .args(["-p", &child.id().to_string(), "-o", "pid=,comm="])
                .output()?;
            anyhow::ensure!(output.status.success());
            let detected = require_no_unix_core_processes(&String::from_utf8_lossy(&output.stdout), false);
            Ok(detected.is_err())
        })();
        let _ = child.kill();
        child.wait()?;
        std::fs::remove_dir_all(directory)?;
        assert_eq!(result?, expect_residual);
        Ok(())
    }

    #[cfg(all(target_os = "linux", feature = "client"))]
    #[test]
    fn linux_process_identity_detects_alpha_in_a_spaced_path() -> Result<()> {
        probe_copied_linux_process(crate::CORE_ALPHA_NAME, true)
    }

    #[cfg(all(target_os = "linux", feature = "client"))]
    #[test]
    fn linux_process_identity_distinguishes_a_fifteen_byte_core_prefix() -> Result<()> {
        if crate::CORE_NAME.len() == 15 {
            probe_copied_linux_process(&format!("{}-extra", crate::CORE_NAME), false)?;
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn fifo_coordination_entry_does_not_block() -> Result<()> {
        use std::os::unix::ffi::OsStrExt as _;
        let path = std::env::temp_dir().join(format!("execution-fifo-test-{}", std::process::id()));
        let name = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        anyhow::ensure!(unsafe { platform_lib::mkfifo(name.as_ptr(), 0o600) } == 0);
        let mut child = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "execution::tests::child_lock_probe"])
            .env("CLASH_VERGE_TEST_LOCK", &path)
            .spawn()?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let result = loop {
            if let Some(status) = child.try_wait()? {
                break Some(status);
            }
            if std::time::Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        if result.is_none() {
            child.kill()?;
            child.wait()?;
        }
        std::fs::remove_file(path)?;
        assert!(
            result.is_some_and(|status| status.success()),
            "opening a FIFO must fail without waiting for a writer"
        );
        Ok(())
    }
    #[cfg(all(target_os = "macos", feature = "client"))]
    #[test]
    fn an_idle_helper_from_the_other_channel_does_not_block_fallback() -> Result<()> {
        let current = format!(
            "/Library/PrivilegedHelperTools/{}.bundle/Contents/MacOS/{}",
            crate::MACOS_SERVICE_ID,
            crate::SERVICE_BINARY
        );
        let other = format!(
            "/Library/PrivilegedHelperTools/{}.bundle/Contents/MacOS/{}",
            crate::channel::OTHER_MACOS_SERVICE_ID,
            crate::SERVICE_BINARY
        );
        require_no_unix_core_processes(&other, true)?;
        for residual in [
            current.to_owned(),
            crate::SERVICE_BINARY.into(),
            format!("/tmp{other}"),
            format!(
                "{other}\n/Library/Application Support/{}/cores/{}",
                crate::SERVICE_SLUG,
                crate::CORE_NAME
            ),
        ] {
            assert!(
                require_no_unix_core_processes(&residual, true).is_err(),
                "unconfirmed idle state must remain blocked: {residual}"
            );
        }
        for residual in [
            current.to_owned(),
            format!("/tmp/{}\n{current}", crate::CORE_NAME),
            format!("{current}\n/tmp/{}", crate::CORE_NAME),
        ] {
            let refusal = require_no_unix_core_processes(&residual, true).expect_err("residual helper must block");
            assert!(
                refusal.downcast_ref::<ResidualServiceError>().is_some(),
                "a residual helper must stay repairable: {residual}"
            );
        }
        Ok(())
    }

    #[test]
    fn child_lock_probe() -> Result<()> {
        if let Some(path) = std::env::var_os("CLASH_VERGE_TEST_LOCK") {
            assert!(CoreExecutionGuard::acquire_at(Path::new(&path)).is_err());
        }
        Ok(())
    }

    #[test]
    fn reservation_excludes_other_processes_until_drop() -> Result<()> {
        let root = std::env::temp_dir().join(format!("execution-lock-test-{}", std::process::id()));
        let first = CoreExecutionGuard::acquire_at(&root)?;
        assert!(CoreExecutionGuard::acquire_at(&root).is_err());
        let child = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "execution::tests::child_lock_probe"])
            .env("CLASH_VERGE_TEST_LOCK", &root)
            .status()?;
        assert!(child.success());
        drop(first);
        drop(CoreExecutionGuard::acquire_at(&root)?);
        std::fs::remove_file(root)?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reservation_survives_stop_request_until_process_exit() -> Result<()> {
        let root = std::env::temp_dir().join(format!("execution-exit-test-{}", std::process::id()));
        let mut child = std::process::Command::new("sleep").arg("30").spawn()?;
        let guard = CoreExecutionGuard::acquire_at(&root)?;
        let (_sender, receiver) = tokio::sync::oneshot::channel();
        let waiter = guard.release_after_exit(child.id(), receiver);
        assert!(CoreExecutionGuard::acquire_at(&root).is_err());
        child.kill()?;
        child.wait()?;
        tokio::time::timeout(std::time::Duration::from_secs(2), waiter).await??;
        drop(CoreExecutionGuard::acquire_at(&root)?);
        std::fs::remove_file(root)?;
        Ok(())
    }
}
