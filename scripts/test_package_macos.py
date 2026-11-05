"""Promoting a package must preserve its binary and never invoke a build."""
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
import zipfile


SCRIPTS = Path(__file__).resolve().parent
FAKE_TOOL = r'''
import json, os
from pathlib import Path
import shutil, sys, zipfile
name = Path(sys.argv[0]).name
args = sys.argv[1:]
if name == 'uname':
    print('Darwin' if args == ['-s'] else 'arm64')
elif name == 'cargo':
    if args[0] != 'metadata':
        raise SystemExit('Compilation is forbidden during package promotion')
    print(json.dumps({'packages': [{'name': 'trash-amp', 'version': '0.1.0-beta.5'}]}))
elif name == 'ditto':
    if args[:2] == ['-x', '-k']:
        with zipfile.ZipFile(args[2]) as archive:
            archive.extractall(args[3])
    else:
        source, destination = map(Path, args[-2:])
        with zipfile.ZipFile(destination, 'w') as archive:
            for path in source.rglob('*'):
                if path.is_file():
                    archive.write(path, path.relative_to(source))
'''


class PackageMacOSTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        scripts = self.directory / 'scripts'
        scripts.mkdir()
        for name in ('package-macos.sh', 'macos_version.py'):
            shutil.copy2(SCRIPTS / name, scripts / name)
        self.log = self.directory / 'commands'
        for name, content in {
            'verify-macos-package.sh': 'echo verify >> "$TOOL_LOG"',
            'sign-macos.sh': 'echo sign >> "$TOOL_LOG"',
            'notarize-macos.sh': 'echo notarize >> "$TOOL_LOG"',
            'bundle-macos.sh': 'echo "Unexpected rebuild" >&2; exit 1',
        }.items():
            tool = scripts / name
            tool.write_text('#!/bin/sh\nset -eu\n' + content + '\n')
            tool.chmod(0o755)
        tools = self.directory / 'bin'
        tools.mkdir()
        for name in ('uname', 'cargo', 'ditto'):
            tool = tools / name
            tool.write_text(f'#!{sys.executable}\n' + FAKE_TOOL)
            tool.chmod(0o755)
        self.version = '0.1.0-beta.5'
        notes = self.directory / 'docs/releases'
        notes.mkdir(parents=True)
        (notes / f'{self.version}.md').write_text('Release notes\n')
        self.archive = self.directory / f'trash-amp-{self.version}-macos-arm64.zip'
        with zipfile.ZipFile(self.archive, 'w') as archive:
            archive.writestr('READ-ME.md', 'Release notes\n')
            archive.writestr('Trash Amp.app/Contents/MacOS/trash-amp', b'original tested binary')
            archive.writestr('Trash Amp.app/Contents/Resources/license', b'original notice')
        self.env = {
            key: value for key, value in os.environ.items()
            if not key.startswith(('MACOS_', 'RELEASE_TAG', 'REQUIRE_MACOS_', 'CARGO_TARGET_DIR'))
        }
        self.env.update(PATH=f'{tools}{os.pathsep}{os.environ["PATH"]}',
                        TOOL_LOG=str(self.log), MACOS_SIGN_IDENTITY='Developer ID Application: Test',
                        MACOS_NOTARY_PROFILE='test', RELEASE_TAG=f'v{self.version}')

    def package(self, **environment):
        return subprocess.run(
            [str(self.directory / 'scripts/package-macos.sh'), '--signed',
             '--from-archive', str(self.archive)],
            env={**self.env, **environment}, capture_output=True, text=True,
        )

    def test_promotes_existing_binary_without_compiling(self):
        result = self.package()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.log.read_text().splitlines(), ['verify', 'sign', 'notarize'])
        final = self.directory / 'target/dist' / self.archive.name
        with zipfile.ZipFile(final) as archive:
            self.assertEqual(archive.read('Trash Amp.app/Contents/MacOS/trash-amp'),
                             b'original tested binary')
            self.assertEqual(archive.read('Trash Amp.app/Contents/Resources/license'),
                             b'original notice')
        self.assertEqual(Path(str(final) + '.sha256').read_text().split(),
                         [hashlib.sha256(final.read_bytes()).hexdigest(), final.name])

    def test_tag_version_mismatch_stops_before_signing(self):
        result = self.package(RELEASE_TAG='v0.1.0-beta.9')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('does not match Cargo version', result.stderr)
        self.assertFalse(self.log.exists())

    def test_changed_release_notes_stop_before_signing(self):
        (self.directory / f'docs/releases/{self.version}.md').write_text('Changed notes\n')
        result = self.package()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.log.read_text().splitlines(), ['verify'])
