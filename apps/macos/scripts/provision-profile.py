#!/usr/bin/env python3
"""Preflight a Developer ID profile; optionally embed it before app signing."""

import argparse
import datetime
import pathlib
import plistlib
import subprocess
import sys
import tempfile


def entitlements(profile, bundle_id, now):
    """Select only the restricted capabilities the desktop app needs."""
    platforms = profile.get('Platform', [])
    if not isinstance(platforms, list) or 'OSX' not in platforms:
        raise ValueError('profile must support macOS (OSX)')
    if profile.get('ProvisionsAllDevices') is not True or profile.get('ProvisionedDevices'):
        raise ValueError('profile must be for Developer ID distribution')
    expiry = profile.get('ExpirationDate')
    if not isinstance(expiry, datetime.datetime) or expiry.replace(tzinfo=datetime.timezone.utc) <= now:
        raise ValueError('profile is expired or has no expiration date')
    allowed = profile.get('Entitlements', {})
    if allowed.get('get-task-allow') or allowed.get('com.apple.security.get-task-allow'):
        raise ValueError('profile allows debugging')
    app_id = allowed.get('com.apple.application-identifier', '')
    prefixes = profile.get('ApplicationIdentifierPrefix', [])
    if not app_id or '*' in app_id or app_id not in [f'{prefix}.{bundle_id}' for prefix in prefixes]:
        raise ValueError('profile must contain the explicit desktop application identifier')
    team = allowed.get('com.apple.developer.team-identifier')
    if not isinstance(team, str) or not team or profile.get('TeamIdentifier') != [team]:
        raise ValueError('profile team identifiers do not agree')
    if allowed.get('com.apple.developer.declared-age-range') is not True:
        raise ValueError('profile must permit Declared Age Range')
    certificates = profile.get('DeveloperCertificates', [])
    if not certificates or not all(isinstance(cert, bytes) and cert for cert in certificates):
        raise ValueError('profile has no signing certificates')
    return {
        'com.apple.application-identifier': app_id,
        'com.apple.developer.team-identifier': team,
        'com.apple.developer.declared-age-range': True,
    }


def check_signer(profile, team, certificate):
    if team != profile['Entitlements']['com.apple.developer.team-identifier']:
        raise ValueError('signing identity belongs to a different profile team')
    if certificate not in profile['DeveloperCertificates']:
        raise ValueError('signing certificate is not included in the profile')


def signing_certificate(code):
    # Apple's Developer ID requirement, including its Apple trust anchor (TN3127).
    requirement = ('=anchor apple generic and '
                   'certificate 1[field.1.2.840.113635.100.6.2.6] exists and '
                   'certificate leaf[field.1.2.840.113635.100.6.1.13] exists')
    subprocess.run(['/usr/bin/codesign', '--verify', '--strict', '-R', requirement, code], check=True)
    with tempfile.TemporaryDirectory() as temporary:
        prefix = pathlib.Path(temporary) / 'certificate'
        result = subprocess.run(
            ['/usr/bin/codesign', '-d', '--verbose=4', f'--extract-certificates={prefix}', code],
            check=True, capture_output=True, text=True)
        team = next((line.removeprefix('TeamIdentifier=') for line in result.stderr.splitlines()
                     if line.startswith('TeamIdentifier=')), '')
        return team, pathlib.Path(f'{prefix}0').read_bytes()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('profile', type=pathlib.Path)
    parser.add_argument('info', type=pathlib.Path, help='the app Info.plist')
    parser.add_argument('--signed-code', help='nested code signed with the final app identity')
    parser.add_argument('--output', type=pathlib.Path, help='write entitlements and embed the profile')
    args = parser.parse_args()
    if bool(args.signed_code) != bool(args.output):
        parser.error('--signed-code and --output must be used together')
    original = args.profile.read_bytes()
    decoded = subprocess.run(['/usr/bin/security', 'cms', '-D'], input=original,
                             capture_output=True, check=True).stdout
    profile = plistlib.loads(decoded)
    bundle_id = plistlib.loads(args.info.read_bytes())['CFBundleIdentifier']
    selected = entitlements(profile, bundle_id, datetime.datetime.now(datetime.timezone.utc))
    if args.signed_code:
        check_signer(profile, *signing_certificate(args.signed_code))
        base = pathlib.Path(__file__).resolve().parent.parent / 'platform/Mobius.entitlements'
        selected = plistlib.loads(base.read_bytes()) | selected
        args.output.write_bytes(plistlib.dumps(selected))
        (args.info.parent / 'embedded.provisionprofile').write_bytes(original)
    print('Provisioning profile validated' + (' and embedded' if args.signed_code else ' (signer checked during bundling)'))


if __name__ == '__main__':
    try:
        main()
    except (ValueError, OSError, KeyError, TypeError, subprocess.CalledProcessError) as error:
        sys.exit(f'Provisioning profile rejected: {error}')
