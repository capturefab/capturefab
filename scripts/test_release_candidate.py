"""Release lifecycle tests; no network access or real release mutations."""
import importlib.util
from pathlib import Path
import unittest
import subprocess
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('candidate', Path(__file__).with_name('release-candidate.py'))
candidate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(candidate)


class CandidateTests(unittest.TestCase):
    def setUp(self):
        self.items = []
        self.refs = {}
        self.calls = []
        self.api_patch = patch.object(candidate, 'api', side_effect=self.api)
        self.api_patch.start()
        self.release_patch = patch.object(candidate, 'releases', side_effect=lambda repo: self.items.copy())
        self.release_patch.start()
        self.output_patch = patch.object(candidate, 'output')
        self.output = self.output_patch.start()
        self.addCleanup(patch.stopall)

    def api(self, repository, path, payload=None):
        self.calls.append((path, payload))
        if path.startswith('git/matching-refs/tags/'):
            prefix = path.removeprefix('git/matching-refs/tags/')
            return [dict(ref=f'refs/tags/{tag}', object=dict(type='commit', sha=sha))
                    for tag, sha in self.refs.items() if tag.startswith(prefix)]
        if path == 'git/refs':
            self.refs[payload['ref'].removeprefix('refs/tags/')] = payload['sha']
            return {}
        if path == 'releases':
            item = dict(payload, id=len(self.items) + 1, assets=[])
            self.items.append(item)
            return item
        if path.startswith('releases/'):
            item = next(item for item in self.items if item['id'] == int(path.split('/')[1]))
            if payload:
                item.update(payload)
            return item.copy()
        raise AssertionError(path)

    def create(self, sha='abc'):
        candidate.create('owner/repo', '1.2.0', sha)
        return self.items[-1]

    def test_candidate_is_draft_with_rc_tag_and_no_final_tag(self):
        item = self.create()
        self.assertEqual(item['tag_name'], 'v1.2.0-rc.1')
        self.assertTrue(item['draft'])
        self.assertTrue(item['prerelease'])
        self.assertEqual(self.refs, {'v1.2.0-rc.1': 'abc'})

    def test_retry_reuses_same_draft_and_new_commit_increments_rc(self):
        self.create()
        self.create()
        self.assertEqual(len(self.items), 1)
        self.output.assert_called_with(release_id=1, candidate_tag='v1.2.0-rc.1')
        self.create('def')
        self.assertEqual(self.items[-1]['tag_name'], 'v1.2.0-rc.2')
        self.assertTrue(self.items[0]['draft'])

    def test_orphaned_rc_tags_are_not_overwritten(self):
        self.refs['v1.2.0-rc.9'] = 'old'
        self.assertEqual(self.create()['tag_name'], 'v1.2.0-rc.10')

    def test_manually_pushed_rc_is_used_on_the_same_commit(self):
        self.refs['v1.2.0-rc.3'] = 'abc'
        self.assertEqual(self.create()['tag_name'], 'v1.2.0-rc.3')
        self.assertEqual(self.refs, {'v1.2.0-rc.3': 'abc'})

    def test_promotes_same_release_preserving_candidate_tag_and_assets(self):
        item = self.create()
        item['assets'] = [{'name': 'SHA256SUMS'}]
        candidate.promote('owner/repo', '1.2.0', 'abc', item['id'])
        self.assertEqual(len(self.items), 1)
        self.assertEqual(item['tag_name'], 'v1.2.0')
        self.assertFalse(item['draft'])
        self.assertFalse(item['prerelease'])
        self.assertEqual(self.refs, {'v1.2.0-rc.1': 'abc', 'v1.2.0': 'abc'})
        self.assertEqual(item['assets'], [{'name': 'SHA256SUMS'}])
        self.assertIn('v1.2.0-rc.1', item['body'])
        self.output.assert_called_with(published='true')

    def test_rejects_published_missing_assets_changed_commit_or_moved_tag(self):
        item = self.create()
        original = item.copy()
        for change in [dict(draft=False), dict(prerelease=False), dict(target_commitish='other'),
                       dict(tag_name='v1.2.0'), dict(assets=[])]:
            with self.subTest(change=change):
                item.update(original, assets=[{'name': 'SHA256SUMS'}])
                item.update(change)
                with self.assertRaises(ValueError):
                    candidate.promote('owner/repo', '1.2.0', 'abc', item['id'])
                self.assertNotIn('v1.2.0', self.refs)
        item.update(original, assets=[{'name': 'SHA256SUMS'}])
        self.refs[item['tag_name']] = 'other'
        with self.assertRaises(ValueError):
            candidate.promote('owner/repo', '1.2.0', 'abc', item['id'])

    def test_conflicting_final_tag_is_never_moved(self):
        item = self.create()
        item['assets'] = [{'name': 'SHA256SUMS'}]
        self.refs['v1.2.0'] = 'other'
        with self.assertRaises(ValueError):
            candidate.promote('owner/repo', '1.2.0', 'abc', item['id'])
        self.assertTrue(item['draft'])
        self.assertEqual(self.refs['v1.2.0'], 'other')

    def test_existing_final_release_is_never_replaced(self):
        self.items.append(dict(id=10, tag_name='v1.2.0', draft=False))
        with self.assertRaises(ValueError):
            self.create()
        self.assertEqual(self.refs, {})


class ApiErrorTests(unittest.TestCase):
    def test_mutation_failure_preserves_github_diagnostic_without_request_payload(self):
        error = subprocess.CalledProcessError(1, ['gh', 'api'],
            output='{"message":"Validation Failed"}', stderr='HTTP 422: already_exists')
        with patch.object(candidate.subprocess, 'run', side_effect=error):
            with self.assertRaises(RuntimeError) as raised:
                candidate.api('owner/repo', 'releases', {'body': 'private request body'})
        self.assertIn('GitHub POST releases failed:', str(raised.exception))
        self.assertIn('HTTP 422: already_exists', str(raised.exception))
        self.assertIn('Validation Failed', str(raised.exception))
        self.assertNotIn('private request body', str(raised.exception))
