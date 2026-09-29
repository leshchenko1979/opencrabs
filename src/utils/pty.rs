//! Pseudo-terminal allocation for streamed child output.
//!
//! A pipe gives a child no terminal. libc then block-buffers its stdout, so a
//! long command's output arrives in one burst at exit and a reader watching the
//! pipe sees nothing until the run is over — a "live" stream that is not live.
//! A pty gives the child a terminal, so libc line-buffers and each line is
//! readable while the command is still running.
//!
//! The slave is deliberately **never made the controlling terminal**: both ends
//! open `O_NOCTTY` and nothing calls `TIOCSCTTY`. A child whose stdio is a tty
//! but which has no controlling terminal keeps `open("/dev/tty")` failing with
//! `ENXIO`, which is what makes `sudo`'s password read and an ssh-without-a-key
//! prompt fail fast instead of blocking on input nobody will type.
//!
//! `OPOST` is cleared on the slave: with it set, the kernel rewrites `\n` as
//! `\r\n` on output, which would corrupt the byte framing the harness compares
//! after a run.

use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

/// Allocate a pty pair, returned as `(master, slave)`.
///
/// The **master** is the harness's end — what the stream reader drains. The
/// **slave** is handed to the child as its stdio; it is a tty, so libc inside
/// the child line-buffers.
///
/// Sequence: `posix_openpt(O_RDWR | O_NOCTTY)` → `grantpt` → `unlockpt` →
/// `ptsname_r` → `open(slave, O_RDWR | O_NOCTTY)`.
///
/// `O_NOCTTY` on both opens is load-bearing, not hygiene: it is what keeps the
/// pty from being adopted as a controlling terminal, and therefore what keeps
/// `/dev/tty` failing with `ENXIO` for the child (see the module docs).
pub fn open_pty_pair() -> io::Result<(OwnedFd, OwnedFd)> {
    // SAFETY: posix_openpt returns a fresh fd or -1 and touches no Rust state.
    let master_fd: RawFd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if master_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // Adopt the fd immediately, so every early return below closes it exactly
    // once instead of leaking a descriptor per failure.
    // SAFETY: master_fd is open, owned by this call, and registered nowhere.
    let master = unsafe { OwnedFd::from_raw_fd(master_fd) };

    // SAFETY: master_fd is an open pty master for the rest of this call.
    if unsafe { libc::grantpt(master_fd) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::unlockpt(master_fd) } != 0 {
        return Err(io::Error::last_os_error());
    }

    // ptsname_r is the reentrant form; ptsname() would use a shared static
    // buffer, which is unsafe to call from this multi-threaded runtime.
    let mut name = [0 as libc::c_char; 128];
    // SAFETY: master_fd is open and `name` is a valid buffer of the given size.
    let rc = unsafe { libc::ptsname_r(master_fd, name.as_mut_ptr(), name.len()) };
    if rc != 0 {
        // ptsname_r reports failure by RETURN, not through errno.
        return Err(io::Error::from_raw_os_error(rc));
    }

    // SAFETY: ptsname_r wrote a NUL-terminated device path into `name`.
    let slave_fd: RawFd = unsafe { libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if slave_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: slave_fd is open, owned by this call, and registered nowhere.
    let slave = unsafe { OwnedFd::from_raw_fd(slave_fd) };

    disable_opost(slave_fd)?;

    Ok((master, slave))
}

/// Clear `OPOST` on `fd`'s termios, so output newlines stay one byte.
///
/// With `OPOST` set the tty driver translates `\n` to `\r\n`, which changes the
/// captured bytes: a producer emitting `a\nb\n` would arrive as `a\r\nb\r\n`.
/// Every other termios flag is left exactly as the pty layer set it.
pub fn disable_opost(fd: RawFd) -> io::Result<()> {
    // SAFETY: termios is plain-old-data and tcgetattr fully writes it before we
    // read a field.
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    t.c_oflag &= !libc::OPOST;
    // TCSANOW: apply immediately, without draining output first.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt as _;

    #[test]
    fn the_slave_is_a_tty() {
        // The whole reason this module exists: a pipe is not a tty, so the
        // child's libc block-buffers and the stream only looks live.
        let (_master, slave) = open_pty_pair().expect("open_pty_pair");
        // SAFETY: isatty only inspects the descriptor.
        let is_tty = unsafe { libc::isatty(slave.as_raw_fd()) };
        assert_eq!(is_tty, 1, "the slave end must be a tty device");
    }

    #[test]
    fn opost_is_cleared_so_newlines_stay_one_byte() {
        let (_master, slave) = open_pty_pair().expect("open_pty_pair");
        // SAFETY: tcgetattr fully writes termios before any field is read.
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut t) };
        assert_eq!(rc, 0, "tcgetattr on the slave");
        assert_eq!(
            t.c_oflag & libc::OPOST,
            0,
            "OPOST must be clear, or \\n is rewritten as \\r\\n"
        );
    }

    #[test]
    fn a_child_on_the_slave_keeps_dev_tty_closed() {
        // The fail-fast property: the harness runs children under setsid() with
        // the pty as stdio and no TIOCSCTTY, so /dev/tty must stay ENXIO. If it
        // ever opened, sudo's password read and ssh-without-a-key would block
        // on a prompt instead of failing fast.
        let (_master, slave) = open_pty_pair().expect("open_pty_pair");
        let slave_err = slave.try_clone().expect("clone the slave fd");

        let mut child = std::process::Command::new("sh");
        child
            .arg("-c")
            // `exec` failing makes the shell exit non-zero.
            .arg("exec 3</dev/tty")
            .stdin(slave)
            .stdout(slave_err)
            .stderr(std::process::Stdio::null());
        // SAFETY: pre_exec runs post-fork/pre-exec; setsid is async-signal-safe
        // and the closure touches no shared state.
        unsafe {
            child.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let status = child.status().expect("spawn sh on the pty slave");
        assert!(
            !status.success(),
            "opening /dev/tty must fail for a session leader with no controlling terminal"
        );
    }
}
