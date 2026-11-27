# Sweep old browser folders (issue #1078)

The first `BrowserHome::create` in a test process (`tests/ui_e2e/browser_home.rs`)
now starts a detached thread that deletes leftover `okf-ui-browser-*` and
`rustwright-profile-*` folders directly under the temp dir. A folder is deleted
only when it is a real directory (symlinks are skipped, never followed), owned by
the current user, last modified more than a day ago, and its path appears in no
running process's command line (read once into memory, never printed). A failed
delete is ignored. Each process removes at most 500 folders, so the roughly 23
thousand left from before #1070 shrink over a few runs without one run spending
long on them; the thread runs beside the journeys, so none waits for it. When it
removed anything it prints one line with the count. Unit tests cover age, prefix,
in-use, symlink, ownership and the cap against a fake temp dir.

Review round 1: the process snapshot is now checked for completeness. A process
whose command line cannot be read counts against the snapshot when this user
owns it or its owner cannot be found out (a browser this user started is owned by
this user); then the sweep deletes nothing. A process that vanished during the
scan (ENOENT or ESRCH) and a process of another user do not. Tests feed a fake
process source (`ProcRead` values) with one readable process plus one
inaccessible process of this user and assert nothing is removed.
