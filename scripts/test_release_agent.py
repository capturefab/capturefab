"""Verify agent decisions and ensure starting a candidate never moves existing tags."""
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('agent', Path(__file__).with_name('release-agent.py'))
agent = importlib.util.module_from_spec(spec)
spec.loader.exec_module(agent)


class AgentTests(unittest.TestCase):
    def item(self, status='completed', conclusion='failure', jobs=None):
        return dict(databaseId=42, headSha='abc', headBranch='v1.2.0-rc.1',
                    status=status, conclusion=conclusion, url='https://example.invalid/run/42', jobs=jobs or [])

    def job(self, name, conclusion):
        return dict(name=name, conclusion=conclusion, url='https://example.invalid/job/1',
                    steps=[dict(name='Compile', conclusion=conclusion)])

    def test_numbering_ignores_other_versions_and_malformed_suffixes(self):
        self.assertEqual(agent.next_tag('1.2.0', ['v1.2.0-rc.2', 'v1.2.0-rc.10',
                                                'v1.2.1-rc.99', 'v1.2.0-rc.9^{}']), 'v1.2.0-rc.11')

    def test_pending_workflow_and_validation_are_not_reported_as_released(self):
        self.assertEqual(agent.summarize(None)['state'], 'not_started')
        self.assertEqual(agent.summarize(self.item(status='in_progress'))['state'], 'validating')

    def test_failure_reports_actionable_jobs_and_steps(self):
        result = agent.summarize(self.item(jobs=[self.job('Build linux x86_64', 'failure')]))
        self.assertEqual(result['state'], 'candidate_failed')
        self.assertEqual(result['failures'][0]['failed_steps'], ['Compile'])
        self.assertIn('start_next_candidate', result['next_action'])

    def test_success_requires_actual_promotion_not_merely_a_green_run(self):
        self.assertEqual(agent.summarize(self.item(conclusion='success'))['state'], 'not_published')
        result = agent.summarize(self.item(conclusion='success', jobs=[
            self.job('Promote successful release candidate', 'success')]))
        self.assertEqual(result['state'], 'published')

    def test_site_failure_after_promotion_does_not_request_another_binary_candidate(self):
        result = agent.summarize(self.item(jobs=[
            self.job('Promote successful release candidate', 'success'),
            self.job('Update the downloads page', 'failure')]))
        self.assertEqual(result['state'], 'published_with_followup_failure')
        self.assertEqual(result['next_action'], 'fix_site_or_followup')

    def test_dirty_worktree_is_rejected_before_tagging(self):
        with patch.object(agent, 'run', return_value=' M src/lib.rs') as run:
            with self.assertRaisesRegex(ValueError, 'worktree must be clean'):
                agent.start('owner/repo', 'origin')
        self.assertEqual(run.call_count, 1)

    def test_start_picks_number_from_local_and_remote_tags_and_pushes_only_rc(self):
        replies = ['', 'abc', 'git@github.com:owner/repo.git',
                   'one\trefs/tags/v1.2.0-rc.2\none\trefs/tags/v1.2.0-rc.2^{}',
                   'v1.2.0-rc.3', '', '']
        with patch.object(agent.tomllib, 'loads', return_value={'package': {'version': '1.2.0'}}), \
                patch.object(agent, 'run', side_effect=replies) as run:
            result = agent.start('owner/repo', 'origin')
        self.assertEqual(result['candidate_tag'], 'v1.2.0-rc.4')
        self.assertEqual(run.call_args_list[-2].args,
                         ('git', 'tag', '-a', 'v1.2.0-rc.4', '-m', 'Release candidate v1.2.0-rc.4', 'abc'))
        self.assertEqual(run.call_args_list[-1].args, ('git', 'push', 'origin', 'refs/tags/v1.2.0-rc.4'))

    def test_existing_final_tag_prevents_start(self):
        with patch.object(agent.tomllib, 'loads', return_value={'package': {'version': '1.2.0'}}), \
                patch.object(agent, 'run', side_effect=['', 'abc', 'git@github.com:owner/repo.git',
                                                        'one\trefs/tags/v1.2.0']) as run:
            with self.assertRaisesRegex(ValueError, 'already exists'):
                agent.start('owner/repo', 'origin')
        self.assertEqual(run.call_count, 4)

    def test_selection_prefers_candidate_run_over_main_version_bump(self):
        with patch.object(agent, 'gh_json', side_effect=[[
                dict(databaseId=1, headBranch='main'), dict(databaseId=2, headBranch='v1.2.0-rc.1')],
                dict(self.item(), workflowName='Release')]) as gh:
            agent.select_run('owner/repo', commit='abc')
        self.assertEqual(gh.call_args_list[-1].args[2], '2')

    def test_selection_rejects_unrelated_workflow(self):
        with patch.object(agent, 'gh_json', return_value=dict(workflowName='CI')):
            with self.assertRaisesRegex(ValueError, 'not the Release workflow'):
                agent.select_run('owner/repo', run_id=42)
