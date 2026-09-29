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
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use tokio::io::unix::AsyncFd;

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

/// The harness's end of a pty, as an [`AsyncRead`].
///
/// Two behaviours make this more than a thin `read(2)` wrapper, and both are
/// what the stream reader depends on:
///
/// * **`O_NONBLOCK`.** The reader is polled on a runtime worker, so a blocking
///   `read` would park that thread. The flag is set once at construction; the
///   *slave* stays blocking, which is what makes the child line-buffer.
/// * **`EIO` is EOF.** Linux reports `EIO` from a master read once every fd for
///   the slave end is closed — the child exiting closes its copy, so the last
///   reader on the master sees `EIO` where a pipe would give `Ok(0)`. Reported
///   as EOF, or every streamed run would end in a spurious read error.
pub struct PtyMaster(AsyncFd<OwnedFd>);

impl PtyMaster {
    /// Wrap a master fd for async reading. Sets `O_NONBLOCK`.
    pub fn new(fd: OwnedFd) -> io::Result<Self> {
        // SAFETY: fcntl with F_GETFL/F_SETFL only manipulates descriptor flags;
        // the fd is owned by the caller and valid for the duration of the call.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: as above; the fd is ours to configure.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(AsyncFd::new(fd)?))
    }
}

impl tokio::io::AsyncRead for PtyMaster {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        loop {
            let mut guard = match self.0.poll_read_ready(cx) {
                std::task::Poll::Ready(Ok(g)) => g,
                std::task::Poll::Ready(Err(e)) => return std::task::Poll::Ready(Err(e)),
                std::task::Poll::Pending => return std::task::Poll::Pending,
            };
            let dst = buf.initialize_unfilled();
            if dst.is_empty() {
                return std::task::Poll::Ready(Ok(()));
            }
            // SAFETY: `dst` is the caller's own unfilled slice, valid and
            // writable for `dst.len()` bytes; the fd is alive inside the guard,
            // which borrows `self.0` for as long as it is held.
            let n = unsafe {
                libc::read(
                    guard.get_inner().as_raw_fd(),
                    dst.as_mut_ptr().cast(),
                    dst.len(),
                )
            };
            if n > 0 {
                buf.advance(n as usize);
                return std::task::Poll::Ready(Ok(()));
            }
            if n == 0 {
                return std::task::Poll::Ready(Ok(()));
            }
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                // Nothing buffered yet — wait for readability instead of
                // spinning. `EWOULDBLOCK` is deliberately absent: on every
                // unix it is the same value as `EAGAIN` (both 11 on Linux), so
                // an alternation is an unreachable pattern, and `-D warnings`
                // rejects it — the "portability" it looks like is a lie the
                // compiler catches.
                Some(libc::EAGAIN) => {
                    guard.clear_ready();
                    continue;
                }
                // A signal interrupted us; the data is still there.
                Some(libc::EINTR) => continue,
                // Last slave fd closed: this stream is over. See the type docs.
                Some(libc::EIO) => return std::task::Poll::Ready(Ok(())),
                _ => return std::task::Poll::Ready(Err(e)),
            }
        }
    }
}

/// Give `cmd` a pty for stdout and a **separate** pty for stderr, returning the
/// two masters.
///
/// Two pairs rather than one merged stream is the deliberate choice: a merged
/// pty would make the harness unable to say which stream spoke, and the result
/// framing this feature must preserve is `STDOUT:` / `STDERR:`. With one pty per
/// stream the child sees a terminal on both (so libc line-buffers and output is
/// live) while the streams stay distinct and byte-identical to the piped form.
///
/// The slaves are *moved* into the command, so the parent holds no slave fd —
/// which is what lets the master reach EOF/`EIO` when the child exits. A parent
/// copy left open would stream forever.
pub fn install_pty(
    cmd: &mut tokio::process::Command,
) -> io::Result<(PtyMaster, PtyMaster)> {
    let (master_out, slave_out) = open_pty_pair()?;
    let (master_err, slave_err) = open_pty_pair()?;

    // Cleared on the master; the termios setting belongs to the tty, shared by
    // both ends. Without it the kernel rewrites \n as \r\n and the framing the
    // harness compares is no longer the child's bytes.
    disable_opost(master_out.as_raw_fd())?;
    disable_opost(master_err.as_raw_fd())?;

    // The masters are wrapped BEFORE `cmd`'s stdio is touched, and that order is
    // the whole point: `PtyMaster::new` can still fail, and if the slaves were
    // already installed then the caller's pipe fallback would find
    // `child.stdout == None` and capture nothing while the child wrote into a
    // pty whose master had just been closed — a silently empty run instead of a
    // degraded one. Failing here leaves `cmd` exactly as the caller built it.
    let mo = PtyMaster::new(master_out)?;
    let me = PtyMaster::new(master_err)?;

    cmd.stdout(std::process::Stdio::from(std::fs::File::from(slave_out)));
    cmd.stderr(std::process::Stdio::from(std::fs::File::from(slave_err)));

    Ok((mo, me))
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

    #[test]
    fn a_master_read_reports_eof_when_the_last_slave_closes() {
        // Linux gives EIO on a master read once every fd for the slave end is
        // closed. Read as an error, every streamed run would end in a spurious
        // failure instead of a clean EOF, so `PtyMaster` maps it — and this test
        // is what keeps that mapping from being lost as seemingly-dead code.
        let (master, slave) = open_pty_pair().expect("open_pty_pair");
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "printf done"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(std::fs::File::from(slave)))
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn");
        child.wait().expect("wait");
        let mut buf = [0u8; 64];
        loop {
            // SAFETY: `buf` is a live 64-byte array; the fd is owned by `master`.
            let n = unsafe { libc::read(master.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                continue; // drain the child's own bytes first
            }
            if n == 0 {
                break;
            }
            let e = io::Error::last_os_error();
            assert_eq!(
                e.raw_os_error(),
                Some(libc::EIO),
                "read on a last-closed master must be EIO, got {e:?}"
            );
            break;
        }
    }

    #[test]
    fn output_is_readable_while_the_child_is_still_running() {
        // The premise of this entire module, as an assertion: a pipe would
        // deliver nothing until exit, so this test cannot pass on piped stdio.
        let (master, slave) = open_pty_pair().expect("open_pty_pair");
        disable_opost(master.as_raw_fd()).expect("disable_opost");
        let mut child = std::process::Command::new("/bin/sh")
            // Two writes with a long sleep between them: the first must reach
            // the reader inside that sleep.
            .args(["-c", "printf early; sleep 5; printf late"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(std::fs::File::from(slave)))
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn");
        let mut buf = [0u8; 64];
        // SAFETY: `buf` is live and writable for 64 bytes; `master` owns the fd.
        let n = unsafe { libc::read(master.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        assert!(n > 0, "the first write must be readable mid-run");
        let got = String::from_utf8_lossy(&buf[..n as usize]).to_string();
        assert!(
            got.contains("early"),
            "expected the first write, read {got:?}"
        );
        assert!(
            !got.contains("late"),
            "the second write must not have happened yet, read {got:?}"
        );
        assert_eq!(
            child.try_wait().expect("try_wait"),
            None,
            "the child must still be running — a finished child proves nothing about liveness"
        );
        assert!(
            !got.contains('\r'),
            "OPOST must be clear or the framing is the kernel's, not the child's: {got:?}"
        );
        child.kill().ok();
        child.wait().ok();
    }
}
