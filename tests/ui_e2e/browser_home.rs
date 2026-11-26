//! A browser's private folder: its profile and its launcher (issue #1070).
//!
//! rustwright 0.3.0 makes a `/tmp/rustwright-profile-*` folder for every
//! browser it launches and deletes it as soon as the main Chromium process
//! has exited. A child (the network service) can still be flushing its HTTP
//! cache index then, which creates the folder again, so every journey left
//! one behind. And a test process that dies before its `Drop` runs (a
//! timeout, a `kill`) leaves the browser running, since nothing tells
//! Chromium its parent is gone.
//!
//! So the harness owns both. [`BrowserHome`] is one temporary folder holding
//! the browser's profile and a launcher script that starts Chromium with
//! `PR_SET_PDEATHSIG` (through `setpriv`), so the kernel kills the browser
//! when the test process goes away. Dropping a [`BrowserHome`] first waits
//! for every process still using the profile to exit (killing stragglers
//! after a grace period), and only then deletes the folder.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// The folder's name prefix, so a leftover is recognisable.
pub const FOLDER_PREFIX: &str = "okf-ui-browser-";

/// How long a browser's processes get to exit on their own after the
/// browser was closed, before they are killed.
const GRACE: Duration = Duration::from_secs(10);

/// How long to wait for a killed process to disappear.
const AFTER_KILL: Duration = Duration::from_secs(10);

/// One browser's temporary folder. Deleted on drop, after every process that
/// uses the profile inside it has exited.
#[derive(Debug)]
pub struct BrowserHome {
    // `TempDir` removes the folder when it is dropped, which happens after
    // `BrowserHome::drop` has run (Rust drops a value's fields after its own
    // `Drop::drop` body).
    dir: TempDir,
}

impl BrowserHome {
    /// Creates the folder, its `profile` subfolder and the launcher script
    /// for the browser binary at `browser`.
    ///
    /// # Arguments
    /// * `browser` - path of the real Chromium binary the launcher runs.
    ///
    /// # Returns
    /// The new home.
    ///
    /// # Errors
    /// Any I/O error from creating the folder or writing the script.
    pub fn create(browser: &str) -> io::Result<Self> {
        let dir = tempfile::Builder::new().prefix(FOLDER_PREFIX).tempdir()?;
        fs::create_dir(dir.path().join("profile"))?;
        let launcher = dir.path().join("launch");
        fs::write(&launcher, launcher_script(browser))?;
        fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755))?;
        Ok(Self { dir })
    }

    /// The folder itself.
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The `--user-data-dir` to give the browser.
    pub fn profile(&self) -> PathBuf {
        self.dir.path().join("profile")
    }

    /// The executable to launch in place of the browser binary.
    pub fn launcher(&self) -> PathBuf {
        self.dir.path().join("launch")
    }
}

impl Drop for BrowserHome {
    /// Waits for the browser's processes to exit, killing any that outlive
    /// the grace period, so nothing writes into the profile while the folder
    /// is deleted. Never panics: it can run while a journey is failing.
    fn drop(&mut self) {
        let profile = self.profile();
        if wait_until_unused(&profile, GRACE) {
            return;
        }
        for pid in processes_using(&profile) {
            // SAFETY: `kill` takes a pid and a signal number and touches no
            // memory of ours. The pid names a process whose command line
            // carries this home's unique profile path, so it is one of this
            // browser's own.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        wait_until_unused(&profile, AFTER_KILL);
    }
}

/// The launcher script: runs `browser` with the arguments rustwright passes,
/// under `setpriv --pdeathsig KILL` when `setpriv` (util-linux) is present.
/// `exec` replaces the shell, so the browser is the process rustwright
/// started and the parent-death signal applies to it. Without `setpriv` the
/// browser starts as usual, only without that protection.
fn launcher_script(browser: &str) -> String {
    // Single quotes keep any character of the path literal; a quote inside
    // the path is closed, escaped and reopened.
    let quoted = format!("'{}'", browser.replace('\'', r"'\''"));
    format!(
        "#!/bin/sh\n\
         if command -v setpriv >/dev/null 2>&1; then\n\
         exec setpriv --pdeathsig KILL -- {quoted} \"$@\"\n\
         fi\n\
         exec {quoted} \"$@\"\n"
    )
}

/// Polls until no process uses `profile` or `limit` has passed.
///
/// # Returns
/// Whether the profile was unused when the wait ended.
fn wait_until_unused(profile: &Path, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if processes_using(profile).is_empty() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The pids of the processes started with `--user-data-dir=<profile>`.
///
/// Reads each `/proc/<pid>/cmdline` and compares it in memory. The command
/// lines are never printed or logged.
pub fn processes_using(profile: &Path) -> Vec<libc::pid_t> {
    let needle = format!("--user-data-dir={}", profile.display()).into_bytes();
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<libc::pid_t>().ok())
        .filter(|pid| {
            fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|cmdline| cmdline.windows(needle.len()).any(|part| part == needle))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launcher_quotes_a_path_with_a_quote() {
        let script = launcher_script("/opt/it's/chrome");
        assert!(script.contains(r"'/opt/it'\''s/chrome'"));
    }

    #[test]
    fn a_home_is_deleted_on_drop_and_holds_the_launcher() {
        let home = BrowserHome::create("/bin/true").expect("create");
        let path = home.path().to_path_buf();
        assert!(home.launcher().is_file());
        assert!(home.profile().is_dir());
        drop(home);
        assert!(!path.exists());
    }
}
