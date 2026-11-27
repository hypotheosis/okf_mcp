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
