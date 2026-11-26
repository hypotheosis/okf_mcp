//! A journey leaves nothing behind (issue #1070): no profile folder and no
//! browser process, and a browser dies with the test process that owns it.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use okf_mcp::config::Config;

use crate::browser_home::{processes_using, BrowserHome};
use crate::harness::drive;
use crate::support::server::{files, TestServer};

/// Whether process `pid` is gone. A process that was killed but not yet
/// reaped (a zombie, state `Z` in `/proc/<pid>/stat`) counts as gone.
fn is_gone(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // The state letter follows the command name, which ends at the last
        // closing parenthesis.
        Ok(stat) => stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
        Err(_) => true,
    }
}

/// Polls `condition` for up to a minute (a generous bound, not a pace).
fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_journey_leaves_no_profile_folder_and_no_browser_behind() {
    let server = TestServer::start(
        vec![("main", files(&[("index.md", "# Main\n")]))],
        Config::default(),
    )
    .await;
    let home: Arc<Mutex<Option<PathBuf>>> = Arc::default();
    let seen = Arc::clone(&home);
    let ran = drive(server.base.clone(), move |ui| {
        ui.open("");
        ui.wait_visible("#login");
        // A browser is running, using a folder of the harness's own.
        assert!(ui.home_path().join("profile").is_dir());
        assert!(!processes_using(&ui.home_path().join("profile")).is_empty());
        *seen.lock().expect("record the folder") = Some(ui.home_path().to_path_buf());
    })
    .await;
    if !ran {
        return;
    }

    let folder = home
        .lock()
        .expect("read the folder")
        .clone()
        .expect("the journey recorded its folder");
    assert!(
        !folder.exists(),
        "the browser's folder outlived its journey"
    );
    assert!(
        processes_using(&folder.join("profile")).is_empty(),
        "a browser process outlived its journey"
    );
}

#[test]
fn a_browser_dies_with_the_process_that_started_it() {
    if !Command::new("setpriv")
        .args(["--pdeathsig", "KILL", "true"])
        .status()
        .is_ok_and(|status| status.success())
    {
        eprintln!("skipping: no setpriv with --pdeathsig (util-linux)");
        return;
    }
    // `sleep` stands in for the browser: the launcher runs it as it runs
    // Chromium. A shell starts the launcher in the background and prints its
    // pid (the launcher `exec`s, so it is the sleeper's pid too).
    let home = BrowserHome::create("/bin/sleep").expect("create a home");
    let mut parent = Command::new("sh")
        .args(["-c", "\"$0\" 300 & echo $!; wait"])
        .arg(home.launcher())
        .stdout(Stdio::piped())
        .spawn()
        .expect("start the parent");
    let mut line = String::new();
    BufReader::new(parent.stdout.take().expect("the parent's stdout"))
        .read_line(&mut line)
        .expect("read the sleeper's pid");
    let sleeper: u32 = line.trim().parse().expect("a pid");
    // The pid is printed before the launcher has run `setpriv` and `exec`ed
    // `sleep`, and the parent-death signal is armed by that `exec`; a parent
    // killed earlier would leave nothing to deliver it. So wait until the
    // process is `sleep`.
    wait_for("the launcher to become sleep", || {
        std::fs::read_to_string(format!("/proc/{sleeper}/comm")).is_ok_and(|c| c.trim() == "sleep")
    });

    parent.kill().expect("kill the parent");
    parent.wait().expect("reap the parent");

    wait_for("the sleeper to die with its parent", || is_gone(sleeper));
}

#[test]
fn a_home_waits_for_the_processes_using_its_profile_before_deleting_it() {
    let home = BrowserHome::create("/bin/sleep").expect("create a home");
    let path: PathBuf = home.path().to_path_buf();
    // A process whose command line names the profile, as a browser's does,
    // and which outlives the home's drop unless the drop kills it.
    // (`sh` waits on its stdin, which this test holds open, and the profile
    // path is its `$0`.)
    let mut straggler = Command::new("sh")
        .args(["-c", "read line"])
        .arg(format!("--user-data-dir={}", home.profile().display()))
        .stdin(Stdio::piped())
        .spawn()
        .expect("start the straggler");
    wait_for("the straggler to appear", || {
        !processes_using(&home.profile()).is_empty()
    });

    drop(home);

    assert!(!path.exists(), "the folder should be gone");
    // The drop killed the straggler; reap it.
    let status = straggler.wait().expect("reap the straggler");
    assert!(!status.success());
}
