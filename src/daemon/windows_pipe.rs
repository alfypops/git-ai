//! Owner-only named pipes for the Windows daemon's control and trace sockets.
//!
//! Every instance comes from [`create_instance`]: the current user owns it and is
//! the only allowed principal, remote clients are rejected, and the first instance
//! refuses a name that already exists. Clients connect at identification level and
//! refuse a pipe another account owns. [`OverlappedPipe`] serves both halves so
//! read, write, receive and idle timeouts keep working.

use crate::error::GitAiError;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::IntoRawHandle;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_NO_DATA,
    ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE, LocalFree, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_FLAGS_AND_ATTRIBUTES,
    PIPE_ACCESS_DUPLEX, ReadFile, SECURITY_IDENTIFICATION, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, NAMED_PIPE_MODE, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, INFINITE, OpenProcessToken, WaitForSingleObject,
};

/// Open mode of every instance; the first instance adds `FILE_FLAG_FIRST_PIPE_INSTANCE`.
const OPEN_MODE: FILE_FLAGS_AND_ATTRIBUTES = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
/// Pipe mode of every instance: a byte stream that remote clients cannot open.
pub(crate) const PIPE_MODE: NAMED_PIPE_MODE =
    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
const PIPE_BUFFER_BYTES: u32 = 65536;
/// How long the shutdown wake waits to reach each worker.
const WAKE_CONNECT_TIMEOUT: Duration = Duration::from_millis(100);

fn wide_nul(value: &std::ffi::OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

/// Frees a buffer the system allocated with `LocalAlloc`.
struct LocalBox(*mut core::ffi::c_void);

// The buffer is owned and never shared, so it can move to another thread.
unsafe impl Send for LocalBox {}

impl Drop for LocalBox {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0) };
        }
    }
}

/// The process token's `TOKEN_USER`, read once.
pub(crate) struct CurrentUser {
    // u64 words keep the TOKEN_USER header aligned.
    token_user: Vec<u64>,
}

impl CurrentUser {
    pub(crate) fn sid(&self) -> PSID {
        unsafe { (*(self.token_user.as_ptr() as *const TOKEN_USER)).User.Sid }
    }

    fn sid_string(&self) -> io::Result<String> {
        let mut raw: *mut u16 = std::ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(self.sid(), &mut raw) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let _free = LocalBox(raw as *mut _);
        let len = (0..).take_while(|&i| unsafe { *raw.add(i) } != 0).count();
        Ok(String::from_utf16_lossy(unsafe {
            std::slice::from_raw_parts(raw, len)
        }))
    }
}

pub(crate) fn current_user() -> io::Result<&'static CurrentUser> {
    static CURRENT_USER: OnceLock<CurrentUser> = OnceLock::new();
    if let Some(user) = CURRENT_USER.get() {
        return Ok(user);
    }
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = Handle(token);
    let mut needed = 0u32;
    unsafe { GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
    let mut token_user = vec![0u64; (needed as usize).div_ceil(8).max(1)];
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            token_user.as_mut_ptr() as *mut _,
            (token_user.len() * 8) as u32,
            &mut needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(CURRENT_USER.get_or_init(|| CurrentUser { token_user }))
}

/// Closes a kernel handle on drop.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// A security descriptor for pipe instances: `owner` owns them, and a protected
/// DACL allows only the current user.
pub(crate) struct InstanceSecurity {
    descriptor: LocalBox,
}

impl InstanceSecurity {
    pub(crate) fn current_user_only() -> io::Result<Self> {
        Self::owned_by(&current_user()?.sid_string()?)
    }

    /// `owner_sid` is a SID string; anything but the current user is for tests.
    pub(crate) fn owned_by(owner_sid: &str) -> io::Result<Self> {
        let user_sid = current_user()?.sid_string()?;
        let sddl = wide_nul(format!("O:{owner_sid}D:P(A;;GA;;;{user_sid})").as_ref());
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            descriptor: LocalBox(descriptor),
        })
    }
}

