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

        // Set LRMUX env var so the child process knows it's inside lrmux.
        // The child inherits this via fork; we unset it in the parent after.
        // Safety: we are single-threaded here (before fork), no race possible.
        unsafe { std::env::set_var("LRMUX", "1") };

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
            ForkptyResult::Parent { master, child } => {
                // Unset in the parent so the server process doesn't have it.
                // Safety: single-threaded, no race.
                unsafe { std::env::remove_var("LRMUX") };
                Pty {
                    master,
                    child_pid: child,
                }
            }
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

/// Get the current working directory of a child process by PID.
/// Returns None if the CWD cannot be determined.
pub fn child_cwd(pid: Pid) -> Option<String> {
    let pid = pid.as_raw();
    #[cfg(target_os = "macos")]
    {
        // macOS: use proc_pidinfo with PROC_PIDVNODEPATHINFO.
        unsafe extern "C" {
            fn proc_pidinfo(
                pid: libc::pid_t,
                flavor: u32,
                arg: u64,
                buffer: *mut libc::c_void,
                buffersize: i32,
            ) -> i32;
        }
        const PROC_PIDVNODEPATHINFO: u32 = 9;
        // struct proc_vnodepathinfo { vnode_info_path pvi_cdir; vnode_info_path pvi_rdir; }
        // struct vnode_info_path { vnode_info vip_vi; char vip_path[MAXPATHLEN]; }
        // MAXPATHLEN = 1024 on macOS.
        // We only need pvi_cdir.vip_path, which is at offset sizeof(vnode_info).
        // Total size: 2 * sizeof(vnode_info_path) = 2 * (sizeof(vnode_info) + 1024)
        // But we can use a simpler approach: allocate enough and read the path.
        const MAXPATHLEN: usize = 1024;
        // vnode_info is 48 bytes (vi_stat + vi_type + vi_pad + vi_fsid + vi_fsid_padding).
        // Total vnode_info_path = 48 + 1024 = 1072 bytes.
        // proc_vnodepathinfo = 2 * 1072 = 2144 bytes.
        #[repr(C)]
        struct VnodeInfoPath {
            _vi: [u8; 48], // vnode_info (48 bytes)
            path: [u8; MAXPATHLEN],
        }
        #[repr(C)]
        struct ProcVnodePathInfo {
            cdir: VnodeInfoPath,
            _rdir: VnodeInfoPath,
        }
        let mut info = ProcVnodePathInfo {
            cdir: VnodeInfoPath {
                _vi: [0u8; 48],
                path: [0u8; MAXPATHLEN],
            },
            _rdir: VnodeInfoPath {
                _vi: [0u8; 48],
                path: [0u8; MAXPATHLEN],
            },
        };
        let size = std::mem::size_of::<ProcVnodePathInfo>() as i32;
        let ret = unsafe {
            proc_pidinfo(
                pid,
                PROC_PIDVNODEPATHINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        if ret > 0 {
            let path_cstr =
                unsafe { std::ffi::CStr::from_ptr(info.cdir.path.as_ptr() as *const libc::c_char) };
            let path = path_cstr.to_string_lossy().into_owned();
            if !path.is_empty() {
                return std::path::Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned());
            }
        }
        None
    }
    #[cfg(target_os = "linux")]
    {
        // Linux: read /proc/<pid>/cwd symlink.
        let link = format!("/proc/{pid}/cwd");
        std::fs::read_link(&link)
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}
