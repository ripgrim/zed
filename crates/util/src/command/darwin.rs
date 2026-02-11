use mach2::exception_types::{
    EXC_MASK_ALL, EXCEPTION_DEFAULT, exception_behavior_t, exception_mask_t,
};
use mach2::kern_return::{KERN_SUCCESS, kern_return_t};
use mach2::mach_types::task_t;
use mach2::port::{MACH_PORT_NULL, mach_port_t};
use mach2::thread_status::{THREAD_STATE_NONE, thread_state_flavor_t};
use mach2::traps::mach_task_self;
use smol::Unblock;
use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::FromRawFd;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output, Stdio};
use std::ptr;

unsafe extern "C" {
    fn task_set_exception_ports(
        task: task_t,
        exception_mask: exception_mask_t,
        new_port: mach_port_t,
        behavior: exception_behavior_t,
        new_flavor: thread_state_flavor_t,
    ) -> kern_return_t;

    fn posix_spawnattr_setexceptionports_np(
        attr: *mut libc::posix_spawnattr_t,
        mask: exception_mask_t,
        new_port: mach_port_t,
        behavior: exception_behavior_t,
        new_flavor: thread_state_flavor_t,
    ) -> libc::c_int;

    fn posix_spawn_file_actions_addchdir_np(
        file_actions: *mut libc::posix_spawn_file_actions_t,
        path: *const libc::c_char,
    ) -> libc::c_int;

    fn posix_spawn_file_actions_addinherit_np(
        file_actions: *mut libc::posix_spawn_file_actions_t,
        filedes: libc::c_int,
    ) -> libc::c_int;

    static environ: *const *mut libc::c_char;
}

pub fn reset_exception_ports() {
    unsafe {
        let task = mach_task_self();
        let kr = task_set_exception_ports(
            task,
            EXC_MASK_ALL,
            MACH_PORT_NULL,
            EXCEPTION_DEFAULT as exception_behavior_t,
            THREAD_STATE_NONE,
        );

        if kr != KERN_SUCCESS {
            eprintln!(
                "Warning: failed to reset exception ports in child process (kern_return: {})",
                kr
            );
        }
    }
}

pub struct Command {
    program: OsString,
    args: Vec<OsString>,
    envs: Option<Vec<(OsString, OsString)>>,
    env_clear: bool,
    current_dir: Option<PathBuf>,
    stdin_cfg: Option<Stdio>,
    stdout_cfg: Option<Stdio>,
    stderr_cfg: Option<Stdio>,
    kill_on_drop: bool,
}

impl Command {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_owned(),
            args: Vec::new(),
            envs: None,
            env_clear: false,
            current_dir: None,
            stdin_cfg: None,
            stdout_cfg: None,
            stderr_cfg: None,
            kill_on_drop: false,
        }
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> &mut Self {
        self.envs
            .get_or_insert_with(Vec::new)
            .push((key.as_ref().to_owned(), val.as_ref().to_owned()));
        self
    }

    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        let env_vec = self.envs.get_or_insert_with(Vec::new);
        env_vec.extend(
            vars.into_iter()
                .map(|(k, v)| (k.as_ref().to_owned(), v.as_ref().to_owned())),
        );
        self
    }

    pub fn env_remove(&mut self, _key: impl AsRef<OsStr>) -> &mut Self {
        self
    }

    pub fn env_clear(&mut self) -> &mut Self {
        self.env_clear = true;
        self.envs = Some(Vec::new());
        self
    }

    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.current_dir = Some(dir.as_ref().to_owned());
        self
    }

    pub fn stdin(&mut self, cfg: Stdio) -> &mut Self {
        self.stdin_cfg = Some(cfg);
        self
    }

    pub fn stdout(&mut self, cfg: Stdio) -> &mut Self {
        self.stdout_cfg = Some(cfg);
        self
    }

    pub fn stderr(&mut self, cfg: Stdio) -> &mut Self {
        self.stderr_cfg = Some(cfg);
        self
    }

    pub fn kill_on_drop(&mut self, kill_on_drop: bool) -> &mut Self {
        self.kill_on_drop = kill_on_drop;
        self
    }

    pub fn spawn(&mut self) -> io::Result<Child> {
        let current_dir = self
            .current_dir
            .as_deref()
            .unwrap_or_else(|| Path::new("."));

        let envs = if self.env_clear {
            self.envs.as_deref()
        } else if self.envs.is_some() {
            let base_envs: Vec<(OsString, OsString)> = std::env::vars_os().collect();
            let mut combined = base_envs;
            if let Some(ref additional) = self.envs {
                combined.extend(additional.iter().cloned());
            }
            self.envs = Some(combined);
            self.envs.as_deref()
        } else {
            None
        };

        spawn_posix(
            &self.program,
            &self.args,
            current_dir,
            envs,
            self.kill_on_drop,
        )
    }

    pub async fn output(&mut self) -> io::Result<Output> {
        self.stdin_cfg.get_or_insert(Stdio::null());
        self.stdout_cfg.get_or_insert(Stdio::piped());
        self.stderr_cfg.get_or_insert(Stdio::piped());

        let child = self.spawn()?;
        child.output().await
    }

    pub async fn status(&mut self) -> io::Result<ExitStatus> {
        let mut child = self.spawn()?;
        child.status().await
    }
}

