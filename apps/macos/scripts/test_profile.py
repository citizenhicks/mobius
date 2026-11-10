import copy
import datetime
import importlib.util
import os
import pathlib
import plistlib
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPTS = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('provision_profile', SCRIPTS / 'provision-profile.py')
provision = importlib.util.module_from_spec(spec)
spec.loader.exec_module(provision)
NOW = datetime.datetime(2026, 9, 28, tzinfo=datetime.timezone.utc)
BUNDLE_ID = 'app.mobius.desktop'
# Legacy App ID prefixes need not equal the signing team.
PROFILE = {
    'Platform': ['OSX'], 'ProvisionsAllDevices': True,
    'ExpirationDate': datetime.datetime(2027, 1, 1),
    'ApplicationIdentifierPrefix': ['LEGACY1234'], 'TeamIdentifier': ['TEAM123456'],
    'DeveloperCertificates': [b'public-leaf-certificate'],
    'Entitlements': {
        'com.apple.application-identifier': f'LEGACY1234.{BUNDLE_ID}',
        'com.apple.developer.team-identifier': 'TEAM123456',
        'com.apple.developer.declared-age-range': True,
        'keychain-access-groups': ['LEGACY1234.*'],
    },
}


class ProfileTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform == 'darwin', 'requires macOS codesign')
    def test_requirement_is_valid_but_rejects_apple_system_code(self):
        with self.assertRaises(subprocess.CalledProcessError) as raised:
            provision.signing_certificate('/usr/bin/codesign')
        # Exit 3 means a valid signature failed our requirement; 1/2 can mask CLI errors.
        self.assertEqual(raised.exception.returncode, 3)

    def test_selects_only_required_capabilities_and_matching_signer(self):
        selected = provision.entitlements(PROFILE, BUNDLE_ID, NOW)
        self.assertEqual(len(selected), 3)
        self.assertNotIn('com.apple.developer.applesignin', selected)
        self.assertNotIn('keychain-access-groups', selected)
        provision.check_signer(PROFILE, 'TEAM123456', b'public-leaf-certificate')
        for team, certificate in [('OTHER12345', b'public-leaf-certificate'),
                                  ('TEAM123456', b'different-certificate')]:
            with self.subTest(team=team), self.assertRaises(ValueError):
                provision.check_signer(PROFILE, team, certificate)

    def test_rejects_wrong_or_missing_profile_permissions(self):
        for key, value in [('Platform', ['iOS']), ('Platform', 'OSX'),
                           ('ProvisionsAllDevices', False), ('ProvisionedDevices', ['device']),
                           ('ExpirationDate', NOW.replace(tzinfo=None)), ('ExpirationDate', None),
                           ('TeamIdentifier', ['OTHER12345']), ('DeveloperCertificates', [])]:
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                provision.entitlements(PROFILE | {key: value}, BUNDLE_ID, NOW)
        for key, value in [('com.apple.application-identifier', 'LEGACY1234.*'),
                           ('com.apple.application-identifier', 'LEGACY1234.app.other'),
                           ('com.apple.developer.team-identifier', 'OTHER12345'),
                           ('com.apple.developer.declared-age-range', False),
                           ('get-task-allow', True), ('com.apple.security.get-task-allow', True)]:
            profile = copy.deepcopy(PROFILE)
            profile['Entitlements'][key] = value
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                provision.entitlements(profile, BUNDLE_ID, NOW)

    def test_embeds_original_cms_and_merges_only_required_entitlements(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = pathlib.Path(temporary)
            profile, info, output = [directory / name for name in ['profile', 'Info.plist', 'entitlements']]
            profile.write_bytes(b'original-CMS-profile')
            info.write_bytes(plistlib.dumps({'CFBundleIdentifier': BUNDLE_ID}))
            fixture = copy.deepcopy(PROFILE)
            fixture['ExpirationDate'] = datetime.datetime(2999, 1, 1)
            decoded = subprocess.CompletedProcess([], 0, stdout=plistlib.dumps(fixture))
            args = ['provision-profile.py', str(profile), str(info), '--signed-code', 'native.dylib',
                    '--output', str(output)]
            with patch('sys.argv', args), patch.object(provision.subprocess, 'run', return_value=decoded), \
                    patch.object(provision, 'signing_certificate', return_value=('TEAM123456', b'public-leaf-certificate')):
                provision.main()
            self.assertEqual((directory / 'embedded.provisionprofile').read_bytes(), profile.read_bytes())
            result = plistlib.loads(output.read_bytes())
            self.assertEqual(len(result), 4)
            self.assertNotIn('com.apple.developer.applesignin', result)
            self.assertTrue(result['com.apple.security.device.audio-input'])
            self.assertNotIn('keychain-access-groups', result)

    def test_shell_preflights_fail_before_building(self):
        environment = dict(os.environ, MOBIUS_SIGNING_IDENTITY='Developer ID Application: test')
        environment.pop('MOBIUS_PROVISIONING_PROFILE', None)
        result = subprocess.run(['/bin/bash', str(SCRIPTS / 'release.sh')], env=environment,
                                capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Set MOBIUS_PROVISIONING_PROFILE', result.stderr)
        environment.update(MOBIUS_SIGNING_IDENTITY='-', MOBIUS_PROVISIONING_PROFILE='unused-profile')
        result = subprocess.run(['/bin/bash', str(SCRIPTS / 'bundle.sh')], env=environment,
                                capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('not ad-hoc signing', result.stderr)


if __name__ == '__main__':
    unittest.main()
