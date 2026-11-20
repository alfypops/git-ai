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
                    &mut size as *mut _ as *mut _,
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
                    let mut pipe = connect(&path, CONNECT_TIMEOUT).expect("connect");
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
        let _server = listener.accept().expect("accept");
        let (kind, elapsed, limit) = client.join().unwrap();
        assert_eq!(kind, std::io::ErrorKind::TimedOut);
        assert!(elapsed >= limit, "returned early after {elapsed:?}");
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
