"""Exercise the shared CI action's real shell steps with isolated build tools."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ACTION = Path(__file__).resolve().parents[1] / '.github/actions/setup-ffmpeg/action.yml'


def shell_step(name):
    step = ACTION.read_text().split(f'- name: {name}\n', 1)[1]
    return step.split('\n      run: ', 1)[1].split('\n', 1)[0]


class FfmpegSetupTests(unittest.TestCase):
    def setUp(self):
        # Hosted Windows paths commonly contain spaces. Use them on every host.
        self.temp = tempfile.TemporaryDirectory(prefix='capturefab ffmpeg ')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / 'scripts').mkdir()
        (self.root / 'tools').mkdir()
        self.env = dict(os.environ, RUNNER_TEMP=str(self.root),
                        GITHUB_ENV=str(self.root / 'github env'),
                        CAPTUREFAB_BUILD_CALL=str(self.root / 'build call'))
        self.env['PATH'] = str(self.root / 'tools') + os.pathsep + self.env['PATH']
        self.tool('scripts/build-ffmpeg.sh',
                  'printf "%s\\n" "$BUILD_DIR" "$1" > "$CAPTUREFAB_BUILD_CALL"\n')
        self.tool('tools/cygpath',
                  '[ "$#" = 2 ] && [ "$1" = -u ] || exit 2\nprintf "%s\\n" "$2"\n')

    def tool(self, name, script):
        path = self.root / name
        path.write_text('#!/usr/bin/env bash\nset -e\n' + script)
        path.chmod(0o755)

    def run_step(self, name):
        return subprocess.run(['bash', '-e', '-c', shell_step(name)],
                              cwd=self.root, env=self.env, capture_output=True, text=True)

    def test_each_platform_build_passes_the_bundle_and_working_paths_intact(self):
        for step in ('Build FFmpeg', 'Build FFmpeg with MinGW'):
            with self.subTest(step=step):
                result = self.run_step(step)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual((self.root / 'build call').read_text().splitlines(), [
                    str(self.root / 'ffmpeg-build'), str(self.root / 'ffmpeg-bundle')])

    def test_source_build_failure_fails_the_step_on_each_platform(self):
        self.tool('scripts/build-ffmpeg.sh', 'exit 42\n')
        for step in ('Build FFmpeg', 'Build FFmpeg with MinGW'):
            with self.subTest(step=step):
                self.assertEqual(self.run_step(step).returncode, 42)

    def test_exports_the_platform_executable_for_following_cargo_steps(self):
        for system, suffix in (('Linux', ''), ('macOS', ''), ('Windows', '.exe')):
            with self.subTest(system=system):
                self.env['RUNNER_OS'] = system
                result = self.run_step('Use the FFmpeg bundle')
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual((self.root / 'github env').read_text().splitlines()[-1],
                                 f'CAPTUREFAB_FFMPEG_BINARY={self.root}/ffmpeg-bundle/bin/ffmpeg{suffix}')


if __name__ == '__main__':
    unittest.main()
