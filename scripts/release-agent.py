#!/usr/bin/env python3
"""Agent release loop: start an RC, inspect validation, collect logs, retry infrastructure."""
import argparse
import json
from pathlib import Path
import re
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def run(*command):
    return subprocess.check_output(command, cwd=ROOT, text=True).strip()


def gh_json(*command):
    return json.loads(run('gh', *command))


def next_tag(version, tags):
    pattern = re.compile(re.escape(f'v{version}') + r'-rc\.([1-9][0-9]*)$')
    numbers = [int(match[1]) for tag in tags if (match := pattern.fullmatch(tag))]
    return f'v{version}-rc.{max(numbers, default=0) + 1}'


def start(repository, remote):
    if run('git', 'status', '--porcelain'):
        raise ValueError('Commit the intended release changes first; the worktree must be clean')
    version = tomllib.loads((ROOT / 'Cargo.toml').read_text())['package']['version']
    if not re.fullmatch(r'\d+\.\d+\.\d+', version):
        raise ValueError('Use the intended stable version in Cargo.toml; RC tags are allocated automatically')
    sha = run('git', 'rev-parse', 'HEAD')
    remote_url = run('git', 'remote', 'get-url', remote)
    expected = [f'git@github.com:{repository}.git', f'https://github.com/{repository}.git',
                f'https://github.com/{repository}', f'ssh://git@github.com/{repository}.git']
    if remote_url not in expected:
        raise ValueError(f'Remote {remote} does not match {repository}; refusing to tag another repository')
    refs = run('git', 'ls-remote', '--tags', remote, f'refs/tags/v{version}*')
    tags = [line.split('\t', 1)[1].removeprefix('refs/tags/') for line in refs.splitlines()]
    if f'v{version}' in tags:
        raise ValueError(f'v{version} already exists; choose a new version rather than replacing a release')
    tags += run('git', 'tag', '--list', f'v{version}-rc.*').splitlines()
    tag = next_tag(version, tags)
    # Supplying a message also works when the user's Git configuration signs tags.
    run('git', 'tag', '-a', tag, '-m', f'Release candidate {tag}', sha)
    # Never force-push or move a tag. A collision fails safely; start again for a new number.
    run('git', 'push', remote, f'refs/tags/{tag}')
    return dict(schema_version=1, candidate_tag=tag, version=version, commit=sha,
                actions_url=f'https://github.com/{repository}/actions/workflows/release.yml',
                next_command=f'python3 scripts/release-agent.py status --commit {sha}')


def select_run(repository, run_id=None, commit=None):
    if run_id is not None:
        item = gh_json('run', 'view', str(run_id), '--repo', repository,
                       '--json', 'databaseId,workflowName,headSha,headBranch,status,conclusion,url,jobs')
        if item['workflowName'] != 'Release':
            raise ValueError('Run is not the Release workflow')
        return item
    if not commit:
        raise ValueError('Supply --run or --commit to identify the candidate explicitly')
    items = gh_json('run', 'list', '--repo', repository, '--workflow', 'release.yml',
                    '--commit', commit, '--limit', '100',
                    '--json', 'databaseId,headBranch,event')
    # A version bump also starts a main-branch run. Prefer the RC tag run.
    items = [item for item in items if re.search(r'-rc\.[1-9][0-9]*$', item['headBranch'])]
    if not items:
        return None
    return select_run(repository, run_id=items[0]['databaseId'])


def summarize(item):
    if item is None:
        return dict(schema_version=1, state='not_started', next_action='wait_for_tag_workflow')
    failures = [dict(name=job['name'], conclusion=job['conclusion'], url=job['url'],
                     failed_steps=[step['name'] for step in job.get('steps', [])
                                   if step['conclusion'] == 'failure'])
                for job in item['jobs'] if job['conclusion'] in ('failure', 'timed_out', 'cancelled')]
    published = any(job['name'] == 'Promote successful release candidate' and job['conclusion'] == 'success'
                    for job in item['jobs'])
    if item['status'] != 'completed':
        state, action = 'validating', 'wait'
    elif failures:
        state, action = ('published_with_followup_failure', 'fix_site_or_followup') if published else (
            'candidate_failed', 'inspect_logs_fix_commit_and_start_next_candidate')
    elif item['conclusion'] == 'success' and published:
        state, action = 'published', 'done'
    else:
        state, action = 'not_published', 'inspect_skipped_or_cancelled_jobs'
    return dict(schema_version=1, state=state, next_action=action, run_id=item['databaseId'],
                commit=item['headSha'], candidate_tag=item['headBranch'], url=item['url'],
                conclusion=item['conclusion'], failures=failures)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repository', default='capturefab/capturefab')
    commands = parser.add_subparsers(dest='command', required=True)
    create = commands.add_parser('start', help='push the next RC tag from the clean, committed worktree')
    create.add_argument('--remote', default='origin')
    for name in ('status', 'logs', 'retry'):
        command = commands.add_parser(name)
        selection = command.add_mutually_exclusive_group(required=True)
        selection.add_argument('--run', type=int)
        selection.add_argument('--commit')
    args = parser.parse_args()
    if args.command == 'start':
        result = start(args.repository, args.remote)
    else:
        item = select_run(args.repository, args.run, args.commit)
        if args.command == 'status':
            result = summarize(item)
        else:
            if item is None:
                raise ValueError('The RC workflow has not appeared yet; check status again shortly')
            run_id = str(item['databaseId'])
            if args.command == 'logs':
                subprocess.run(['gh', 'run', 'view', run_id, '--repo', args.repository, '--log-failed'],
                               cwd=ROOT, check=True)
                return
            if item['status'] != 'completed' or item['conclusion'] not in ('failure', 'timed_out'):
                raise ValueError('Only completed failed runs can be retried')
            run('gh', 'run', 'rerun', run_id, '--repo', args.repository, '--failed')
            result = dict(schema_version=1, run_id=item['databaseId'], state='retry_requested')
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    try:
        main()
    except (ValueError, subprocess.CalledProcessError) as error:
        print(f'release-agent: {error}', file=sys.stderr)
        sys.exit(1)
