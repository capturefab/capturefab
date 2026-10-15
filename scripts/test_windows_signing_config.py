"""Exercise the release workflow's signing preflight without Azure access."""
import os
from pathlib import Path
import subprocess
import textwrap
import unittest


WORKFLOW = Path(__file__).resolve().parents[1] / '.github/workflows/release.yml'
REQUIRED = {
    'AZURE_CLIENT_ID': 'client-id',
    'AZURE_TENANT_ID': 'tenant-id',
    'AZURE_SUBSCRIPTION_ID': 'subscription-id',
    'AZURE_ARTIFACT_SIGNING_ENDPOINT': 'https://eus.codesigning.azure.net/',
    'AZURE_ARTIFACT_SIGNING_ACCOUNT_NAME': 'signing-account',
    'AZURE_ARTIFACT_SIGNING_CERTIFICATE_PROFILE': 'public-trust',
}


class WindowsSigningConfigTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        # Run the actual workflow shell rather than a copy of its policy.
        step = WORKFLOW.read_text().split('      - name: Check Windows signing configuration\n', 1)[1]
        cls.script = textwrap.dedent(step.split('        run: |\n', 1)[1].split('\n  candidate:', 1)[0])

    def check(self, provider='azure', release='true', config=None):
        env = {key: value for key, value in os.environ.items() if key not in REQUIRED}
        env.update(PROVIDER=provider, RELEASE=release)
        env.update(REQUIRED if config is None else config)
        return subprocess.run(['bash', '-c', self.script], env=env, capture_output=True, text=True)

    def test_configured_azure_release_passes(self):
        self.assertEqual(self.check().returncode, 0)

    def test_each_missing_azure_variable_blocks_release(self):
        for name in REQUIRED:
            with self.subTest(name=name):
                config = dict(REQUIRED)
                del config[name]
                result = self.check(config=config)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(name, result.stdout)

    def test_azure_requires_https(self):
        config = dict(REQUIRED, AZURE_ARTIFACT_SIGNING_ENDPOINT='http://eus.codesigning.azure.net/')
        result = self.check(config=config)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('HTTPS', result.stdout)

    def test_branch_dry_run_needs_no_azure_configuration(self):
        self.assertEqual(self.check(release='false', config={}).returncode, 0)

    def test_unset_provider_preserves_legacy_path(self):
        self.assertEqual(self.check(provider='', config={}).returncode, 0)

    def test_unknown_provider_cannot_silently_disable_signing(self):
        self.assertNotEqual(self.check(provider='azuer', config={}).returncode, 0)


if __name__ == '__main__':
    unittest.main()
