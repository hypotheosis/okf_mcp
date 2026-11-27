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
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

use tempfile::TempDir;

/// The folder's name prefix, so a leftover is recognisable.
pub const FOLDER_PREFIX: &str = "okf-ui-browser-";

/// The prefix of the folders rustwright makes for a browser it launches itself.
const RUSTWRIGHT_PREFIX: &str = "rustwright-profile-";

/// A leftover folder is swept only once it is this old, which keeps the
/// sweep clear of other sessions' live runs.
const SWEEP_MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// The most folders one test process deletes. About 23 thousand were left
/// before issue #1070; every test process removes some more, so the backlog
/// shrinks over a few runs without any one run spending long on it.
const SWEEP_CAP: usize = 500;

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
        sweep_once();
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
/// under `setpriv --pdeathsig KILL` when the installed `setpriv` (util-linux)
/// supports that option. The script runs `setpriv --pdeathsig KILL true`
/// first: older util-linux ships a `setpriv` without `--pdeathsig`, which
/// would exit with an unrecognized-option error instead of starting the
/// browser.
/// `exec` replaces the shell, so the browser is the process rustwright
/// started and the parent-death signal applies to it. Without a capable
/// `setpriv` the browser starts as usual, only without that protection.
fn launcher_script(browser: &str) -> String {
    // Single quotes keep any character of the path literal; a quote inside
    // the path is closed, escaped and reopened.
    let quoted = format!("'{}'", browser.replace('\'', r"'\''"));
    format!(
        "#!/bin/sh\n\
         if command -v setpriv >/dev/null 2>&1 \\\n\
         && setpriv --pdeathsig KILL true >/dev/null 2>&1; then\n\
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

/// Starts the sweep of old leftover folders in the temp dir, at most once
/// per test process, on a detached thread so no journey waits for it
/// (issue #1078). The thread ends with the process.
fn sweep_once() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let spawned = std::thread::Builder::new()
            .name("browser-home-sweep".into())
            .spawn(|| {
                // SAFETY: `geteuid` takes no arguments and cannot fail.
                let uid = unsafe { libc::geteuid() };
                let removed = sweep_stale(
                    &std::env::temp_dir(),
                    SystemTime::now(),
                    SWEEP_MIN_AGE,
                    uid,
                    SWEEP_CAP,
                    &all_cmdlines(),
                );
                if removed > 0 {
                    eprintln!("swept {removed} old browser folders from the temp dir");
                }
            });
        if let Err(error) = spawned {
            eprintln!("could not start the browser folder sweep: {error}");
        }
    });
}

/// Every process's command line, read once. Held in memory only, never
/// printed or logged.
fn all_cmdlines() -> Vec<Vec<u8>> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.bytes().all(|byte| byte.is_ascii_digit()))
        })
        .filter_map(|entry| fs::read(entry.path().join("cmdline")).ok())
        .collect()
}

/// The folders directly under `temp` that are leftovers of a browser: named
/// with one of the two prefixes, a real directory (never a symlink) owned by
/// `uid`, last modified at least `min_age` before `now`, and not named by any
/// command line in `cmdlines`.
///
/// A folder whose path appears anywhere in a command line counts as in use
/// (the profile, or the launcher inside it); that errs towards keeping it.
fn stale_folders(
    temp: &Path,
    now: SystemTime,
    min_age: Duration,
    uid: u32,
    cmdlines: &[Vec<u8>],
) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(temp) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(FOLDER_PREFIX) || name.starts_with(RUSTWRIGHT_PREFIX)
        })
        .filter_map(|entry| {
            // `symlink_metadata` does not follow a link, so a symlink named
            // like a leftover is seen as a symlink and skipped.
            let metadata = fs::symlink_metadata(entry.path()).ok()?;
            let old_enough = metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age >= min_age);
            (metadata.is_dir() && metadata.uid() == uid && old_enough).then(|| entry.path())
        })
        .filter(|path| {
            let needle = path.as_os_str().as_encoded_bytes();
            !cmdlines
                .iter()
                .any(|cmdline| cmdline.windows(needle.len()).any(|part| part == needle))
        })
        .collect()
}

