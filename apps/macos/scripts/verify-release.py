#!/usr/bin/env python3
"""Verify a desktop release's appcast and archive before channel promotion."""
import hashlib
import pathlib
import sys
import xml.etree.ElementTree as ET

SPARKLE = '{http://www.andymatuschak.org/xml-namespaces/sparkle}'


def verify(directory, version, build_number):
    if len(version.split('.')) != 3 or not all(part.isdecimal() for part in version.split('.')):
        raise ValueError('Expected a numeric X.Y.Z version')
    archive = directory / f'mobius-desktop-{version}.zip'
    expected = (directory / f'{archive.name}.sha256').read_text().split()
    checksum = hashlib.sha256()
    with archive.open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            checksum.update(block)
    digest = checksum.hexdigest()
    if expected != [digest, archive.name]:
        raise ValueError('Archive checksum or filename differs')
    item = ET.parse(directory / 'appcast.xml').find('channel/item')
    if item is None or item.findtext(f'{SPARKLE}shortVersionString') != version:
        raise ValueError('Appcast does not promote the requested version')
    if item.findtext(f'{SPARKLE}version') != str(build_number):
        raise ValueError('Appcast bundle build number differs')
    enclosure = item.find('enclosure')
    url = f'https://github.com/citizenhicks/mobius/releases/download/mobius-desktop-v{version}/{archive.name}'
    if enclosure is None or enclosure.get('url') != url:
        raise ValueError('Appcast archive URL differs')
    if enclosure.get('length') != str(archive.stat().st_size):
        raise ValueError('Appcast archive size differs')
    signature = enclosure.get(f'{SPARKLE}edSignature')
    if not signature:
        raise ValueError('Appcast has no signature')
    return signature


if __name__ == '__main__':
    import tomllib
    with (pathlib.Path(__file__).resolve().parents[1] / 'Cargo.toml').open('rb') as source:
        package = tomllib.load(source)['package']
    if package['version'] != sys.argv[2]:
        raise ValueError('Checked-out release source version differs')
    print(verify(pathlib.Path(sys.argv[1]), sys.argv[2], package['metadata']['bundle']['build-number']))
