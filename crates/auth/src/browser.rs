//! Opening the system browser for the interactive step.

use std::fmt::Debug;
use std::process::{Command, Stdio};

use url::Url;

use crate::error::{Error, ErrorKind, Result};

/// Opens a URL in the user's browser (M-MOCKABLE-SYSCALLS).
///
/// The desktop app may pass its own (e.g. Tauri's opener); tests pass a fake that performs
/// the redirect itself.
pub trait BrowserOpener: Debug + Send + Sync {
    /// Opens `url`. Should return promptly, without waiting for the browser to exit.
    ///
    /// # Errors
    /// Any error; sign-in fails with it.
    fn open(&self, url: &Url) -> Result<()>;
}

/// Opens the URL with the platform's launcher: `open` (macOS), `xdg-open` (Linux and other
/// Unix), or `rundll32 url.dll,FileProtocolHandler` (Windows; unlike `cmd /c start` it does
/// not reinterpret `&` in the URL).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemBrowser;

impl BrowserOpener for SystemBrowser {
    fn open(&self, url: &Url) -> Result<()> {
        if url.scheme() != "https" && url.scheme() != "http" {
            return Err(Error::new(ErrorKind::Bug, "refusing to open a non-http URL"));
        }
        let mut cmd = launcher();
        cmd.arg(url.as_str()).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let mut child =
            cmd.spawn().map_err(|e| Error::new(ErrorKind::Config, "could not open the browser").with_source(e))?;
        // Reap the launcher in the background so it does not linger as a zombie.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn launcher() -> Command {
    Command::new("/usr/bin/open")
}

#[cfg(target_os = "windows")]
fn launcher() -> Command {
    let mut c = Command::new("rundll32");
    c.arg("url.dll,FileProtocolHandler");
    c
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn launcher() -> Command {
    Command::new("xdg-open")
}
