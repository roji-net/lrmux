// PTY: spawn/resize/read/write over nix::pty.

pub fn spawn() {
    // Phase 1+: forkpty-based shell spawn.
    todo!("pty::spawn");
}

pub fn resize() {
    // Phase 1+: set PTY window size (SIGWINCH to child).
    todo!("pty::resize");
}
