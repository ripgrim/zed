use std::ffi::OsStr;

#[cfg(target_os = "macos")]
mod darwin;

#[cfg(target_os = "macos")]
pub use darwin::{Child, Command, reset_exception_ports};

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000_u32;

pub fn new_command(program: impl AsRef<OsStr>) -> Command {
    Command::new(program)
}

#[cfg(target_os = "windows")]
pub fn new_std_command(program: impl AsRef<OsStr>) -> std::process::Command {
    use std::os::windows::process::CommandExt;

    let mut command = std::process::Command::new(program);
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(not(target_os = "windows"))]
pub fn new_std_command(program: impl AsRef<OsStr>) -> std::process::Command {
    std::process::Command::new(program)
}

#[deprecated(note = "use `new_command` instead")]
pub fn new_smol_command(program: impl AsRef<OsStr>) -> smol::process::Command {
    #[cfg(target_os = "windows")]
    {
        use smol::process::windows::CommandExt;
        let mut cmd = smol::process::Command::new(program);
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd
    }

    #[cfg(target_os = "macos")]
    {
        use std::os::unix::process::CommandExt;
        let mut std_cmd = std::process::Command::new(program);
        unsafe {
            std_cmd.pre_exec(|| {
                darwin::reset_exception_ports();
                Ok(())
            });
        }
        smol::process::Command::from(std_cmd)
    }

    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        smol::process::Command::new(program)
    }
}

#[cfg(not(target_os = "macos"))]
pub type Child = smol::process::Child;

#[cfg(not(target_os = "macos"))]
#[repr(transparent)]
pub struct Command(smol::process::Command);

#[cfg(not(target_os = "macos"))]
impl std::ops::Deref for Command {
    type Target = smol::process::Command;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(not(target_os = "macos"))]
impl std::ops::DerefMut for Command {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[cfg(not(target_os = "macos"))]
impl Command {
    #[inline]
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        #[cfg(target_os = "windows")]
        {
            use smol::process::windows::CommandExt;
            let mut cmd = smol::process::Command::new(program);
            cmd.creation_flags(CREATE_NO_WINDOW);
            Self(cmd)
        }
        #[cfg(not(target_os = "windows"))]
        Self(smol::process::Command::new(program))
    }
}
