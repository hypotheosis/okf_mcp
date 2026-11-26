"""Regression tests for scripts/test-redis.sh and scripts/test-pg.sh `run`.

A stub `docker` on PATH stands in for the real one. `up` runs inside a command
substitution, where Bash clears errexit, so a failed startup step used to be
swallowed and the command ran against a server that never came up.

Run: python3 -m unittest scripts.test_test_servers
"""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# The stub keeps the names of the containers it "started" in `state`, logs
# every call in `calls`, and fails what $STUB_FAIL names: `redis-ready`
# (the standalone Redis never answers PONG) or `pg-run` (`docker run` for
# Postgres fails).
STUB = r"""#!/bin/bash
echo "$*" >> "$STUB_DIR/calls"
case "$1" in
run)
    name=
    prev=
    for arg in "$@"; do
        [ "$prev" = --name ] && name=$arg
        prev=$arg
    done
    if [ "$STUB_FAIL" = pg-run ] && [[ " $* " == *postgres* ]]; then exit 1; fi
    echo "$name" >> "$STUB_DIR/state"
    ;;
exec)
    case "$*" in
    *redis-cli*ping*) [ "$STUB_FAIL" = redis-ready ] && exit 1; echo PONG ;;
    *valkey-cli*ping*) echo PONG ;;
    *"cluster info"*) echo cluster_state:ok ;;
    *) ;;
    esac
    ;;
port) echo "127.0.0.1:41234" ;;
container) grep -qx "$3" "$STUB_DIR/state" ;;
rm)
    for arg in "$@"; do
        case "$arg" in -*|rm) ;; *) sed -i "/^$arg\$/d" "$STUB_DIR/state" ;; esac
    done
    ;;
esac
exit 0
"""


class ServerScripts(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)
        (self.dir / "bin").mkdir()
        docker = self.dir / "bin" / "docker"
        docker.write_text(STUB)
        docker.chmod(0o755)
        (self.dir / "state").write_text("")
        self.marker = self.dir / "command-ran"

    def run_script(self, script, fail):
        env = {
            "PATH": f"{self.dir / 'bin'}:{os.environ['PATH']}",
            "STUB_DIR": str(self.dir),
            "STUB_FAIL": fail,
            "TEST_REDIS_READY_TIMEOUT": "1",
            "HOME": str(self.dir),
        }
        return subprocess.run(
            [str(ROOT / "scripts" / script), "run", "touch", str(self.marker)],
            env=env, text=True, capture_output=True, timeout=60,
        )

    def assert_refused_and_cleaned_up(self, result, containers):
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.marker.exists(), "the command must not run")
        calls = (self.dir / "calls").read_text()
        for name in containers:
            self.assertRegex(calls, rf"rm .*{name}", f"{name} was not removed")
        self.assertEqual((self.dir / "state").read_text().strip(), "")

    def test_redis_not_ready_while_the_cluster_is_ready_stops_the_run(self):
        result = self.run_script("test-redis.sh", "redis-ready")
        self.assert_refused_and_cleaned_up(
            result, ["okf-mcp-test-redis-[0-9]+", "okf-mcp-test-redis-[0-9]+-cluster"]
        )

    def test_a_failed_postgres_start_stops_the_run(self):
        result = self.run_script("test-pg.sh", "pg-run")
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.marker.exists(), "the command must not run")


if __name__ == "__main__":
    unittest.main()