pub struct Child {
    pid: libc::pid_t,
    pub stdin: Option<Unblock<std::fs::File>>,
    pub stdout: Option<Unblock<std::fs::File>>,
    pub stderr: Option<Unblock<std::fs::File>>,
    kill_on_drop: bool,
    status: Option<ExitStatus>,
}

impl Drop for Child {
    fn drop(&mut self) {
        if self.kill_on_drop && self.status.is_none() {
            let _ = self.kill();
        }
    }
}

impl Child {
    pub fn id(&self) -> u32 {
        self.pid as u32
    }

    pub fn kill(&mut self) -> io::Result<()> {
        let result = unsafe { libc::kill(self.pid, libc::SIGKILL) };
        if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn try_status(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }

        let mut status: libc::c_int = 0;
        let result = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };

        if result == -1 {
            Err(io::Error::last_os_error())
        } else if result == 0 {
            Ok(None)
        } else {
            let exit_status = ExitStatus::from_raw(status);
            self.status = Some(exit_status);
            Ok(Some(exit_status))
        }
    }

    pub async fn status(&mut self) -> io::Result<ExitStatus> {
        drop(self.stdin.take());

        if let Some(status) = self.status {
            return Ok(status);
        }

        let pid = self.pid;
        let status = smol::unblock(move || {
            let mut status: libc::c_int = 0;
            let result = unsafe { libc::waitpid(pid, &mut status, 0) };
            if result == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(ExitStatus::from_raw(status))
            }
        })
        .await?;

        self.status = Some(status);
        Ok(status)
    }

    pub async fn output(mut self) -> io::Result<Output> {
        use futures_lite::AsyncReadExt;

        drop(self.stdin.take());

        let stdout_future = async {
            let mut data = Vec::new();
            if let Some(mut stdout) = self.stdout.take() {
                stdout.read_to_end(&mut data).await?;
            }
            io::Result::Ok(data)
        };

        let stderr_future = async {
            let mut data = Vec::new();
            if let Some(mut stderr) = self.stderr.take() {
                stderr.read_to_end(&mut data).await?;
            }
            io::Result::Ok(data)
        };

        let (stdout_data, stderr_data) =
            futures_lite::future::try_zip(stdout_future, stderr_future).await?;

        let pid = self.pid;
        let status = if let Some(status) = self.status {
            status
        } else {
            smol::unblock(move || {
                let mut status: libc::c_int = 0;
                let result = unsafe { libc::waitpid(pid, &mut status, 0) };
                if result == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(ExitStatus::from_raw(status))
                }
            })
            .await?
        };

        Ok(Output {
            status,
            stdout: stdout_data,
            stderr: stderr_data,
        })
    }
}

