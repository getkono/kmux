//! What the forked PTY child does between `forkpty` and `execve`.
//!
//! The daemon is multithreaded, so after `fork` the child may only make
//! async-signal-safe calls: another thread could have held the allocator lock
//! at the moment of the fork, and the child has no thread left to release it.
//! [`ChildPlan::new`] therefore does every allocation in the parent, and
//! [`ChildPlan::exec`] only makes raw syscalls.

use std::ffi::{CString, c_char, c_int};
use std::ptr;

use nix::errno::Errno;
use nix::libc;

use crate::config::PtyConfig;
use crate::error::{KmuxError, Result};

/// One past the highest signal number on any supported platform (Linux's
/// `_NSIG`; macOS stops at 32). `sigaction` rejects numbers it does not know,
/// or may not change, with `EINVAL`, which the reset loop ignores.
const SIGNAL_LIMIT: c_int = 65;

/// The most descriptors the child closes before `execve`. Past this, only
/// kmux's own close-on-exec flags keep an fd out of the shell; a higher
/// `RLIMIT_NOFILE` would make every spawn pay one `close` per possible fd.
const MAX_FD_SWEEP: c_int = 65_536;

/// The exit code of a child that could not start: what a shell reports for a
/// command it could not run.
pub(crate) const START_FAILURE_EXIT: c_int = 127;

/// Everything the child needs, prepared in the parent.
pub(crate) struct ChildPlan {
    program: CString,
    /// Owns the strings `argv_ptrs` points into.
    _argv: Vec<CString>,
    argv_ptrs: Vec<*const c_char>,
    /// Owns the strings `envp_ptrs` points into.
    _envp: Vec<CString>,
    envp_ptrs: Vec<*const c_char>,
    cwd: Option<CString>,
    fd_sweep_end: c_int,
}

impl ChildPlan {
    pub(crate) fn new(config: &PtyConfig) -> Result<Self> {
        let program = CString::new(config.program.as_str())
            .map_err(|_| KmuxError::Spawn("program name contains null byte".into()))?;

        let mut argv = Vec::with_capacity(config.args.len() + 1);
        argv.push(program.clone());
        for arg in &config.args {
            argv.push(
                CString::new(arg.as_str())
                    .map_err(|_| KmuxError::Spawn(format!("arg contains null byte: {arg}")))?,
            );
        }

        let envp = config
            .env
            .clone()
            .build()
            .iter()
            .map(|(k, v)| {
                CString::new(format!("{k}={v}"))
                    .map_err(|_| KmuxError::Spawn("env var contains null byte".into()))
            })
            .collect::<Result<Vec<_>>>()?;

        let cwd = config
            .cwd
            .as_ref()
            .map(|dir| {
                use std::os::unix::ffi::OsStrExt;
                CString::new(dir.as_os_str().as_bytes())
                    .map_err(|_| KmuxError::Spawn("cwd contains null byte".into()))
            })
            .transpose()?;

        Ok(Self {
            program,
            argv_ptrs: null_terminated(&argv),
            _argv: argv,
            envp_ptrs: null_terminated(&envp),
            _envp: envp,
            cwd,
            fd_sweep_end: fd_sweep_end(),
        })
    }

    /// Prepare the forked child and replace it with the program. Never
    /// returns: on failure it writes a diagnostic to the PTY and exits with
    /// [`START_FAILURE_EXIT`].
    ///
    /// # Safety
    /// Call only in the child of a `fork`, before anything else runs there.
    pub(crate) unsafe fn exec(&self) -> ! {
        // SAFETY: each call below is async-signal-safe and touches only memory
        // prepared before the fork.
        unsafe {
            reset_signal_state();
            close_fds_from(3, self.fd_sweep_end);
            if let Some(cwd) = &self.cwd
                && libc::chdir(cwd.as_ptr()) != 0
            {
                fail(b"kmux: cannot chdir to ", cwd.as_bytes());
            }
            libc::execve(
                self.program.as_ptr(),
                self.argv_ptrs.as_ptr(),
                self.envp_ptrs.as_ptr(),
            );
            fail(b"kmux: cannot exec ", self.program.as_bytes())
        }
    }
}

