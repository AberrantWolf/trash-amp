"""Exercise release failure boundaries without credentials or Apple uploads."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPTS = Path(__file__).resolve().parent
FAKE_TOOL = r'''
import json, os
from pathlib import Path
import sys
name = Path(sys.argv[0]).name
args = sys.argv[1:]
with open(os.environ['TOOL_LOG'], 'a') as log:
    log.write(json.dumps([name, *args]) + '\n')
if name == 'codesign' and '--display' in args:
    if os.environ.get('ADHOC'):
        print('Signature=adhoc\nTeamIdentifier=not set', file=sys.stderr)
    else:
        print('CodeDirectory v=20500 flags=0x10000(runtime)\n'
              'Authority=Developer ID Application: Test (TESTTEAM01)\n'
              'Timestamp=Oct 23, 2026 at 9:00:00\nTeamIdentifier=TESTTEAM01', file=sys.stderr)
elif name == 'ditto':
    Path(args[-1]).write_bytes(b'archive')
elif name == 'xcrun' and args[:2] == ['notarytool', 'submit']:
    print(json.dumps({'id': 'test-submission', 'status': os.environ.get('NOTARY_STATUS', 'Accepted')}))
    sys.exit(int(os.environ.get('SUBMIT_EXIT', '0')))
elif name == 'xcrun' and args[:2] == ['notarytool', 'log']:
    Path(args[-1]).write_text('{}')
elif name == 'spctl':
    sys.exit(int(os.environ.get('GATEKEEPER_EXIT', '0')))
elif name == 'cargo':
    print(json.dumps({'packages': [{'name': 'trash-amp', 'version': '0.1.0-beta.1'}]}))
elif name == 'security' and args[0] == 'find-identity':
    print('1) TESTHASH "Developer ID Application: Test (TESTTEAM01)"')
elif name == 'security' and args[0] == 'create-keychain':
    Path(args[-1]).touch()
'''


class MacOSSigningTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.log = self.directory / 'commands.jsonl'
        for name in ('codesign', 'ditto', 'xcrun', 'spctl', 'cargo', 'security'):
            tool = self.directory / name
            tool.write_text(f'#!{sys.executable}\n' + FAKE_TOOL)
            tool.chmod(0o755)
        self.env = {
            key: value for key, value in os.environ.items()
            if not key.startswith(('MACOS_', 'RELEASE_TAG', 'REQUIRE_MACOS_'))
        }
        self.env.update(
            PATH=f'{self.directory}{os.pathsep}{os.environ["PATH"]}',
            TOOL_LOG=str(self.log), MACOS_NOTARY_PROFILE='test-profile',
        )

    def run_notary(self, **environment):
        result = subprocess.run(
            [str(SCRIPTS / 'notarize-macos.sh'), str(self.directory / 'Test.app'),
             str(self.directory / 'notary-logs')],
            env={**self.env, **environment}, capture_output=True, text=True,
        )
        commands = [json.loads(line) for line in self.log.read_text().splitlines()]
        return result, commands

    def test_accepted_submission_staples_validates_and_checks_gatekeeper(self):
        result, commands = self.run_notary()
        self.assertEqual(result.returncode, 0, result.stderr)
        transitions = [c[:3] for c in commands if c[0] == 'xcrun']
        self.assertEqual(transitions, [
            ['xcrun', 'notarytool', 'submit'], ['xcrun', 'stapler', 'staple'],
            ['xcrun', 'stapler', 'validate'],
        ])
        submit = next(c for c in commands if c[:3] == ['xcrun', 'notarytool', 'submit'])
        self.assertIn('--wait', submit)
        self.assertIn('--timeout', submit)
        self.assertEqual(commands[-1][0], 'spctl')

    def test_rejected_submission_fails_even_when_notarytool_exits_zero(self):
        result, commands = self.run_notary(NOTARY_STATUS='Invalid')
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue((self.directory / 'notary-logs/notary-log.json').exists())
        self.assertFalse(any(c[:2] == ['xcrun', 'stapler'] for c in commands))
        self.assertFalse(any(c[0] == 'spctl' for c in commands))

    def test_timeout_cannot_produce_a_stapled_package(self):
        result, commands = self.run_notary(NOTARY_STATUS='In Progress', SUBMIT_EXIT='69')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any(c[:2] == ['xcrun', 'stapler'] for c in commands))

    def test_gatekeeper_failure_is_fatal_after_acceptance(self):
        result, _ = self.run_notary(GATEKEEPER_EXIT='3')
        self.assertNotEqual(result.returncode, 0)

    def test_adhoc_signature_is_rejected_before_upload(self):
        result, commands = self.run_notary(ADHOC='1')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any(c[0] == 'xcrun' for c in commands))

    def test_release_tag_requires_signing_before_build(self):
        env = {**self.env, 'RELEASE_TAG': 'v0.1.0-beta.1'}
        env.pop('MACOS_NOTARY_PROFILE')
        result = subprocess.run(
            [str(SCRIPTS / 'package-macos.sh')], env=env, capture_output=True, text=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('require a Developer ID identity', result.stderr)
        commands = [json.loads(line) for line in self.log.read_text().splitlines()]
        self.assertFalse(any(c[0] == 'cargo' and 'build' in c for c in commands))

    def run_import(self, team='TESTTEAM01'):
        runner = self.directory / 'runner'
        runner.mkdir()
        environment = self.directory / 'github-env'
        result = subprocess.run(
            [str(SCRIPTS / 'import-macos-signing.sh')], capture_output=True, text=True,
            env={**self.env, 'RUNNER_TEMP': str(runner), 'GITHUB_ENV': str(environment),
                 'MACOS_CERT_P12_BASE64': 'dGVzdC1jZXJ0aWZpY2F0ZQ==',
                 'MACOS_CERT_PASSWORD': 'test-cert-password',
                 'MACOS_KEYCHAIN_PASSWORD': 'test-keychain-password',
                 'MACOS_NOTARY_APPLE_ID': 'test@example.com',
                 'MACOS_NOTARY_PASSWORD': 'test-notary-password',
                 'MACOS_NOTARY_TEAM_ID': team},
        )
        return result, runner / 'macos-signing', environment

    def test_import_uses_team_credentials_and_removes_certificate_file(self):
        result, directory, environment = self.run_import()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((directory / 'certificate.p12').exists())
        configuration = environment.read_text()
        self.assertIn('MACOS_NOTARY_PROFILE=developer-id-notary\n', configuration)
        self.assertIn('MACOS_SIGNING_TEAM_ID=TESTTEAM01\n', configuration)
        self.assertIn(str(directory / 'signing.keychain-db'), configuration)
        self.assertNotIn('test-notary-password', configuration)
        self.assertNotIn('test-cert-password', configuration)

    def test_import_rejects_a_different_notarization_team(self):
        result, directory, environment = self.run_import(team='OTHERTEAM1')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('matching the notarization team', result.stderr)
        self.assertFalse((directory / 'certificate.p12').exists())
        self.assertFalse(environment.exists())
