import hashlib
import importlib.util
import pathlib
import tempfile
import unittest
import argparse
import appcast

spec = importlib.util.spec_from_file_location('verify_release', pathlib.Path(__file__).with_name('verify-release.py'))
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class ReleaseTests(unittest.TestCase):
    def test_appcast_separates_the_visible_version_from_update_ordering(self):
        args = argparse.Namespace(version='0.3.14', build_number=1, date=None,
                                  minimum_system='26.0', notes_url='https://example.com/notes',
                                  url='https://example.com/update.zip', length=15, signature='test-signature')
        item = appcast.item(args)
        self.assertEqual(item.findtext(f'{release.SPARKLE}version'), '1')
        self.assertEqual(item.findtext(f'{release.SPARKLE}shortVersionString'), '0.3.14')

    def test_promotes_only_the_matching_archive(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = pathlib.Path(temporary)
            archive = directory / 'mobius-desktop-0.2.0.zip'
            archive.write_bytes(b'release archive')
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()
            (directory / f'{archive.name}.sha256').write_text(f'{digest}  {archive.name}\n')
            (directory / 'appcast.xml').write_text('''<rss xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle"><channel><item>
                <sparkle:version>1</sparkle:version><sparkle:shortVersionString>0.2.0</sparkle:shortVersionString>
                <enclosure url="https://github.com/citizenhicks/mobius/releases/download/mobius-desktop-v0.2.0/mobius-desktop-0.2.0.zip" length="15" sparkle:edSignature="test-signature"/>
                </item></channel></rss>''')
            self.assertEqual(release.verify(directory, '0.2.0', 1), 'test-signature')
            with self.assertRaisesRegex(ValueError, 'build number'):
                release.verify(directory, '0.2.0', 2)
            appcast = directory / 'appcast.xml'
            original = appcast.read_text()
            appcast.write_text(original.replace('shortVersionString>0.2.0', 'shortVersionString>0.2.1'))
            with self.assertRaisesRegex(ValueError, 'requested version'):
                release.verify(directory, '0.2.0', 1)
            appcast.write_text(original)
            archive.write_bytes(b'altered archive')
            with self.assertRaisesRegex(ValueError, 'checksum'):
                release.verify(directory, '0.2.0', 1)


if __name__ == '__main__':
    unittest.main()
