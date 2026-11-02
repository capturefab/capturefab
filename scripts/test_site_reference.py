#!/usr/bin/env python3
"""Unit tests for scripts/site-reference.py, the generated command reference."""
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('site_reference', SCRIPTS / 'site-reference.py')
reference = importlib.util.module_from_spec(spec)
spec.loader.exec_module(reference)

HELP = '''Save frames: one shot, a burst

Usage: capturefab capture [OPTIONS]

Options:
  -c, --camera <CAM>    Camera [env: CAPTUREFAB_CAMERA]
  -h, --help            Print help

Never overwrites; add {seq} to -o for a series.

Examples:
  capturefab capture -c sim:0 -o shot.png
  capturefab capture -c sim:0 -n 10 -o frames/
'''

TOP = '''Camera capture for people and agents.

Usage: capturefab [OPTIONS] [COMMAND]

Commands:
  list     List cameras [alias: ls]
  capture  Save frames

Options:
      --json            One JSON document on stdout [env: CAPTUREFAB_JSON=]
  -q, --quiet           No progress on stderr
      --session <NAME>  Use this running session [env: CAPTUREFAB_SESSION=]

Examples:
  capturefab list
'''


def snapshot():
    schema = dict(contract_version=1, commands=dict(capture=['file_write', 'Save frames'], frobnicate=['read', 'Something new']), effect_with={},
                  conventions=dict(json='--json: one document', results='snake_case keys', credentials='never printed', describe='capturefab schema COMMAND',
                                   globals='--json, -q', camera='-c/--camera CAM', routing='--session NAME', jobs='--background prints a job'),
                  errors=dict(usage=[2, 'fix the command'], exists=[1, 'never overwritten <ever>']),
                  exit_codes={'0': 'ok', '130': 'interrupted', '2': 'usage'},
                  grammars=dict(duration='250ms, 2s', template='{seq} {time}'))
    capture = dict(command='capture', summary='Save frames', effect='file_write', contract_version=1, errors=['usage', 'exists'],
                   args=[dict(name='cameras', long='--camera', short='-c', value_name='CAM', type='camera', help='Camera, repeatable', env='CAPTUREFAB_CAMERA', repeatable=True, required=False),
                         dict(name='dry_run', long='--dry-run', type='flag', default='false', help='Capture nothing', repeatable=False, required=False),
                         dict(name='format', long='--format', short='-f', value_name='FORMAT', type='choice', choices=['png', 'jpg'], default='png', help='Format', repeatable=False, required=False),
                         dict(name='names', value_name='NAME', type='name', help='Features', repeatable=True, required=True)],
                   result={'files': '[string] — absolute paths', 'count': 'int — frames saved'}, examples=['capturefab capture -c sim:0 -o shot.png'], notes=['Never overwrites.'], aliases=['shoot'])
    frobnicate = dict(command='frobnicate', summary='Something new', effect='read', contract_version=1, errors=[], args=[], examples=[], notes=[])
    return dict(about='Camera capture for people & agents.', global_options=[dict(long='--json', short=None, value_name=None, help='One document', env='CAPTUREFAB_JSON')],
                schema=schema, commands=dict(capture=capture, frobnicate=frobnicate), groups=dict(preset=dict(summary='Saved settings', notes=['Lives on this computer.'], examples=['capturefab preset list'])))


class ParseHelpTests(unittest.TestCase):
    def test_command_help_gives_examples_and_notes(self):
        parsed = reference.parse_help(HELP)
        self.assertEqual(parsed['about'], 'Save frames: one shot, a burst')
        self.assertEqual(parsed['examples'], ['capturefab capture -c sim:0 -o shot.png', 'capturefab capture -c sim:0 -n 10 -o frames/'])
        self.assertEqual(parsed['notes'], ['Never overwrites; add {seq} to -o for a series.'])

    def test_top_level_help_gives_globals_and_aliases(self):
        parsed = reference.parse_help(TOP)
        self.assertEqual(parsed['aliases'], {'list': ['ls']})
        self.assertEqual([(o['short'], o['long'], o['value_name'], o['env']) for o in parsed['options']],
                         [(None, '--json', None, 'CAPTUREFAB_JSON'), ('-q', '--quiet', None, None), (None, '--session', 'NAME', 'CAPTUREFAB_SESSION')])
        self.assertEqual(parsed['options'][0]['help'], 'One JSON document on stdout')