/// Creates one pipe instance. The only place the daemon creates one.
pub(crate) fn create_instance(
    path: &Path,
    first_instance: bool,
    security: &InstanceSecurity,
) -> io::Result<OverlappedPipe> {
    let name = wide_nul(path.as_os_str());
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.descriptor.0,
        bInheritHandle: 0,
    };
    let open_mode = if first_instance {
        OPEN_MODE | FILE_FLAG_FIRST_PIPE_INSTANCE
    } else {
        OPEN_MODE
    };
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            open_mode,
            PIPE_MODE,
            PIPE_UNLIMITED_INSTANCES,
            PIPE_BUFFER_BYTES,
            PIPE_BUFFER_BYTES,
            0,
            &attributes,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    OverlappedPipe::new(handle)
}

/// One worker's pipe: it owns its pending instance and creates the next one
/// itself, always through [`create_instance`].
pub(crate) struct PipeListener {
    path: PathBuf,
    security: InstanceSecurity,
    pending: Option<OverlappedPipe>,
}

impl PipeListener {
    pub(crate) fn bind(path: &Path, first_instance: bool) -> Result<Self, GitAiError> {
        let mut listener = Self {
            path: path.to_path_buf(),
            security: InstanceSecurity::current_user_only()
                .map_err(|error| bind_error(path, false, error))?,
            pending: None,
        };
        listener.create_pending(first_instance)?;
        Ok(listener)
    }

    fn create_pending(&mut self, first_instance: bool) -> Result<(), GitAiError> {
        let instance = create_instance(&self.path, first_instance, &self.security)
            .map_err(|error| bind_error(&self.path, first_instance, error))?;
        self.pending = Some(instance);
        Ok(())
    }

    /// The instance waiting for the next client, created now if the last one
    /// was handed out.
    pub(crate) fn pending(&mut self) -> Result<&OverlappedPipe, GitAiError> {
        if self.pending.is_none() {
            self.create_pending(false)?;
        }
        Ok(self.pending.as_ref().expect("pending instance"))
    }

    /// Waits for a client on the pending instance and hands it out.
    pub(crate) fn accept(&mut self) -> Result<OverlappedPipe, GitAiError> {
        self.pending()?;
        let pending = self.pending.as_ref().expect("pending instance");
        pending.wait_for_client().map_err(|error| {
            GitAiError::Generic(format!(
                "failed accepting windows daemon pipe {}: {}",
                self.path.display(),
                error
            ))
        })?;
        Ok(self.pending.take().expect("pending instance"))
    }
}

fn bind_error(path: &Path, first_instance: bool, error: io::Error) -> GitAiError {
    if first_instance && error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
        return GitAiError::Generic(format!(
            "failed binding windows daemon pipe {} (already in use)",
            path.display()
        ));
    }
    GitAiError::Generic(format!(
        "failed binding windows daemon pipe {}: {}",
        path.display(),
        error
    ))
}

/// Runs `worker_count` pipe workers on `pipe_path` until `is_shutting_down`,
/// handing each connection to `handle_connection` on its own thread. A worker
/// that fails calls `on_worker_error`.
pub(crate) fn serve_pipe_workers<H>(
    label: &'static str,
    pipe_path: &Path,
    worker_count: usize,
    is_shutting_down: impl Fn() -> bool + Clone + Send + 'static,
    on_worker_error: impl Fn() + Clone + Send + 'static,
    handle_connection: H,
) -> Result<(), GitAiError>
where
    H: Fn(OverlappedPipe) + Clone + Send + 'static,
{
    let mut workers = Vec::with_capacity(worker_count);
    for index in 0..worker_count {
        let listener = PipeListener::bind(pipe_path, index == 0)?;
        let is_shutting_down = is_shutting_down.clone();
        let on_worker_error = on_worker_error.clone();
        let handle_connection = handle_connection.clone();
        workers.push(std::thread::spawn(move || {
            let result = run_pipe_worker(label, listener, is_shutting_down, handle_connection);
            if let Err(error) = &result {
                tracing::error!(%error, "{label} worker error");
                on_worker_error();
            }
            result
        }));
    }

    while !is_shutting_down() {
        std::thread::sleep(Duration::from_millis(50));
    }

    // Each worker re-creates its instance only after checking for shutdown, so
    // one connection per worker reaches every pending instance.
    for _ in 0..worker_count {
        let _ = connect(pipe_path, WAKE_CONNECT_TIMEOUT);
    }

    for worker in workers {
        worker
            .join()
            .map_err(|_| GitAiError::Generic(format!("daemon {label} worker panicked")))??;
    }
    Ok(())
}

