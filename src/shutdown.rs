use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;

static REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn request() {
    REQUESTED.store(true, Ordering::SeqCst);
}

pub fn requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
}

#[cfg(unix)]
pub fn install() -> Result<()> {
    extern "C" fn handle_sigterm(_: libc::c_int) {
        REQUESTED.store(true, Ordering::SeqCst);
    }
    // SAFETY: installs a signal handler that performs only one lock-free atomic
    // store. No allocation, locking, or other non-signal-safe work occurs.
    let previous = unsafe {
        libc::signal(
            libc::SIGTERM,
            handle_sigterm as *const () as libc::sighandler_t,
        )
    };
    if previous == libc::SIG_ERR {
        anyhow::bail!("could not install SIGTERM handler");
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn install() -> Result<()> {
    Ok(())
}
