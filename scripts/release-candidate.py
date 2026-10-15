#!/usr/bin/env python3
"""Create an explicitly tagged draft candidate, then promote that same release."""
import argparse
import json
import os
import re
import subprocess


def api(repository, path, payload=None):
    command = ['gh', 'api', f'repos/{repository}/{path}']
    if payload is None:
        return json.loads(subprocess.check_output(command, text=True))
    method = 'PATCH' if re.fullmatch(r'releases/\d+', path) else 'POST'
    try:
        result = subprocess.run(command + ['--method', method, '--input', '-'],
                                input=json.dumps(payload), text=True, capture_output=True, check=True)
    except subprocess.CalledProcessError as error:
        detail = '\n'.join(part.strip() for part in (error.stderr, error.stdout) if part)
        raise RuntimeError(f'GitHub {method} {path} failed: {detail or str(error)}') from None
    return json.loads(result.stdout)


def releases(repository):
    result = subprocess.check_output(
        ['gh', 'api', '--paginate', '--slurp', f'repos/{repository}/releases?per_page=100'], text=True)
    return [release for page in json.loads(result) for release in page]


def output(**values):
    for key, value in values.items():
        print(f'{key}={value}')
    if os.environ.get('GITHUB_OUTPUT'):
        with open(os.environ['GITHUB_OUTPUT'], 'a') as stream:
            for key, value in values.items():
                stream.write(f'{key}={value}\n')


def tag_sha(repository, tag):
    refs = api(repository, f'git/matching-refs/tags/{tag}')
    ref = next((ref for ref in refs if ref['ref'] == f'refs/tags/{tag}'), None)
    if ref is None:
        return None
    obj = ref['object']
    while obj['type'] == 'tag':
        obj = api(repository, f'git/tags/{obj["sha"]}')['object']
    if obj['type'] != 'commit':
        raise ValueError('Release tag must point to a commit')
    return obj['sha']


def ensure_tag(repository, tag, sha):
    existing = tag_sha(repository, tag)
    if existing is None:
        api(repository, 'git/refs', dict(ref=f'refs/tags/{tag}', sha=sha))
    elif existing != sha:
        raise ValueError(f'{tag} points to another commit; refusing to move it')


def create(repository, version, sha):
    final = f'v{version}'
    pattern = re.compile(re.escape(final) + r'-rc\.([1-9][0-9]*)$')
    existing = releases(repository)
    if any(item['tag_name'] == final for item in existing):
        raise ValueError(f'{final} already has a release; refusing to replace it')
    for item in existing:
        if (pattern.fullmatch(item['tag_name']) and item['draft'] and item['prerelease']
                and item['target_commitish'] == sha):
            ensure_tag(repository, item['tag_name'], sha)
            output(release_id=item['id'], candidate_tag=item['tag_name'])
            return
    refs = api(repository, f'git/matching-refs/tags/{final}-rc.')
    tags = [ref['ref'].removeprefix('refs/tags/') for ref in refs]
    tags += [item['tag_name'] for item in existing]
    numbers = [int(match[1]) for tag in tags if (match := pattern.fullmatch(tag))]
    # A manually pushed RC tag (or a retry after draft creation failed) is
    # already a suitable candidate, provided it has no associated release.
    associated = {item['tag_name'] for item in existing}
    unclaimed = [ref['ref'].removeprefix('refs/tags/') for ref in refs
                 if pattern.fullmatch(ref['ref'].removeprefix('refs/tags/'))
                 and ref['ref'].removeprefix('refs/tags/') not in associated
                 and tag_sha(repository, ref['ref'].removeprefix('refs/tags/')) == sha]
    candidate = (max(unclaimed, key=lambda tag: int(pattern.fullmatch(tag)[1])) if unclaimed
                 else f'{final}-rc.{max(numbers, default=0) + 1}')
    ensure_tag(repository, candidate, sha)
    item = api(repository, 'releases', dict(
        tag_name=candidate, target_commitish=sha, name=f'Capturefab {version} release candidate',
        draft=True, prerelease=True,
        body=f'Release candidate `{candidate}` for `{final}`. Builds and tests are pending.\n\n'
             'This draft is promoted automatically only after all validation succeeds.'))
    output(release_id=item['id'], candidate_tag=candidate)


def promote(repository, version, sha, release_id):
    item = api(repository, f'releases/{release_id}')
    pattern = re.escape(f'v{version}') + r'-rc\.[1-9][0-9]*'
    if not (item['draft'] and item['prerelease'] and re.fullmatch(pattern, item['tag_name'])
            and item['target_commitish'] == sha and tag_sha(repository, item['tag_name']) == sha):
        raise ValueError('Promotion requires the original draft RC on the validated commit')
    if not item.get('assets'):
        raise ValueError('Cannot promote a candidate without release assets')
    final = f'v{version}'
    if any(release['tag_name'] == final for release in releases(repository)):
        raise ValueError(f'{final} already has a release; refusing to replace it')
    ensure_tag(repository, final, sha)
    prerelease = '-' in version.split('+')[0]
    api(repository, f'releases/{release_id}', dict(
        tag_name=final, target_commitish=sha, name=f'Capturefab {version}',
        body=item.get('body', '') + f'\n\nPromoted from validated draft candidate `{item["tag_name"]}`.\n',
        draft=False, prerelease=prerelease, make_latest='false' if prerelease else 'true'))
    output(published='true')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['create', 'promote'])
    parser.add_argument('--repository', required=True)
    parser.add_argument('--version', required=True)
    parser.add_argument('--sha', required=True)
    parser.add_argument('--release-id', type=int)
    args = parser.parse_args()
    if args.command == 'create':
        create(args.repository, args.version, args.sha)
    else:
        if args.release_id is None:
            parser.error('promote requires --release-id')
        promote(args.repository, args.version, args.sha, args.release_id)


if __name__ == '__main__':
    main()