fn run_pipe_worker<H>(
    label: &'static str,
    mut listener: PipeListener,
    is_shutting_down: impl Fn() -> bool,
    handle_connection: H,
) -> Result<(), GitAiError>
where
    H: Fn(OverlappedPipe) + Clone + Send + 'static,
{
    loop {
        let pipe = listener.accept()?;
        if is_shutting_down() {
            return Ok(());
        }
        listener.pending()?;

        let handle_connection = handle_connection.clone();
        std::thread::Builder::new()
            .spawn(move || handle_connection(pipe))
            .map_err(|error| {
                GitAiError::Generic(format!(
                    "failed spawning {label} pipe handler for {}: {}",
                    listener.path.display(),
                    error
                ))
            })?;
    }
}

/// Opens the daemon's pipe at identification level, waiting while every
/// instance is busy, and refuses it unless the current user owns it.
pub(crate) fn connect(path: &Path, timeout: Duration) -> Result<OverlappedPipe, GitAiError> {
    let connect_error = |error: io::Error| {
        GitAiError::Generic(format!(
            "timed out after {:?} connecting daemon socket {}: {}",
            timeout,
            path.display(),
            error
        ))
    };
    let deadline = Instant::now() + timeout;
    let file = loop {
        match OpenOptions::new()
            .read(true)
            .write(true)
            .security_qos_flags(SECURITY_IDENTIFICATION)
            .custom_flags(FILE_FLAG_OVERLAPPED)
            .open(path)
        {
            Ok(file) => break file,
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(connect_error(error));
                }
                let name = wide_nul(path.as_os_str());
                unsafe { WaitNamedPipeW(name.as_ptr(), timeout_ms(Some(remaining))) };
            }
            Err(error) => return Err(connect_error(error)),
        }
    };
    let pipe = OverlappedPipe::new(file.into_raw_handle() as HANDLE).map_err(connect_error)?;
    if !pipe.owned_by_current_user().map_err(connect_error)? {
        return Err(GitAiError::Generic(format!(
            "refusing daemon pipe {}: another user owns the pipe",
            path.display()
        )));
    }
    Ok(pipe)
}

fn timeout_ms(timeout: Option<Duration>) -> u32 {
    timeout.map_or(INFINITE, |timeout| {
        timeout.as_millis().clamp(1, u128::from(INFINITE - 1)) as u32
    })
}

/// An overlapped pipe handle with blocking `Read`/`Write` and timeouts. On a
/// timeout the I/O is cancelled and awaited before its buffer is released.
pub struct OverlappedPipe {
    handle: HANDLE,
    event: HANDLE,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
}

// Kernel handles may be used from any thread; `&mut self` serializes the I/O.
unsafe impl Send for OverlappedPipe {}

