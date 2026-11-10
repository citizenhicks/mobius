#!/usr/bin/env python3
"""Adds one release to a Sparkle appcast, newest first, replacing any item of the same version."""
import argparse
import email.utils
import pathlib
import xml.etree.ElementTree as ET

SPARKLE = 'http://www.andymatuschak.org/xml-namespaces/sparkle'
KEEP = 20
ET.register_namespace('sparkle', SPARKLE)


def tag(name):
    return f'{{{SPARKLE}}}{name}'


def empty_feed(title, link):
    rss = ET.Element('rss', {'version': '2.0'})
    channel = ET.SubElement(rss, 'channel')
    ET.SubElement(channel, 'title').text = title
    ET.SubElement(channel, 'link').text = link
    ET.SubElement(channel, 'language').text = 'en'
    return ET.ElementTree(rss)


def item(args):
    element = ET.Element('item')
    ET.SubElement(element, 'title').text = f'Version {args.version}'
    ET.SubElement(element, 'pubDate').text = args.date or email.utils.formatdate(usegmt=True)
    ET.SubElement(element, tag('version')).text = str(args.build_number)
    ET.SubElement(element, tag('shortVersionString')).text = args.version
    ET.SubElement(element, tag('minimumSystemVersion')).text = args.minimum_system
    ET.SubElement(element, tag('fullReleaseNotesLink')).text = args.notes_url
    ET.SubElement(element, 'link').text = args.notes_url
    ET.SubElement(element, 'enclosure', {
        'url': args.url,
        'length': str(args.length),
        'type': 'application/octet-stream',
        tag('edSignature'): args.signature,
    })
    return element


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--existing', type=pathlib.Path)
    parser.add_argument('--output', type=pathlib.Path, required=True)
    parser.add_argument('--version', required=True)
    parser.add_argument('--build-number', required=True, type=int)
    parser.add_argument('--url', required=True)
    parser.add_argument('--length', type=int, required=True)
    parser.add_argument('--signature', required=True)
    parser.add_argument('--notes-url', required=True)
    parser.add_argument('--minimum-system', required=True)
    parser.add_argument('--date', help='RFC 2822 date; defaults to now')
    args = parser.parse_args()
    if not 1 <= args.build_number <= 9999:
        parser.error('build number must be an integer from 1 to 9999')
    if args.existing and args.existing.is_file() and args.existing.stat().st_size:
        tree = ET.parse(args.existing)
    else:
        tree = empty_feed('möbius for macOS', args.notes_url.rsplit('/tag/', 1)[0])
    channel = tree.getroot().find('channel')
    items = [each for each in channel.findall('item')
             if each.findtext(tag('version')) != str(args.build_number)
             and each.findtext(tag('shortVersionString')) != args.version]
    for each in channel.findall('item'):
        channel.remove(each)
    for each in [item(args), *items][:KEEP]:
        channel.append(each)
    ET.indent(tree)
    tree.write(args.output, encoding='utf-8', xml_declaration=True)


if __name__ == '__main__':
    main()
