#[cfg(target_os = "freebsd")]
mod platform {
    use std::io;
    use std::process::Command;

    fn run(program: &str, args: &[&str]) -> io::Result<()> {
        let status = Command::new(program).args(args).status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Other,
                format!("{program} exited with status {status}"),
            ))
        }
    }

    fn unsupported(operation: &str) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("{operation} is not supported on FreeBSD"),
        ))
    }

    pub fn shutdown() -> io::Result<()> {
        run("shutdown", &["-p", "now"])
    }

    pub fn force_shutdown() -> io::Result<()> {
        run("halt", &["-p"])
    }

    pub fn reboot() -> io::Result<()> {
        run("shutdown", &["-r", "now"])
    }

    pub fn force_reboot() -> io::Result<()> {
        run("reboot", &[])
    }

    pub fn logout() -> io::Result<()> {
        unsupported("logout")
    }

    pub fn force_logout() -> io::Result<()> {
        unsupported("force_logout")
    }

    pub fn sleep() -> io::Result<()> {
        run("acpiconf", &["-s", "3"])
    }

    pub fn hibernate() -> io::Result<()> {
        unsupported("hibernate")
    }
}

#[cfg(not(target_os = "freebsd"))]
mod platform {
    use std::io;

    fn unsupported(operation: &str) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("{operation} is not supported on this platform"),
        ))
    }

    pub fn shutdown() -> io::Result<()> {
        unsupported("shutdown")
    }

    pub fn force_shutdown() -> io::Result<()> {
        unsupported("force_shutdown")
    }

    pub fn reboot() -> io::Result<()> {
        unsupported("reboot")
    }

    pub fn force_reboot() -> io::Result<()> {
        unsupported("force_reboot")
    }

    pub fn logout() -> io::Result<()> {
        unsupported("logout")
    }

    pub fn force_logout() -> io::Result<()> {
        unsupported("force_logout")
    }

    pub fn sleep() -> io::Result<()> {
        unsupported("sleep")
    }

    pub fn hibernate() -> io::Result<()> {
        unsupported("hibernate")
    }
}

pub use platform::*;