/// Deletes up to `cap` leftover folders (see [`stale_folders`]) under `temp`.
/// A folder that cannot be deleted is skipped: the sweep never fails a test.
///
/// An empty `cmdlines` means `/proc` could not be read (a live machine always
/// has at least this process), so nothing can be shown unused and nothing is
/// deleted.
///
/// # Returns
/// How many folders were removed.
fn sweep_stale(
    temp: &Path,
    now: SystemTime,
    min_age: Duration,
    uid: u32,
    cap: usize,
    cmdlines: &[Vec<u8>],
) -> usize {
    if cmdlines.is_empty() {
        return 0;
    }
    stale_folders(temp, now, min_age, uid, cmdlines)
        .into_iter()
        .take(cap)
        // Another test process may be deleting the same folder; `remove_dir_all`
        // does not follow symlinks inside it, and a failure is ignored.
        .filter(|path| fs::remove_dir_all(path).is_ok())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launcher_quotes_a_path_with_a_quote() {
        let script = launcher_script("/opt/it's/chrome");
        assert!(script.contains(r"'/opt/it'\''s/chrome'"));
    }

    /// Runs the launcher for `/bin/echo` with `PATH` set to `path_dir` only,
    /// and returns what it printed.
    fn run_launcher_with_path(path_dir: &Path) -> String {
        let home = BrowserHome::create("/bin/echo").expect("create");
        let output = std::process::Command::new(home.launcher())
            .arg("started")
            .env("PATH", path_dir)
            .output()
            .expect("run the launcher");
        assert!(
            output.status.success(),
            "the launcher failed: {:?}",
            output.status
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[test]
    fn a_setpriv_without_pdeathsig_takes_the_direct_launch() {
        // A `setpriv` that answers `--version` but rejects `--pdeathsig`,
        // as older util-linux does.
        let bin = tempfile::tempdir().expect("a folder for the fake");
        let fake = bin.path().join("setpriv");
        fs::write(
            &fake,
            "#!/bin/sh\ncase \"$1\" in\n--version) echo fake;;\n*) echo 'unrecognized option' >&2; exit 1;;\nesac\n",
        )
        .expect("write the fake");
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).expect("chmod");

        assert_eq!(run_launcher_with_path(bin.path()), "started");
    }

    #[test]
    fn no_setpriv_takes_the_direct_launch() {
        let empty = tempfile::tempdir().expect("an empty PATH folder");

        assert_eq!(run_launcher_with_path(empty.path()), "started");
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

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    fn me() -> u32 {
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        unsafe { libc::geteuid() }
    }

    /// Makes `name` under `temp` with a modification time `age` ago.
    fn make_aged(temp: &Path, name: &str, age: Duration) -> PathBuf {
        let path = temp.join(name);
        fs::create_dir(&path).expect("make the folder");
        fs::write(path.join("file"), b"x").expect("fill it");
        let when = SystemTime::now() - age;
        fs::File::open(&path)
            .expect("open the folder")
            .set_modified(when)
            .expect("set its time");
        path
    }

    #[test]
    fn only_old_prefixed_folders_are_selected() {
        let temp = tempfile::tempdir().expect("fake temp");
        let old_ui = make_aged(temp.path(), "okf-ui-browser-a", DAY * 2);
        let old_rw = make_aged(temp.path(), "rustwright-profile-b", DAY * 2);
        make_aged(temp.path(), "okf-ui-browser-new", DAY / 2);
        make_aged(temp.path(), "unrelated-folder", DAY * 2);

        let mut found = stale_folders(temp.path(), SystemTime::now(), DAY, me(), &[]);
        found.sort();

        assert_eq!(found, vec![old_ui, old_rw]);
    }

    #[test]
    fn a_folder_named_by_a_running_process_is_kept() {
        let temp = tempfile::tempdir().expect("fake temp");
        let live = make_aged(temp.path(), "rustwright-profile-live", DAY * 2);
        let dead = make_aged(temp.path(), "rustwright-profile-dead", DAY * 2);
        let cmdline = format!("chrome\0--user-data-dir={}\0--flag", live.display()).into_bytes();

        let found = stale_folders(temp.path(), SystemTime::now(), DAY, me(), &[cmdline]);

        assert_eq!(found, vec![dead]);
    }

    #[test]
    fn a_symlink_is_never_selected_or_followed() {
        let temp = tempfile::tempdir().expect("fake temp");
        let target = make_aged(temp.path(), "precious", DAY * 2);
        let link = temp.path().join("okf-ui-browser-link");
        std::os::unix::fs::symlink(&target, &link).expect("link");

        let found = stale_folders(temp.path(), SystemTime::now(), DAY, me(), &[]);
        let removed = sweep_stale(temp.path(), SystemTime::now(), DAY, me(), 10, &[vec![0]]);

        assert!(found.is_empty());
        assert_eq!(removed, 0);
        assert!(target.join("file").is_file());
    }

    #[test]
    fn a_folder_of_another_owner_is_skipped() {
        let temp = tempfile::tempdir().expect("fake temp");
        make_aged(temp.path(), "okf-ui-browser-theirs", DAY * 2);

        let found = stale_folders(temp.path(), SystemTime::now(), DAY, me() + 1, &[]);

        assert!(found.is_empty());
    }

    #[test]
    fn the_sweep_removes_old_folders_up_to_the_cap() {
        let temp = tempfile::tempdir().expect("fake temp");
        for index in 0..5 {
            make_aged(temp.path(), &format!("rustwright-profile-{index}"), DAY * 2);
        }
        let fresh = make_aged(temp.path(), "rustwright-profile-fresh", DAY / 2);

        let removed = sweep_stale(temp.path(), SystemTime::now(), DAY, me(), 3, &[vec![0]]);
        let left = fs::read_dir(temp.path()).expect("list").count();

        assert_eq!(removed, 3);
        assert_eq!(left, 3);
        assert!(fresh.is_dir());
    }

    #[test]
    fn nothing_is_deleted_when_no_process_could_be_read() {
        let temp = tempfile::tempdir().expect("fake temp");
        let old = make_aged(temp.path(), "okf-ui-browser-old", DAY * 2);

        let removed = sweep_stale(temp.path(), SystemTime::now(), DAY, me(), 10, &[]);

        assert_eq!(removed, 0);
        assert!(old.is_dir());
    }
}