impl OverlappedPipe {
    fn new(handle: HANDLE) -> io::Result<Self> {
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            let error = io::Error::last_os_error();
            unsafe { CloseHandle(handle) };
            return Err(error);
        }
        Ok(Self {
            handle,
            event,
            read_timeout: None,
            write_timeout: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn raw_handle(&self) -> HANDLE {
        self.handle
    }

    pub(crate) fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.read_timeout = timeout;
    }

    pub(crate) fn set_write_timeout(&mut self, timeout: Option<Duration>) {
        self.write_timeout = timeout;
    }

    fn overlapped(&self) -> OVERLAPPED {
        OVERLAPPED {
            hEvent: self.event,
            ..Default::default()
        }
    }

    /// `ERROR_PIPE_CONNECTED` and `ERROR_NO_DATA` (a client that connected and
    /// already closed) both count as connected.
    fn wait_for_client(&self) -> io::Result<()> {
        let mut overlapped = self.overlapped();
        if unsafe { ConnectNamedPipe(self.handle, &mut overlapped) } != 0 {
            return Ok(());
        }
        match unsafe { GetLastError() } {
            ERROR_PIPE_CONNECTED | ERROR_NO_DATA => Ok(()),
            ERROR_IO_PENDING => {
                let mut transferred = 0u32;
                if unsafe { GetOverlappedResult(self.handle, &overlapped, &mut transferred, 1) }
                    == 0
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            }
            code => Err(io::Error::from_raw_os_error(code as i32)),
        }
    }

    fn owned_by_current_user(&self) -> io::Result<bool> {
        let mut owner: PSID = std::ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let status = unsafe {
            GetSecurityInfo(
                self.handle,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let _free = LocalBox(descriptor);
        Ok(unsafe { EqualSid(owner, current_user()?.sid()) } != 0)
    }

    /// Starts one overlapped operation and waits up to `timeout` for it. On
    /// expiry it cancels the I/O and waits for it to finish, so the buffer and
    /// `OVERLAPPED` outlive the kernel's use: bytes that completed anyway are
    /// returned, and only a cancelled operation is `TimedOut`.
    fn complete(
        &mut self,
        timeout: Option<Duration>,
        start: impl FnOnce(HANDLE, *mut OVERLAPPED) -> i32,
    ) -> io::Result<usize> {
        let mut overlapped = self.overlapped();
        let mut transferred = 0u32;
        if start(self.handle, &mut overlapped) == 0 {
            let code = unsafe { GetLastError() };
            if code != ERROR_IO_PENDING {
                return Err(io::Error::from_raw_os_error(code as i32));
            }
            let waited = unsafe { WaitForSingleObject(self.event, timeout_ms(timeout)) };
            if waited != WAIT_OBJECT_0 {
                let wait_error = (waited != WAIT_TIMEOUT).then(io::Error::last_os_error);
                unsafe { CancelIoEx(self.handle, &overlapped) };
                if unsafe { GetOverlappedResult(self.handle, &overlapped, &mut transferred, 1) }
                    != 0
                {
                    return Ok(transferred as usize);
                }
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(ERROR_OPERATION_ABORTED as i32) {
                    return Err(error);
                }
                return Err(wait_error.unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::TimedOut, "pipe operation timed out")
                }));
            }
        }
        if unsafe { GetOverlappedResult(self.handle, &overlapped, &mut transferred, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(transferred as usize)
    }
}

impl Read for OverlappedPipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = buf.len().min(u32::MAX as usize) as u32;
        let ptr = buf.as_mut_ptr();
        match self.complete(self.read_timeout, |handle, overlapped| unsafe {
            ReadFile(handle, ptr, len, std::ptr::null_mut(), overlapped)
        }) {
            // The other end closed: end of stream, as std reads a pipe.
            Err(error) if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) => Ok(0),
            result => result,
        }
    }
}

impl Write for OverlappedPipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let len = buf.len().min(u32::MAX as usize) as u32;
        let ptr = buf.as_ptr();
        self.complete(self.write_timeout, |handle, overlapped| unsafe {
            WriteFile(handle, ptr, len, std::ptr::null_mut(), overlapped)
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for OverlappedPipe {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
            CloseHandle(self.event);
        }
    }
}

