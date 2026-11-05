#!/usr/bin/env python3
"""Reuse commit validation independently of the availability of its unsigned package."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import zipfile


ARTIFACT_NAME = 'macos-arm64-unsigned-v1'
WORKFLOW = 'macos-alpha.yml'
SCRIPTS = Path(__file__).resolve().parent


def api(endpoint):
    return json.loads(subprocess.check_output(['gh', 'api', endpoint], text=True))


def pages(endpoint, key):
    page = 1
    while True:
        items = api(f'{endpoint}&per_page=100&page={page}')[key]
        yield from items
        if len(items) < 100:
            return
        page += 1


def eligible_run(run, repository, sha):
    return (
        run['head_sha'] == sha
        and run['status'] == 'completed'
        and run['conclusion'] == 'success'
        and run['event'] in ('push', 'workflow_dispatch')
        and run['head_repository']['full_name'] == repository
        and str(run['id']) != os.environ.get('GITHUB_RUN_ID')
    )


def output(name, value):
    with open(os.environ['GITHUB_OUTPUT'], 'a') as destination:
        destination.write(f'{name}={value}\n')


def reuse(repository, sha, version, destination):
    filename = f'trash-amp-{version}-macos-arm64.zip'
    runs = pages(
        f'repos/{repository}/actions/workflows/{WORKFLOW}/runs?head_sha={sha}&status=success',
        'workflow_runs',
    )
    for run in runs:
        if not eligible_run(run, repository, sha):
            continue
        output('validated', 'true')
        print(f'Commit {sha} already passed validation in run {run["id"]}.')
        artifacts = pages(
            f'repos/{repository}/actions/runs/{run["id"]}/artifacts?name={ARTIFACT_NAME}',
            'artifacts',
        )
        for artifact in artifacts:
            if artifact['name'] != ARTIFACT_NAME or artifact['expired']:
                continue
            # Download by immutable artifact ID, never by a branch's latest run.
            destination.mkdir(parents=True, exist_ok=True)
            with tempfile.TemporaryFile() as download:
                subprocess.run(
                    ['gh', 'api', f'repos/{repository}/actions/artifacts/{artifact["id"]}/zip'],
                    stdout=download, check=True,
                )
                download.seek(0)
                with zipfile.ZipFile(download) as bundle:
                    if sorted(bundle.namelist()) != sorted([filename, filename + '.sha256']):
                        raise ValueError('Unexpected artifact contents')
                    bundle.extractall(destination)
            archive = destination / filename
            checksum = Path(str(archive) + '.sha256').read_text().split()
            with archive.open('rb') as source:
                digest = hashlib.file_digest(source, 'sha256').hexdigest()
            if checksum != [digest, filename]:
                raise ValueError('Artifact checksum mismatch')
            subprocess.run(
                [str(SCRIPTS / 'verify-macos-package.sh'), str(archive)], check=True,
                env={**os.environ, 'REQUIRE_MACOS_NOTARIZATION': 'false',
                     'EXPECTED_VERSION': version},
            )
            print(f'Reusing verified unsigned artifact {artifact["id"]} from run {run["id"]}.')
            return archive
    return None


def main():
    try:
        archive = reuse(os.environ['GH_REPO'], os.environ['GITHUB_SHA'],
                        sys.argv[1], Path('target/reused').resolve())
    except (subprocess.SubprocessError, OSError, ValueError, KeyError, zipfile.BadZipFile) as error:
        print(f'Artifact reuse unavailable ({error}); building a fresh package. '
              'Prior successful validation, if found, still applies.')
        return
    if archive:
        output('archive', archive)
    else:
        print('No reusable artifact for this commit; building a fresh package.')


if __name__ == '__main__':
    main()