fn spawn_posix(
    program: &OsStr,
    args: &[OsString],
    current_dir: &Path,
    envs: Option<&[(OsString, OsString)]>,
    kill_on_drop: bool,
) -> io::Result<Child> {
    let program_cstr = CString::new(program.as_bytes()).map_err(|_| invalid_input_error())?;

    let current_dir_cstr =
        CString::new(current_dir.as_os_str().as_bytes()).map_err(|_| invalid_input_error())?;

    let mut argv_cstrs = vec![program_cstr.clone()];
    for arg in args {
        let cstr = CString::new(arg.as_bytes()).map_err(|_| invalid_input_error())?;
        argv_cstrs.push(cstr);
    }
    let mut argv_ptrs: Vec<*mut libc::c_char> = argv_cstrs
        .iter()
        .map(|s| s.as_ptr() as *mut libc::c_char)
        .collect();
    argv_ptrs.push(ptr::null_mut());

    let (envp_cstrs, envp_ptrs): (Option<Vec<CString>>, Vec<*mut libc::c_char>) =
        if let Some(envs) = envs {
            let cstrs: Vec<CString> = envs
                .iter()
                .map(|(key, value)| {
                    let mut env_str = key.as_bytes().to_vec();
                    env_str.push(b'=');
                    env_str.extend_from_slice(value.as_bytes());
                    CString::new(env_str)
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| invalid_input_error())?;
            let mut ptrs: Vec<*mut libc::c_char> = cstrs
                .iter()
                .map(|s| s.as_ptr() as *mut libc::c_char)
                .collect();
            ptrs.push(ptr::null_mut());
            (Some(cstrs), ptrs)
        } else {
            (None, vec![ptr::null_mut()])
        };

    let (stdin_read, stdin_write) = create_pipe()?;
    let (stdout_read, stdout_write) = create_pipe()?;
    let (stderr_read, stderr_write) = create_pipe()?;

    let mut attr: libc::posix_spawnattr_t = ptr::null_mut();
    let mut file_actions: libc::posix_spawn_file_actions_t = ptr::null_mut();

    unsafe {
        check_posix_err(libc::posix_spawnattr_init(&mut attr))?;
        check_posix_err(libc::posix_spawn_file_actions_init(&mut file_actions))?;

        check_posix_err(libc::posix_spawnattr_setflags(
            &mut attr,
            libc::POSIX_SPAWN_CLOEXEC_DEFAULT as libc::c_short,
        ))?;

        check_posix_err(posix_spawnattr_setexceptionports_np(
            &mut attr,
            EXC_MASK_ALL,
            MACH_PORT_NULL,
            EXCEPTION_DEFAULT as exception_behavior_t,
            THREAD_STATE_NONE,
        ))?;

        check_posix_err(posix_spawn_file_actions_addchdir_np(
            &mut file_actions,
            current_dir_cstr.as_ptr(),
        ))?;

        check_posix_err(libc::posix_spawn_file_actions_adddup2(
            &mut file_actions,
            stdin_read,
            libc::STDIN_FILENO,
        ))?;
        check_posix_err(posix_spawn_file_actions_addinherit_np(
            &mut file_actions,
            libc::STDIN_FILENO,
        ))?;

        check_posix_err(libc::posix_spawn_file_actions_adddup2(
            &mut file_actions,
            stdout_write,
            libc::STDOUT_FILENO,
        ))?;
        check_posix_err(posix_spawn_file_actions_addinherit_np(
            &mut file_actions,
            libc::STDOUT_FILENO,
        ))?;

        check_posix_err(libc::posix_spawn_file_actions_adddup2(
            &mut file_actions,
            stderr_write,
            libc::STDERR_FILENO,
        ))?;
        check_posix_err(posix_spawn_file_actions_addinherit_np(
            &mut file_actions,
            libc::STDERR_FILENO,
        ))?;

        let mut pid: libc::pid_t = 0;

        let envp = if envs.is_some() {
            envp_ptrs.as_ptr()
        } else {
            environ
        };

        let spawn_result = libc::posix_spawnp(
            &mut pid,
            program_cstr.as_ptr(),
            &file_actions,
            &attr,
            argv_ptrs.as_ptr(),
            envp,
        );

        libc::posix_spawnattr_destroy(&mut attr);
        libc::posix_spawn_file_actions_destroy(&mut file_actions);

        libc::close(stdin_read);
        libc::close(stdout_write);
        libc::close(stderr_write);

        check_posix_err(spawn_result)?;

        let _envp_cstrs = envp_cstrs;

        Ok(Child {
            pid,
            stdin: Some(Unblock::new(std::fs::File::from_raw_fd(stdin_write))),
            stdout: Some(Unblock::new(std::fs::File::from_raw_fd(stdout_read))),
            stderr: Some(Unblock::new(std::fs::File::from_raw_fd(stderr_read))),
            kill_on_drop,
            status: None,
        })
    }
}

fn create_pipe() -> io::Result<(libc::c_int, libc::c_int)> {
    let mut fds: [libc::c_int; 2] = [0; 2];
    let result = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok((fds[0], fds[1]))
}

fn check_posix_err(result: libc::c_int) -> io::Result<()> {
    if result != 0 {
        Err(io::Error::from_raw_os_error(result))
    } else {
        Ok(())
    }
}

fn invalid_input_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "invalid argument: path or argument contains null byte",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_lite::AsyncWriteExt;

    #[test]
    fn test_spawn_echo() {
        smol::block_on(async {
            let output = Command::new("/bin/echo")
                .args(["-n", "hello world"])
                .output()
                .await
                .expect("failed to run command");

            assert!(output.status.success());
            assert_eq!(output.stdout, b"hello world");
        });
    }

    #[test]
    fn test_spawn_cat_stdin() {
        smol::block_on(async {
            let mut child = Command::new("/bin/cat")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("failed to spawn");

            if let Some(ref mut stdin) = child.stdin {
                stdin
                    .write_all(b"hello from stdin")
                    .await
                    .expect("failed to write");
                stdin.close().await.expect("failed to close");
            }
            drop(child.stdin.take());

            let output = child.output().await.expect("failed to get output");
            assert!(output.status.success());
            assert_eq!(output.stdout, b"hello from stdin");
        });
    }

    #[test]
    fn test_spawn_stderr() {
        smol::block_on(async {
            let output = Command::new("/bin/sh")
                .args(["-c", "echo error >&2"])
                .output()
                .await
                .expect("failed to run command");

            assert!(output.status.success());
            assert_eq!(output.stderr, b"error\n");
        });
    }

    #[test]
    fn test_spawn_exit_code() {
        smol::block_on(async {
            let output = Command::new("/bin/sh")
                .args(["-c", "exit 42"])
                .output()
                .await
                .expect("failed to run command");

            assert!(!output.status.success());
            assert_eq!(output.status.code(), Some(42));
        });
    }

    #[test]
    fn test_spawn_current_dir() {
        smol::block_on(async {
            let output = Command::new("/bin/pwd")
                .current_dir("/tmp")
                .output()
                .await
                .expect("failed to run command");

            assert!(output.status.success());
            let pwd = String::from_utf8_lossy(&output.stdout);
            assert!(pwd.trim() == "/tmp" || pwd.trim() == "/private/tmp");
        });
    }

    #[test]
    fn test_spawn_env() {
        smol::block_on(async {
            let output = Command::new("/bin/sh")
                .args(["-c", "echo $MY_TEST_VAR"])
                .env("MY_TEST_VAR", "test_value")
                .output()
                .await
                .expect("failed to run command");

            assert!(output.status.success());
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "test_value");
        });
    }

    #[test]
    fn test_spawn_status() {
        smol::block_on(async {
            let status = Command::new("/usr/bin/true")
                .status()
                .await
                .expect("failed to run command");

            assert!(status.success());

            let status = Command::new("/usr/bin/false")
                .status()
                .await
                .expect("failed to run command");

            assert!(!status.success());
        });
    }
}