class RenderTests(unittest.TestCase):
    def setUp(self):
        self.page = reference.render(snapshot())

    def test_every_command_and_group_has_an_anchor(self):
        for anchor in ('command-capture', 'command-frobnicate', 'command-preset', 'error-usage', 'error-exists', 'cli-basics', 'cli-global-options', 'cli-grammars'):
            self.assertIn(f'id="{anchor}"', self.page)
        # A command without a section of its own still appears.
        self.assertIn('id="cli-more"', self.page)
        self.assertIn('<a href="#cli-more">More commands</a>', self.page)

    def test_arguments_examples_results_and_errors(self):
        self.assertIn('<code>-c</code>, <code>--camera</code> CAM', self.page)
        self.assertIn('env <code>CAPTUREFAB_CAMERA</code>; repeatable', self.page)
        self.assertIn('one of <code>png</code>, <code>jpg</code>; default <code>png</code>', self.page)
        self.assertIn('<code>NAME...</code>', self.page)
        self.assertNotIn('default <code>false</code>', self.page)
        self.assertIn('capturefab capture -c sim:0 -o shot.png', self.page)
        self.assertIn('<code>files</code>', self.page)
        self.assertIn('<a href="#error-exists"><code>exists</code></a>', self.page)
        self.assertIn('Alias: <code>shoot</code>', self.page)

    def test_examples_quote_bracketed_names_for_zsh(self):
        self.assertEqual(reference.shell_safe('capturefab set -c sim:0 TriggerMode[FrameStart]=On trigger=off'),
                         "capturefab set -c sim:0 'TriggerMode[FrameStart]=On' trigger=off")
        for line in ("capturefab exec -c sim:0 'TriggerSoftware[FrameStart]'", "capturefab features -c sim:0 'Exposure*'",
                     'echo \'{"names":[{"name":"Width"}]}\' | capturefab debug rpc -c sim:0'):
            self.assertEqual(reference.shell_safe(line), line)
        snap = snapshot()
        snap['commands']['capture']['examples'] = ['capturefab get -c sim:0 TriggerMode[FrameStart]']
        self.assertIn('capturefab get -c sim:0 &#x27;TriggerMode[FrameStart]&#x27;', reference.render(snap))

    def test_text_is_escaped_and_exit_codes_are_numeric_order(self):
        self.assertIn('people &amp; agents', self.page)
        self.assertIn('never overwritten &lt;ever&gt;', self.page)
        self.assertLess(self.page.index('<td><code>2</code></td>'), self.page.index('<td><code>130</code></td>'))

    def test_sections_are_search_documents(self):
        spec = importlib.util.spec_from_file_location('docs_search', SCRIPTS / 'docs-search.py')
        search = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(search)
        parser = search.Documentation('reference.html')
        parser.feed(self.page)
        parser.close()
        self.assertEqual(parser.errors, [])
        self.assertIn('cli-basics', [document['id'] for document in parser.documents])


class CommandLineTests(unittest.TestCase):
    def run_script(self, *args):
        return subprocess.run([sys.executable, '-B', str(SCRIPTS / 'site-reference.py'), *map(str, args)], capture_output=True, text=True)

    def test_check_detects_a_stale_page(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = Path(temp)
            (temp / 'cli-schema.json').write_text(json.dumps(snapshot()))
            page = temp / 'reference.html'
            self.assertEqual(self.run_script('--snapshot', temp / 'cli-schema.json', '--page', page).returncode, 0)
            self.assertEqual(self.run_script('--check', '--snapshot', temp / 'cli-schema.json', '--page', page).returncode, 0)
            page.write_text(page.read_text() + ' ')
            result = self.run_script('--check', '--snapshot', temp / 'cli-schema.json', '--page', page)
            self.assertEqual(result.returncode, 1)
            self.assertIn('stale', result.stderr)

    @unittest.skipIf(os.name == 'nt', 'uses a shell script as the binary')
    def test_binary_snapshot_records_schema_and_help(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = Path(temp)
            bare = snapshot()['schema']
            bare['commands'] = dict(list=['read', 'List cameras'])
            detail = dict(command='list', summary='List cameras', effect='read', contract_version=1, errors=[], args=[])
            fake = temp / 'capturefab'
            fake.write_text('#!/bin/sh\n'
                            'if [ -n "$CAPTUREFAB_CAMERA" ]; then echo leaked >&2; exit 9; fi\n'
                            f'if [ "$1" = schema ] && [ $# = 1 ]; then cat <<\'JSON\'\n{json.dumps(bare)}\nJSON\nexit 0; fi\n'
                            f'if [ "$1" = schema ]; then cat <<\'JSON\'\n{json.dumps(detail)}\nJSON\nexit 0; fi\n'
                            f'if [ "$1" = -h ]; then cat <<\'HELP\'\n{TOP}HELP\nexit 0; fi\n'
                            'printf "List cameras\\n\\nExamples:\\n  capturefab list --kind sim\\n"\n')
            fake.chmod(0o755)
            env = dict(os.environ, CAPTUREFAB_CAMERA='sim:0')
            result = subprocess.run([sys.executable, '-B', str(SCRIPTS / 'site-reference.py'), '--binary', str(fake), '--snapshot', str(temp / 's.json'), '--page', str(temp / 'r.html')],
                                    capture_output=True, text=True, env=env)
            self.assertEqual(result.returncode, 0, result.stderr)
            recorded = json.loads((temp / 's.json').read_text())
            self.assertEqual(recorded['commands']['list']['examples'], ['capturefab list --kind sim'])
            self.assertEqual(recorded['commands']['list']['aliases'], ['ls'])
            self.assertEqual(recorded['global_options'][0]['long'], '--json')
            check = subprocess.run([sys.executable, '-B', str(SCRIPTS / 'site-reference.py'), '--check', '--binary', str(fake), '--snapshot', str(temp / 's.json'), '--page', str(temp / 'r.html')],
                                   capture_output=True, text=True)
            self.assertEqual(check.returncode, 0, check.stderr)

    def test_repository_reference_is_current(self):
        self.assertEqual(self.run_script('--check').returncode, 0, 'run scripts/site-reference.py')

    def test_every_repository_command_has_a_section(self):
        # "More commands" is a fallback for a command added since the last
        # release of the page, not a home for one.
        recorded = json.loads(reference.SNAPSHOT.read_text())
        placed = {root for *_, roots in reference.GROUPS for root in roots}
        roots = {name.split()[0] for name in recorded['commands']} | set(recorded['groups'])
        self.assertEqual(sorted(roots - placed), [], 'add them to GROUPS in scripts/site-reference.py')
        self.assertNotIn('id="cli-more"', reference.PAGE.read_text())


if __name__ == '__main__':
    unittest.main()
