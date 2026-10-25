"""Exercise actual publication decisions without creating or editing GitHub releases."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPTS = Path(__file__).resolve().parent
FAKE_GH = r'''
import json, os, sys
args = sys.argv[1:]
with open(os.environ['GH_TEST_LOG'], 'a') as log:
    log.write(json.dumps(args) + '\n')
action = args[1]
if action == 'view':
    state = os.environ.get('GH_TEST_STATE', 'absent')
    if state == 'absent':
        print('release not found', file=sys.stderr)
        sys.exit(1)
    if state == 'unavailable':
        print('HTTP 503: Service Unavailable', file=sys.stderr)
        sys.exit(1)
    print({'draft': 'true', 'published': 'false'}.get(state, 'unexpected'))
elif action == os.environ.get('GH_TEST_FAIL'):
    sys.exit(1)
'''


class PublishPrereleaseTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.log = self.directory / 'gh.jsonl'
        tool = self.directory / 'gh'
        tool.write_text(f'#!{sys.executable}\n' + FAKE_GH)
        tool.chmod(0o755)
        self.archive = self.directory / 'trash-amp-0.1.0-beta.1-macos-arm64.zip'
        self.archive.write_bytes(b'verified archive fixture')
        digest = hashlib.sha256(self.archive.read_bytes()).hexdigest()
        Path(str(self.archive) + '.sha256').write_text(f'{digest}  {self.archive.name}\n')

    def publish(self, tag='v0.1.0-beta.1', **environment):
        result = subprocess.run(
            [str(SCRIPTS / 'publish-prerelease.sh'), tag, str(self.directory)],
            env={**os.environ, 'PATH': f'{self.directory}{os.pathsep}{os.environ["PATH"]}',
                 'GH_TEST_LOG': str(self.log), **environment},
            capture_output=True, text=True,
        )
        commands = [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []
        return result, commands

    def test_new_release_publishes_assets_as_a_prerelease(self):
        result, commands = self.publish()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([c[1] for c in commands], ['view', 'create'])
        create = commands[-1]
        for argument in (str(self.archive), str(self.archive) + '.sha256',
                         '--verify-tag', '--prerelease', '--latest=false', '--notes-file'):
            self.assertIn(argument, create)
        self.assertNotIn('--draft', create)

    def test_interrupted_draft_uploads_before_publishing(self):
        result, commands = self.publish(GH_TEST_STATE='draft')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([c[1] for c in commands], ['view', 'upload', 'edit'])
        for argument in ('--draft=false', '--prerelease', '--latest=false', '--verify-tag'):
            self.assertIn(argument, commands[-1])

    def test_failed_upload_does_not_publish_the_draft(self):
        result, commands = self.publish(GH_TEST_STATE='draft', GH_TEST_FAIL='upload')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual([c[1] for c in commands], ['view', 'upload'])

    def test_failed_new_release_creation_is_fatal(self):
        result, commands = self.publish(GH_TEST_FAIL='create')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual([c[1] for c in commands], ['view', 'create'])

    def test_published_release_is_unchanged_on_rerun(self):
        result, commands = self.publish(GH_TEST_STATE='published')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([c[1] for c in commands], ['view'])

    def test_corrupt_archive_cannot_create_or_edit_a_release(self):
        self.archive.write_bytes(b'corrupt archive')
        result, commands = self.publish()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('checksum does not match', result.stderr)
        self.assertEqual(commands, [])

    def test_service_error_cannot_create_or_edit_a_release(self):
        result, commands = self.publish(GH_TEST_STATE='unavailable')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual([c[1] for c in commands], ['view'])

    def test_unexpected_lookup_result_cannot_modify_a_release(self):
        result, commands = self.publish(GH_TEST_STATE='unknown')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual([c[1] for c in commands], ['view'])

    def test_unsupported_tag_or_missing_archive_cannot_modify_a_release(self):
        result, commands = self.publish(tag='v0.1.0')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(commands, [])
        self.archive.unlink()
        result, commands = self.publish()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(commands, [])
