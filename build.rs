fn main() {
    #[cfg(target_os = "macos")]
    {
        // Link to libproc for proc_pidinfo (child CWD lookup).
        println!("cargo:rustc-link-lib=proc");
    }
}