fn null_terminated(strings: &[CString]) -> Vec<*const c_char> {
    strings
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(ptr::null()))
        .collect()
}

/// The first fd number past the child's sweep: the soft `RLIMIT_NOFILE`,
/// capped at [`MAX_FD_SWEEP`]. Computed in the parent because `sysconf` is not
/// async-signal-safe.
fn fd_sweep_end() -> c_int {
    // SAFETY: sysconf has no preconditions.
    let limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    c_int::try_from(limit)
        .unwrap_or(MAX_FD_SWEEP)
        .clamp(0, MAX_FD_SWEEP)
}

/// Undo what the daemon did to its own signal state, which `execve` would
/// otherwise hand the program: an ignored disposition survives `execve` (the
/// Rust runtime ignores `SIGPIPE`, so `yes | head -1` would complain about a
/// broken pipe), and so does the signal mask.
///
/// Every disposition goes back to `SIG_DFL`, not only the ignored ones. A
/// handler would be reset by `execve` anyway, but until then a signal would
/// run the daemon's handler inside the child.
///
/// # Safety
/// Async-signal-safe; meant for the forked child.
unsafe fn reset_signal_state() {
    // SAFETY: sigaction/sigprocmask on stack-local, fully initialised values.
    unsafe {
        let mut default_action: libc::sigaction = std::mem::zeroed();
        default_action.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&raw mut default_action.sa_mask);
        for signal in 1..SIGNAL_LIMIT {
            libc::sigaction(signal, &raw const default_action, ptr::null_mut());
        }
        let mut empty: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut empty);
        libc::sigprocmask(libc::SIG_SETMASK, &raw const empty, ptr::null_mut());
    }
}

/// Close every fd from `first` up to `end`, whoever opened it.
///
/// Close-on-exec covers the fds kmux opens; this also covers the ones it does
/// not control: a descriptor another library opened without the flag, or one
/// a concurrent thread had just created on a platform with no atomic
/// `SOCK_CLOEXEC` (macOS) when the fork happened. A shell holding a stray
/// listener keeps that socket reachable after its owner closed it.
///
/// # Safety
/// Async-signal-safe; meant for the forked child, whose fds 0-2 are the PTY.
unsafe fn close_fds_from(first: c_int, end: c_int) {
    for fd in first..end {
        // SAFETY: closing an fd the child no longer needs; EBADF is harmless.
        unsafe { libc::close(fd) };
    }
}

/// Report a failed start on the PTY (fd 2 is its slave by now) and exit.
///
/// # Safety
/// Async-signal-safe; meant for the forked child.
unsafe fn fail(what: &[u8], subject: &[u8]) -> ! {
    let errno = Errno::last_raw();
    let mut digits = [0u8; 10];
    let errno_text = format_decimal(errno.unsigned_abs(), &mut digits);
    for part in [what, subject, b": errno ", errno_text, b"\n"] {
        // SAFETY: write(2) of a live slice; a short or failed write only loses
        // the diagnostic.
        unsafe { libc::write(2, part.as_ptr().cast(), part.len()) };
    }
    // SAFETY: _exit skips atexit handlers and stdio flushing, both unsafe in a
    // forked child.
    unsafe { libc::_exit(START_FAILURE_EXIT) }
}

/// Render `n` in decimal into the tail of `buf`, without allocating.
fn format_decimal(mut n: u32, buf: &mut [u8; 10]) -> &[u8] {
    let mut start = buf.len();
    for slot in buf.iter_mut().rev() {
        *slot = b'0' + u8::try_from(n % 10).unwrap_or(0);
        start -= 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    buf.get(start..).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_decimal_renders_every_digit_count() {
        for (n, text) in [
            (0, "0"),
            (7, "7"),
            (42, "42"),
            (1_000, "1000"),
            (u32::MAX, "4294967295"),
        ] {
            let mut buf = [0u8; 10];
            assert_eq!(format_decimal(n, &mut buf), text.as_bytes(), "{n}");
        }
    }
}