#[cfg(all(test, windows))]
mod windows_pipe_tests {
    use super::*;
    use crate::daemon::{ControlConnection, control_receive_timed_out, read_json_line};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    use windows_sys::Win32::Foundation::{ERROR_INVALID_OWNER, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
        GetSecurityDescriptorControl, GetTokenInformation, OWNER_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, RevertToSelf, SE_DACL_PROTECTED, SecurityIdentification,
        TOKEN_QUERY, TokenImpersonationLevel,
    };
    use windows_sys::Win32::System::Pipes::{
        ImpersonateNamedPipeClient, PIPE_REJECT_REMOTE_CLIENTS,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};

    /// `BUILTIN\Administrators`: an owner other than the test's own user.
    const ADMINISTRATORS_SID: &str = "S-1-5-32-544";
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

    fn unique_pipe_path(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        PathBuf::from(format!(
            r"\\.\pipe\git-ai-test-{}-{}-{}",
            std::process::id(),
            nanos,
            tag
        ))
    }

    /// Connects once the pipe exists: a server started on another thread may
    /// not have bound its name yet.
    fn connect_when_listening(path: &Path) -> OverlappedPipe {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        loop {
            match connect(path, CONNECT_TIMEOUT) {
                Ok(pipe) => return pipe,
                Err(error) if Instant::now() >= deadline => panic!("connect: {error}"),
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    /// The owner, DACL protection and allow ACEs of a pipe handle.
    struct PipeSecurity {
        owner_is_current_user: bool,
        dacl_protected: bool,
        allow_ace_sids_are_current_user: Vec<bool>,
        ace_count: u32,
    }

    fn read_pipe_security(handle: HANDLE) -> PipeSecurity {
        let mut owner: PSID = std::ptr::null_mut();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let status = unsafe {
            GetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(status, 0, "GetSecurityInfo failed");
        let user = current_user().expect("current user").sid();

        let mut control = 0u16;
        let mut revision = 0u32;
        assert_ne!(
            unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) },
            0
        );

        assert!(
            !dacl.is_null(),
            "the pipe has a NULL DACL (everyone allowed)"
        );
        let mut size = ACL_SIZE_INFORMATION {
            AceCount: 0,
            AclBytesInUse: 0,
            AclBytesFree: 0,
        };
        assert_ne!(
            unsafe {
                GetAclInformation(
                    dacl,
                    &mut size as *mut ACL_SIZE_INFORMATION as *mut core::ffi::c_void,
                    std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                )
            },
            0
        );
        let mut allow_ace_sids_are_current_user = Vec::new();
        for index in 0..size.AceCount {
            let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
            assert_ne!(unsafe { GetAce(dacl, index, &mut ace) }, 0);
            let ace = ace as *const ACCESS_ALLOWED_ACE;
            if unsafe { (*ace).Header.AceType } == ACCESS_ALLOWED_ACE_TYPE {
                let sid = unsafe { &(*ace).SidStart } as *const u32 as PSID;
                allow_ace_sids_are_current_user.push(unsafe { EqualSid(sid, user) } != 0);
            }
        }

        let security = PipeSecurity {
            owner_is_current_user: unsafe { EqualSid(owner, user) } != 0,
            dacl_protected: control & SE_DACL_PROTECTED != 0,
            allow_ace_sids_are_current_user,
            ace_count: size.AceCount,
        };
        unsafe { LocalFree(descriptor) };
        security
    }

    #[test]
    fn pipe_mode_rejects_remote_clients() {
        assert_ne!(PIPE_MODE & PIPE_REJECT_REMOTE_CLIENTS, 0);
    }

    #[test]
    fn a_second_first_instance_is_refused_as_already_in_use() {
        let path = unique_pipe_path("squat");
        let _first = PipeListener::bind(&path, true).expect("first bind");
        let error = PipeListener::bind(&path, true)
            .err()
            .expect("a second first-instance bind must fail");
        assert!(
            error.to_string().contains("(already in use)"),
            "unexpected error: {error}"
        );
        let error = serve_pipe_workers("test", &path, 1, || true, || {}, |_| {})
            .err()
            .expect("serving a taken name must fail");
        assert!(
            error.to_string().contains("(already in use)"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn every_re_created_instance_is_owner_only() {
        let path = unique_pipe_path("owner-only");
        let mut listener = PipeListener::bind(&path, true).expect("bind");
        let initial =
            read_pipe_security(listener.pending().expect("pending instance").raw_handle());
        assert!(initial.owner_is_current_user);

        let client_path = path.clone();
        let client = std::thread::spawn(move || connect(&client_path, CONNECT_TIMEOUT));
        let _server = listener.accept().expect("accept");
        let _client = client.join().unwrap().expect("client connect");

        let recreated =
            read_pipe_security(listener.pending().expect("pending instance").raw_handle());
        assert!(
            recreated.owner_is_current_user,
            "the re-created instance's owner is not the current user"
        );
        assert!(recreated.dacl_protected, "the DACL is not protected");
        assert_eq!(recreated.ace_count, 1, "expected exactly one ACE");
        assert_eq!(recreated.allow_ace_sids_are_current_user, vec![true]);
    }

    #[test]
    fn concurrent_clients_round_trip_and_the_shutdown_wake_returns_every_worker() {
        let path = unique_pipe_path("workers");
        let shutting_down = Arc::new(AtomicBool::new(false));
        let worker_error = Arc::new(AtomicBool::new(false));
        let server = {
            let path = path.clone();
            let shutting_down = shutting_down.clone();
            let worker_error = worker_error.clone();
            std::thread::spawn(move || {
                serve_pipe_workers(
                    "test",
                    &path,
                    2,
                    move || shutting_down.load(Ordering::SeqCst),
                    move || worker_error.store(true, Ordering::SeqCst),
                    |pipe: OverlappedPipe| {
                        let mut reader = BufReader::new(pipe);
                        let mut line = String::new();
                        if reader.read_line(&mut line).is_ok() {
                            let _ = reader.get_mut().write_all(line.as_bytes());
                        }
                    },
                )
            })
        };

        let clients: Vec<_> = (0..2)
            .map(|index| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let mut pipe = connect_when_listening(&path);
                    pipe.set_read_timeout(Some(CONNECT_TIMEOUT));
                    let message = format!("hello {index}\n");
                    pipe.write_all(message.as_bytes()).expect("write");
                    let mut reply = String::new();
                    BufReader::new(pipe).read_line(&mut reply).expect("read");
                    assert_eq!(reply, message);
                })
            })
            .collect();
        for client in clients {
            client.join().expect("client round trip");
        }

        shutting_down.store(true, Ordering::SeqCst);
        server
            .join()
            .expect("server thread")
            .expect("every worker returns after the shutdown wake");
        assert!(!worker_error.load(Ordering::SeqCst));
    }

    #[test]
    fn a_pipe_another_account_owns_is_refused() {
        let path = unique_pipe_path("foreign-owner");
        let security = InstanceSecurity::owned_by(ADMINISTRATORS_SID).expect("descriptor");
        let _instance = match create_instance(&path, true, &security) {
            Ok(instance) => instance,
            Err(error) if error.raw_os_error() == Some(ERROR_INVALID_OWNER as i32) => {
                // CI runners are elevated, so there the refusal must really run.
                assert!(
                    std::env::var_os("CI").is_none(),
                    "ERROR_INVALID_OWNER in CI: the owner refusal was not tested"
                );
                eprintln!(
                    "skipped: this token cannot assign Administrators as owner (ERROR_INVALID_OWNER); run elevated, as CI does"
                );
                return;
            }
            Err(error) => panic!("create_instance failed: {error}"),
        };
        let error = connect(&path, CONNECT_TIMEOUT)
            .err()
            .expect("the client must refuse a pipe another account owns");
        assert!(
            error.to_string().contains("another user owns the pipe"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn the_client_connects_at_identification_level() {
        let path = unique_pipe_path("identification");
        let mut listener = PipeListener::bind(&path, true).expect("bind");
        let client_path = path.clone();
        let client = std::thread::spawn(move || {
            let mut pipe = connect(&client_path, CONNECT_TIMEOUT).expect("connect");
            pipe.write_all(b"x").expect("write");
            pipe
        });
        let mut server = listener.accept().expect("accept");
        let mut byte = [0u8; 1];
        server
            .read_exact(&mut byte)
            .expect("read before impersonating");
        let _client = client.join().unwrap();

        assert_ne!(
            unsafe { ImpersonateNamedPipeClient(server.raw_handle()) },
            0,
            "ImpersonateNamedPipeClient failed: {}",
            std::io::Error::last_os_error()
        );
        let mut token: HANDLE = std::ptr::null_mut();
        let opened = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) };
        let open_error = std::io::Error::last_os_error();
        let mut level = 0i32;
        let mut returned = 0u32;
        let queried = opened != 0
            && unsafe {
                GetTokenInformation(
                    token,
                    TokenImpersonationLevel,
                    &mut level as *mut i32 as *mut _,
                    std::mem::size_of::<i32>() as u32,
                    &mut returned,
                )
            } != 0;
        assert_ne!(unsafe { RevertToSelf() }, 0);
        if opened != 0 {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(token) };
        }
        assert!(opened != 0, "OpenThreadToken failed: {open_error}");
        assert!(
            queried,
            "GetTokenInformation(TokenImpersonationLevel) failed"
        );
        assert_eq!(level, SecurityIdentification);
    }

    #[test]
    fn the_client_read_times_out_when_the_server_never_replies() {
        let path = unique_pipe_path("client-timeout");
        let mut listener = PipeListener::bind(&path, true).expect("bind");
        let client_path = path.clone();
        let client = std::thread::spawn(move || {
            let mut pipe = connect(&client_path, CONNECT_TIMEOUT).expect("connect");
            let limit = Duration::from_millis(300);
            pipe.set_read_timeout(Some(limit));
            let started = Instant::now();
            let error = pipe
                .read(&mut [0u8; 16])
                .expect_err("a read with no reply must time out");
            (error.kind(), started.elapsed(), limit)
        });
        let server = listener.accept().expect("accept");
        // Close the server end later, so a missing timeout fails instead of hanging.
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(5));
            drop(server);
        });
        let (kind, elapsed, limit) = client.join().expect("the client read must time out");
        assert_eq!(kind, std::io::ErrorKind::TimedOut);
        assert!(
            elapsed + Duration::from_millis(20) >= limit,
            "returned early after {elapsed:?}"
        );
        assert!(elapsed < limit + Duration::from_secs(2), "took {elapsed:?}");
    }

    #[test]
    fn the_server_drops_an_idle_client_after_its_receive_timeout() {
        let path = unique_pipe_path("idle-drop");
        let mut listener = PipeListener::bind(&path, true).expect("bind");
        let client_path = path.clone();
        let client = std::thread::spawn(move || {
            let mut pipe = connect(&client_path, CONNECT_TIMEOUT).expect("connect");
            pipe.set_read_timeout(Some(CONNECT_TIMEOUT));
            pipe.read(&mut [0u8; 16])
        });

        let server = listener.accept().expect("accept");
        let mut reader = BufReader::new(server);
        reader
            .get_mut()
            .set_receive_timeout(Some(Duration::from_millis(300)))
            .expect("set receive timeout");
        let error = read_json_line(&mut reader).expect_err("an idle client must time out");
        assert!(
            control_receive_timed_out(&error),
            "not classified as a receive timeout: {error}"
        );
        drop(reader);

        match client.join().unwrap() {
            Ok(0) => {}
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
            other => panic!("the client was not dropped: {other:?}"),
        }
    }
}
