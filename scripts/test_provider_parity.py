import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("provider_parity", Path(__file__).with_name("provider-parity.py"))
parity = importlib.util.module_from_spec(spec)
spec.loader.exec_module(parity)


class BaselineEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.checkout = self.root / "fixture"
        self.checkout.mkdir()
        (self.checkout / "fixture.lock").write_text("pinned\n")
        self.reference = {"repository": "fixture/repo", "commit": "0" * 40,
                          "locks": ["fixture.lock"], "install": [], "checks": []}

    def command(self, source):
        self.reference["checks"].append([sys.executable, "-c", source])

    def evidence(self):
        return json.loads((self.root / "fixture.baseline.json").read_text())

    def test_success_records_commands_and_hashes(self):
        self.command("print('offline fixture passed')")
        self.assertTrue(parity.baseline(self.root, "fixture", self.reference))
        record = self.evidence()
        self.assertEqual(record["locksBefore"], record["locksAfter"])
        self.assertEqual(record["commands"][0]["exitCode"], 0)
        self.assertEqual(record["commands"][0]["logSha256"], parity.digest(self.root / "fixture-0.log"))

    def test_failure_is_not_a_pass_and_stops_later_commands(self):
        self.command("raise SystemExit(7)")
        self.command("from pathlib import Path; Path('must-not-run').touch()")
        self.assertFalse(parity.baseline(self.root, "fixture", self.reference))
        self.assertFalse(self.evidence()["passed"])
        self.assertEqual(self.evidence()["commands"][0]["exitCode"], 7)
        self.assertFalse((self.checkout / "must-not-run").exists())

    def test_lock_mutation_invalidates_success(self):
        self.command("from pathlib import Path; Path('fixture.lock').write_text('changed')")
        with self.assertRaisesRegex(ValueError, "changed a lockfile"):
            parity.baseline(self.root, "fixture", self.reference)
        self.assertFalse(self.evidence()["passed"])

    def test_existing_unverified_directory_is_not_overwritten(self):
        with patch.object(parity.subprocess, "run") as command:
            with self.assertRaisesRegex(ValueError, "unverified"):
                parity.fetch(self.root, "fixture", self.reference)
            command.assert_not_called()

    def test_untracked_reference_inputs_are_rejected(self):
        identity = {key: self.reference[key] for key in ("repository", "commit")}
        (self.root / "fixture.reference.json").write_text(json.dumps(identity))
        with patch.object(parity.subprocess, "run"), patch.object(parity.subprocess, "check_output", side_effect=[self.reference["commit"] + "\n", "src/injected.test.ts\n"]):
            with self.assertRaisesRegex(ValueError, "untracked reference inputs"):
                parity.fetch(self.root, "fixture", self.reference)


if __name__ == "__main__":
    unittest.main()
