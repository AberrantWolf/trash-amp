"""Check commit validation independently of artifact availability."""
from contextlib import redirect_stdout
import hashlib
import importlib.util
import io
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import zipfile


SPEC = importlib.util.spec_from_file_location(
    'reuse_macos_artifact', Path(__file__).with_name('reuse-macos-artifact.py'))
reuse = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(reuse)


class ReuseMacOSArtifactTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.outputs = self.directory / 'outputs'
        self.environment = patch.dict(os.environ, {
            'GITHUB_OUTPUT': str(self.outputs), 'GITHUB_RUN_ID': '99',
            'GH_REPO': 'owner/repo', 'GITHUB_SHA': 'abc123',
        })
        self.environment.start()
        self.addCleanup(self.environment.stop)
        self.run = {
            'id': 42, 'head_sha': 'abc123', 'status': 'completed',
            'conclusion': 'success', 'event': 'push',
            'head_repository': {'full_name': 'owner/repo'},
        }
        self.artifact = {'id': 7, 'name': reuse.ARTIFACT_NAME, 'expired': False}
        self.filename = 'trash-amp-0.1.0-beta.5-macos-arm64.zip'
        self.api_calls = []
        self.commands = []

    def api(self, endpoint):
        self.api_calls.append(endpoint)
        if '/artifacts?' in endpoint:
            return {'artifacts': [self.artifact]}
        return {'workflow_runs': [self.run]}

    def command(self, args, **kwargs):
        self.commands.append(args)
        if args[:2] == ['gh', 'api']:
            with zipfile.ZipFile(kwargs['stdout'], 'w') as archive:
                archive.writestr(self.filename, b'tested package')
                digest = hashlib.sha256(b'tested package').hexdigest()
                archive.writestr(self.filename + '.sha256', f'{digest}  {self.filename}\n')
        return subprocess.CompletedProcess(args, 0)

    def lookup(self):
        with patch.object(reuse, 'api', side_effect=self.api), \
             patch.object(reuse.subprocess, 'run', side_effect=self.command), \
             redirect_stdout(io.StringIO()):
            return reuse.reuse('owner/repo', 'abc123', '0.1.0-beta.5', self.directory)

    def output(self):
        return self.outputs.read_text() if self.outputs.exists() else ''

    def test_no_successful_run_requires_tests_and_a_build(self):
        with patch.object(reuse, 'api', return_value={'workflow_runs': []}):
            self.assertIsNone(reuse.reuse('owner/repo', 'abc123', '0.1.0-beta.5', self.directory))
        self.assertEqual(self.output(), '')

    def test_successful_commit_reuses_verified_artifact(self):
        self.assertEqual(self.lookup(), self.directory / self.filename)
        self.assertIn('validated=true\n', self.output())
        self.assertIn('head_sha=abc123&status=success', self.api_calls[0])
        self.assertEqual(self.commands[0], ['gh', 'api', 'repos/owner/repo/actions/artifacts/7/zip'])
        self.assertEqual(Path(self.commands[-1][0]).name, 'verify-macos-package.sh')

    def test_expired_artifact_does_not_invalidate_successful_tests(self):
        self.artifact['expired'] = True
        self.assertIsNone(self.lookup())
        self.assertIn('validated=true\n', self.output())
        self.assertEqual(self.commands, [])

    def test_successful_legacy_run_without_unsigned_artifact_still_skips_tests(self):
        self.artifact['name'] = 'macos-arm64-prerelease'
        self.assertIsNone(self.lookup())
        self.assertIn('validated=true\n', self.output())
        self.assertEqual(self.commands, [])

    def test_other_commits_failed_runs_pull_requests_and_forks_are_not_reused(self):
        variants = [
            {'head_sha': 'different'}, {'conclusion': 'failure'},
            {'status': 'in_progress'}, {'event': 'pull_request'},
            {'head_repository': {'full_name': 'fork/repo'}}, {'id': 99},
        ]
        original = dict(self.run)
        for variant in variants:
            with self.subTest(variant=variant):
                self.run = {**original, **variant}
                self.assertIsNone(self.lookup())
                self.assertEqual(self.output(), '')
        self.assertEqual(self.commands, [])

    def test_manual_success_can_validate_the_same_commit(self):
        self.run['event'] = 'workflow_dispatch'
        self.assertIsNotNone(self.lookup())

    def test_pages_include_older_matching_runs(self):
        first = [{**self.run, 'id': index, 'head_sha': 'different'} for index in range(100)]
        with patch.object(reuse, 'api', side_effect=[{'workflow_runs': first},
                                                   {'workflow_runs': [self.run]}]):
            self.assertEqual(len(list(reuse.pages('runs?status=success', 'workflow_runs'))), 101)

    def main_with_error(self, error):
        def fail(*args):
            reuse.output('validated', 'true')
            raise error
        with patch.object(reuse, 'reuse', side_effect=fail), \
             patch.object(reuse.sys, 'argv', ['reuse-macos-artifact.py', '0.1.0-beta.5']), \
             redirect_stdout(io.StringIO()):
            reuse.main()
        self.assertEqual(self.output(), 'validated=true\n')

    def test_download_failure_keeps_prior_validation_but_rebuilds(self):
        self.main_with_error(subprocess.CalledProcessError(1, 'gh'))

    def test_corrupt_zip_keeps_prior_validation_but_rebuilds(self):
        self.main_with_error(zipfile.BadZipFile('broken download'))

    def test_checksum_mismatch_cannot_be_reused(self):
        def corrupt(args, **kwargs):
            result = self.command(args, **kwargs)
            if args[:2] == ['gh', 'api']:
                with zipfile.ZipFile(kwargs['stdout'], 'w') as archive:
                    archive.writestr(self.filename, b'tampered')
                    archive.writestr(self.filename + '.sha256', 'bad checksum')
            return result
        with patch.object(reuse, 'api', side_effect=self.api), \
             patch.object(reuse.subprocess, 'run', side_effect=corrupt):
            with self.assertRaisesRegex(ValueError, 'checksum mismatch'):
                reuse.reuse('owner/repo', 'abc123', '0.1.0-beta.5', self.directory)
        self.assertEqual(self.output(), 'validated=true\n')

    def test_failed_package_verification_cannot_be_reused(self):
        def reject(args, **kwargs):
            if args[:2] != ['gh', 'api']:
                raise subprocess.CalledProcessError(1, args)
            return self.command(args, **kwargs)
        with patch.object(reuse, 'api', side_effect=self.api), \
             patch.object(reuse.subprocess, 'run', side_effect=reject):
            with self.assertRaises(subprocess.CalledProcessError):
                reuse.reuse('owner/repo', 'abc123', '0.1.0-beta.5', self.directory)
        self.assertEqual(self.output(), 'validated=true\n')
