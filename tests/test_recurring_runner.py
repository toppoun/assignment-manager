"""Drive the scheduled runner with a synthetic CLI and notifier."""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import plistlib
import stat
import subprocess
import sys
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[1]
RUNNER = REPO / "scripts/run-recurring-sync.py"
EXIT_CODES = {"success": 0, "failed": 1, "blocked": 2, "partial": 3, "busy": 4}
MARKER = "synthetic-private-credential-marker"

FAKE_CLI = '''
import json
import os
from pathlib import Path
import stat
import sys

assert sys.argv[1] == "--config"
assert sys.argv[3:] == ["sync", "recurring", "--apply"]
assert not sys.stdin.isatty() and sys.stdin.read() == ""
config_copy = Path(sys.argv[2])
assert config_copy.read_bytes() == Path(os.environ["FAKE_CONFIG"]).read_bytes()
assert stat.S_IMODE(config_copy.stat().st_mode) == 0o600
assert json.loads(config_copy.read_bytes())["webclass"] is False
with Path(os.environ["FAKE_CALLS"]).open("a") as file:
    file.write(str(config_copy) + "\\n")
print(os.environ["FAKE_STDOUT"])
print("synthetic-private-credential-marker", file=sys.stderr)
sys.exit(int(os.environ["FAKE_EXIT"]))
'''

FAKE_NOTIFIER = '''
import os
from pathlib import Path
import sys

assert not sys.stdin.isatty() and sys.stdin.read() == ""
with Path(os.environ["FAKE_NOTICES"]).open("a") as file:
    file.write(" ".join(sys.argv[1:]) + "\\n")
print("synthetic-private-credential-marker")
sys.exit(int(os.environ.get("FAKE_NOTIFY_EXIT", "0")))
'''


def report(status, *, error=None, acquire="not_attempted", candidates=None, changes=None,
           root_error=None):
    return {
        "schema_version": 1, "mode": "apply", "status": status, "error": root_error,
        "moodle": {
            "acquisition": acquire,
            "application": "applied" if changes is not None else "not_attempted",
            "candidates": candidates,
            "applied": None if changes is None else {"create": changes[0], "update": changes[1]},
            "error": error,
        },
        "webclass": {"acquisition": "disabled", "application": "not_attempted"},
    }


class RecurringRunnerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.config = self.root / "prod config.json"
        self.settings = {
            "webclass": False, "moodle": {},
            "recurring_sync": {"moodle": {"max_creates": 5, "max_updates": 3}},
        }
        self.write_config()
        code = f"#!{sys.executable}\n" + FAKE_CLI
        directory = self.root / hashlib.sha256(code.encode()).hexdigest()
        directory.mkdir()
        self.cli = directory / "assignment-manager"
        self.cli.write_text(code, encoding="utf-8")
        self.cli.chmod(0o500)
        self.notifier = self.root / "notify"
        self.notifier.write_text(f"#!{sys.executable}\n" + FAKE_NOTIFIER, encoding="utf-8")
        self.notifier.chmod(0o700)
        self.notices = self.root / "notices"
        self.calls = self.root / "calls"
        self.state = self.root / "log directory"

    def write_config(self):
        self.config.write_text(json.dumps(self.settings), encoding="utf-8")

    def invoke(self, report, *, raw=None, cli_exit=None, expected=None, notifier_exit=0,
               expect_log=True):
        status_code = EXIT_CODES[report["status"]] if report is not None else 0
        proc = subprocess.run(
            [sys.executable, "-B", str(RUNNER), "--cli", str(self.cli), "--config",
             str(self.config), "--state-dir", str(self.state), "--notifier", str(self.notifier)],
            capture_output=True, text=True,
            env={
                **os.environ, "FAKE_CONFIG": str(self.config), "FAKE_CALLS": str(self.calls),
                "FAKE_STDOUT": raw if raw is not None else json.dumps(report),
                "FAKE_EXIT": str(cli_exit if cli_exit is not None else status_code),
                "FAKE_NOTICES": str(self.notices), "FAKE_NOTIFY_EXIT": str(notifier_exit),
            },
        )
        self.assertEqual(proc.returncode, status_code if expected is None else expected, proc.stderr)
        self.assertEqual(proc.stdout + proc.stderr, "")
        if not expect_log:
            self.assertFalse((self.state / "runs.jsonl").exists())
            return None
        log = (self.state / "runs.jsonl").read_text(encoding="utf-8")
        self.assertNotIn(MARKER, log)
        return json.loads(log.splitlines()[-1])

    def test_success_logs_counts_and_removes_private_config_copy(self):
        record = self.invoke(report("success", acquire="success", candidates=11, changes=(1, 2)))
        self.assertEqual(record["schema_version"], 1)
        self.assertIsNotNone(record["started_at"])
        self.assertIsNotNone(record["finished_at"])
        self.assertEqual((record["moodle_candidates"], record["created"], record["updated"]), (11, 1, 2))
        self.assertEqual(record["cli_exit_code"], 0)
        self.assertIsNone(record["category"])
        self.assertFalse(Path(self.calls.read_text().strip()).exists())
        self.assertEqual(stat.S_IMODE(self.state.stat().st_mode), 0o700)
        self.assertEqual(stat.S_IMODE((self.state / "runs.jsonl").stat().st_mode), 0o600)

    def test_classifies_failures_and_notifies_once_per_episode(self):
        network = report("failed", error="request_timeout_stage_unknown", acquire="failed")
        auth = report("failed", error="http_authentication", acquire="failed")
        paused = report("blocked", root_error="write_not_approved")
        busy = report("busy", root_error="target_busy")
        for failure, category, consecutive in [(network, "network", 1), (auth, "authentication", 2),
                                                (busy, "busy", 2), (paused, "write_blocked", 3)]:
            record = self.invoke(failure)
            self.assertEqual(record["category"], category)
            self.assertIsNone(record["created"])
            self.assertEqual(record["consecutive_failures"], consecutive)
        self.assertEqual(record["notification"], "submitted")
        for _ in range(4):
            self.assertIsNone(self.invoke(paused)["notification"])
        self.assertEqual(self.notices.read_text().splitlines(), ["write_blocked 3"])
        self.assertEqual(self.invoke(report("success", acquire="success", candidates=0, changes=(0, 0)))
                         ["consecutive_failures"], 0)
        for _ in range(3):
            record = self.invoke(network)
        self.assertEqual(record["notification"], "submitted")
        self.assertEqual(self.notices.read_text().splitlines(), ["write_blocked 3", "network 3"])

    def test_failed_notification_is_recorded_and_not_repeated(self):
        failure = report("blocked", root_error="policy_changed")
        for _ in range(3):
            record = self.invoke(failure, notifier_exit=1)
        self.assertEqual(record["notification"], "failed")
        self.assertIsNone(self.invoke(failure)["notification"])
        self.assertEqual(self.notices.read_text().splitlines(), ["write_blocked 3"])

    def test_unrecognized_diagnostic_and_malformed_output_are_redacted(self):
        record = self.invoke(report("failed", error=MARKER, acquire="failed"))
        self.assertEqual(record["error"], "unclassified")
        record = self.invoke(None, raw=MARKER, expected=1)
        self.assertEqual((record["error"], record["cli_exit_code"]), ("cli_result_invalid", 0))

    def test_rejects_webclass_config_and_unpinned_binary_before_starting_cli(self):
        self.settings["recurring_sync"]["webclass"] = {}
        self.write_config()
        self.assertEqual(self.invoke(None, expected=2)["error"], "non_moodle_configuration")
        del self.settings["recurring_sync"]["webclass"]
        self.settings["webclass"] = True
        self.write_config()
        self.assertEqual(self.invoke(None, expected=2)["error"], "non_moodle_configuration")
        self.settings["webclass"] = False
        self.write_config()
        self.cli.chmod(0o700)
        with self.cli.open("a") as file:
            file.write("# different digest\n")
        self.assertEqual(self.invoke(None, expected=2)["error"], "cli_digest_mismatch")
        self.assertFalse(self.calls.exists())

    def test_rotates_to_three_backups(self):
        self.state.mkdir(mode=0o700)
        log = self.state / "runs.jsonl"
        for index in range(1, 6):
            log.write_bytes(bytes([64 + index]) * (256 * 1024))
            self.invoke(report("success", acquire="success", candidates=0, changes=(0, 0)))
        self.assertEqual(
            [path.name for path in sorted(self.state.glob("runs.jsonl*"))],
            ["runs.jsonl", "runs.jsonl.1", "runs.jsonl.2", "runs.jsonl.3"],
        )
        self.assertEqual((self.state / "runs.jsonl.1").read_bytes()[:1], b"E")
        self.assertEqual((self.state / "runs.jsonl.3").read_bytes()[:1], b"C")
        self.assertLess(log.stat().st_size, 1024)

    def test_overlapping_runner_does_not_start_cli(self):
        self.state.mkdir(mode=0o700)
        with (self.state / "runner.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            self.invoke(None, expected=4, expect_log=False)
        self.assertFalse(self.calls.exists())

    def test_launchagent_template_is_hourly_and_uses_the_same_runner(self):
        with (REPO / "macos/io.assignment-manager.moodle-recurring.plist").open("rb") as file:
            agent = plistlib.load(file)
        self.assertEqual(agent["StartCalendarInterval"], {"Minute": 0})
        self.assertEqual(agent["LimitLoadToSessionType"], "Aqua")
        self.assertEqual(agent["ProgramArguments"][1], "__REPO__/scripts/run-recurring-sync.py")
        self.assertNotIn("RunAtLoad", agent)
        self.assertEqual([agent[key] for key in ("StandardInPath", "StandardOutPath", "StandardErrorPath")],
                         ["/dev/null"] * 3)


if __name__ == "__main__":
    unittest.main()
