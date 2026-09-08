// PTY: spawn a child process in a pseudo-terminal, resize, read/write.

use std::ffi::CString;
use std::os::fd::{AsRawFd, OwnedFd};
use std::process;

use nix::pty::{ForkptyResult, Winsize, forkpty};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Pid, execvp};

/// A spawned PTY child: the master fd (for I/O) and the child's PID.
pub struct Pty {
    pub master: OwnedFd,
    pub child_pid: Pid,
}

/// Window size (rows, cols).
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
}

impl Pty {
    /// Spawn a shell (or given command) in a new PTY.
    ///
    /// Uses `forkpty` to create a pseudo-terminal and fork. The child
    /// execs the command; the parent gets the master fd.
    pub fn spawn(argv: &[CString], size: PtySize) -> Self {
        let winsize = Winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        let result = unsafe { forkpty(Some(&winsize), None) }.expect("forkpty failed");

        match result {
            ForkptyResult::Child => {
                // Child process: exec the command.
                // Only async-signal-safe operations allowed here.
                // execvp replaces the process image; if it returns, it failed.
                #[allow(unreachable_code)]
                {
                    execvp(&argv[0], argv).expect("exec failed");
                    process::exit(127);
                }
            }
            ForkptyResult::Parent { master, child } => Pty {
                master,
                child_pid: child,
            },
        }
    }

    /// Resize the PTY window. Sends SIGWINCH to the child.
    pub fn resize(&self, size: PtySize) {
        let ws = Winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // TIOCSWINSZ ioctl
        unsafe {
            libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws);
        }
    }

    /// Get the master fd for read/write operations.
    pub fn master_fd(&self) -> i32 {
        self.master.as_raw_fd()
    }

    /// Check if the child process is still running (non-blocking).
    pub fn child_alive(&self) -> bool {
        match waitpid(self.child_pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => true,
            Ok(_) => false,
            Err(_) => false,
        }
    }

    /// Wait for the child to exit, returning the exit status.
    pub fn wait(&self) -> i32 {
        match waitpid(self.child_pid, None) {
            Ok(WaitStatus::Exited(_, code)) => code,
            Ok(_) => -1,
            Err(_) => -1,
        }
    }
}

/// Build the default argv for spawning a shell.
/// Uses $SHELL, falling back to /bin/sh.
pub fn default_shell_argv() -> Vec<CString> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    vec![CString::new(shell).unwrap()]
}

/// Send a signal to the child process.
pub fn kill_child(pid: Pid, signal: libc::c_int) {
    unsafe {
        libc::kill(pid.as_raw(), signal);
    }
}

/// Exit the process with the given code (used in the child after fork if exec fails).
#[allow(dead_code)]
pub fn child_exit(code: i32) -> ! {
    process::exit(code);
}
