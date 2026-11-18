"""The real Sonar wrapper must retain guarded builds and refuse incomplete reports."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("build_tools", ROOT / "scripts/build_tools.py")
tools = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(tools)

DOUBLE = r'''
import json, os, pathlib, shlex, sys
name = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
with open(os.environ['SONAR_TEST_CALLS'], 'a') as log:
    log.write(json.dumps([name, *args]) + '\n')
if name == 'python3':
    if args[0] == '-c':
        print(os.environ['SONAR_TEST_TARGET'])
        sys.exit(0)
    assert pathlib.Path(args.pop(0)).name == 'rust-dev.py', args
    stage = args[0] if args[0] != 'llvm-cov' else args[1]
    if stage == os.environ.get('SONAR_TEST_FAIL'):
        sys.exit(42)
    if stage == 'clippy':
        print(json.dumps({'reason': 'compiler-artifact'}))
    elif stage == 'show-env':
        assert os.environ['CARGO_LLVM_COV_TARGET_DIR'] == os.environ['SONAR_TEST_TARGET']
        print('export SONAR_TEST_INSTRUMENTED=1')
    elif stage in ('clean', 'test', 'report'):
        assert os.environ['SONAR_TEST_INSTRUMENTED'] == '1'
        if stage == 'clean':
            assert '--workspace' in args
            assert os.environ['CARGO_LLVM_COV_DENY_WARNINGS'] == '1'
            if os.environ.get('SONAR_TEST_FAIL') == 'clean-warning':
                print('warning: underlying Cargo clean failed', file=sys.stderr)
                sys.exit(1)
        if stage == 'report':
            pathlib.Path(args[args.index('--output-path') + 1]).write_text('end_of_record\n')
    else:
        raise AssertionError(args)
elif name == 'sonar':
    assert args[0] == 'scan' and '-Dsonar.qualitygate.wait=true' in args
    paths = {key: value for key, value in (arg.split('=', 1) for arg in args if '=' in arg)}
    report = pathlib.Path(paths['-Dsonar.rust.clippyReport.reportPaths'])
    assert json.loads(report.read_text()) == {'reason': 'compiler-artifact'}
    assert pathlib.Path(paths['-Dsonar.rust.lcov.reportPaths']).read_text() == 'end_of_record\n'
else:
    raise AssertionError('unguarded command: ' + name)
'''


@unittest.skipIf(os.name == "nt", "Bash Sonar workflow")
class SonarWorkflowTests(unittest.TestCase):
    def setUp(self):
        root = tools.cache_root() / "test-fixtures"
        root.mkdir(parents=True, exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(dir=root)
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.commands = self.root / "commands"
        self.commands.mkdir()
        for name in ("python3", "sonar", "cargo"):
            command = self.commands / name
            command.write_text(f"#!{sys.executable}\n" + DOUBLE)
            command.chmod(0o755)
        self.calls = self.root / "calls.jsonl"
        self.output = self.root / "reports"
        self.env = dict(os.environ, PATH=str(self.commands) + os.pathsep + os.environ["PATH"],
                        CODEVETRA_SONAR_OUTPUT_ROOT=str(self.output),
                        SONAR_TEST_CALLS=str(self.calls), SONAR_TEST_TARGET=str(self.root / "target"))

    def run_workflow(self, fail=""):
        result = subprocess.run(["bash", str(ROOT / "scripts/sonar-rust.sh")],
                                env=dict(self.env, SONAR_TEST_FAIL=fail),
                                text=True, capture_output=True, timeout=30)
        calls = [json.loads(line) for line in self.calls.read_text().splitlines()]
        return result, calls

    def test_success_uses_guarded_builds_and_scans_only_complete_reports(self):
        result, calls = self.run_workflow()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls[-1][:2], ["sonar", "scan"])
        builds = [call[2:] for call in calls if call[0] == "python3" and call[1] != "-c"]
        tests = [call for call in builds if call[0] == "test"]
        self.assertEqual(len(tests), 4)
        self.assertIn("--workspace", tests[0])
        for command, target in zip(tests[1:], ("rocks_graph", "live_supervisor", "rocks_owner")):
            self.assertIn(target, command)
            self.assertIn("--ignored", command)
            self.assertIn("--test-threads=1", command)
        self.assertFalse(list(self.output.glob("sonar/*.tmp.*")))

    def test_failed_environment_export_stops_before_coverage_or_scan(self):
        result, calls = self.run_workflow("show-env")
        self.assertEqual(result.returncode, 42)
        self.assertFalse(any(call[0] == "sonar" for call in calls))
        self.assertEqual(calls[-1][-3:], ["llvm-cov", "show-env", "--sh"])

    def test_failed_clippy_does_not_publish_partial_or_reuse_stale_report(self):
        report = self.output / "sonar/clippy.json"
        report.parent.mkdir(parents=True)
        report.write_text("stale")
        result, calls = self.run_workflow("clippy")
        self.assertEqual(result.returncode, 42)
        self.assertFalse(report.exists())
        self.assertFalse(list(report.parent.glob("*.tmp.*")))
        self.assertFalse(any(call[0] == "sonar" for call in calls))

    def test_failed_coverage_never_reaches_scanner(self):
        result, calls = self.run_workflow("test")
        self.assertEqual(result.returncode, 42)
        self.assertFalse(any(call[0] == "sonar" for call in calls))

    def test_underlying_cleanup_warning_stops_before_tests_or_scanner(self):
        result, calls = self.run_workflow("clean-warning")
        self.assertEqual(result.returncode, 1)
        self.assertFalse(any(call[0] == "sonar" for call in calls))
        self.assertFalse(any("test" in call for call in calls))


if __name__ == "__main__":
    unittest.main()
