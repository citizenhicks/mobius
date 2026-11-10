#!/usr/bin/env python3
"""Exercise the native ABI and the real desktop provider's fail-closed boundary."""
import ctypes
import json
import os
import pathlib
import platform
import re
import subprocess
import tempfile

root = pathlib.Path(__file__).resolve().parents[1]
shared_writers = root / 'platform/OnDeviceWriters.swift'
# The private iOS app keeps a byte-identical copy; checked when it sits beside this checkout.
ios_writers = root.parents[2] / 'mobius-ios.nosync/apple/Sources/MobiusApp/Chat/OnDeviceWriters.swift'
if ios_writers.exists():
    assert shared_writers.read_bytes() == ios_writers.read_bytes(), (
        f'{shared_writers} and {ios_writers} must stay byte-identical')
gateway_wire = (root / 'platform/desktop/GatewayWire.swift').read_text()
protocol = int(re.search(r'let gatewayProtocolVersion = (\d+)', gateway_wire).group(1))
with tempfile.TemporaryDirectory(prefix='mobius-native-check-') as temporary:
    output = pathlib.Path(temporary)
    subprocess.run([str(root / 'scripts/build-native.sh'), str(output)], check=True)
    native = ctypes.CDLL(str(output / 'libMobiusNative.dylib'))
    native.mobius_native_poll.restype = ctypes.c_void_p
    native.mobius_native_free.argtypes = [ctypes.c_void_p]
    native.mobius_native_connected.argtypes = [ctypes.c_bool]
    native.mobius_native_desktop.argtypes = [ctypes.c_bool, ctypes.c_bool]
    native.mobius_native_gateway.argtypes = [ctypes.c_char_p]
    native.mobius_native_desktop_status.restype = ctypes.c_uint8
    native.mobius_native_voice_mute.argtypes = [ctypes.c_bool]
    wake_calls = []
    wake_callback_type = ctypes.CFUNCTYPE(None)
    wake_callback = wake_callback_type(lambda: wake_calls.append(True))
    native.mobius_native_set_wake.argtypes = [wake_callback_type]
    native.mobius_native_set_wake(wake_callback)

    def events():
        pointer = native.mobius_native_poll()
        assert pointer, 'Native event queue returned no allocation'
        try:
            return json.loads(ctypes.string_at(pointer))
        finally:
            native.mobius_native_free(pointer)

    assert events() == [{'type': 'voice_state', 'active': False, 'connected': False, 'muted': False}]
    before_wakes = len(wake_calls)
    native.mobius_native_voice_mute(True)
    assert len(wake_calls) > before_wakes, 'Voice mute changes must wake Rust without polling'
    assert events() == [{'type': 'voice_state', 'active': False, 'connected': False, 'muted': True}]
    native.mobius_native_voice_mute(False)
    assert events() == [{'type': 'voice_state', 'active': False, 'connected': False, 'muted': False}]
    native.mobius_native_connected(True)
    native.mobius_native_desktop(False, False)
    assert not native.mobius_native_desktop_status() & 1
    before_wakes = len(wake_calls)
    native.mobius_native_gateway(b'{"version":0,"type":"accepted"}')
    assert len(wake_calls) > before_wakes, 'Queued native events must wake Rust without polling'
    assert events()[0]['type'] == 'error', 'Mismatched protocol must be rejected'

    # A desktop request while disabled must report an error through the unchanged wire.
    native.mobius_native_gateway(json.dumps({
        'version': protocol, 'type': 'desktop_control_requested',
        'execution_id': 'native-check', 'session_id': 'native-check', 'request_id': 'native-check',
        'request': {'action': 'apps'},
    }).encode())
    core = ctypes.CDLL('/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation')
    core.CFRunLoopRunInMode.argtypes = [ctypes.c_void_p, ctypes.c_double, ctypes.c_bool]
    default_mode = ctypes.c_void_p.in_dll(core, 'kCFRunLoopDefaultMode')
    core.CFRunLoopRunInMode(default_mode, 0.2, False)
    reply = events()
    assert len(reply) == 1, reply
    request = reply[0]['request']
    assert request['type'] == 'desktop_control_reply', reply
    assert request['request_id'] == 'native-check', reply
    assert 'error' in request['response'], 'Disabled computer control must never execute'
    native.mobius_native_connected(False)

    # A headless library has no presentation window and must not open a sign-in browser.
    native.mobius_native_cloud.argtypes = [ctypes.c_uint8]
    native.mobius_native_cloud(1)
    cloud = [event for event in events() if event['type'] == 'cloud_status']
    assert cloud and cloud[-1]['status']['error_kind'] == 'other', cloud
    assert cloud[-1]['status']['error'] == 'Open the möbius app window to sign in with Apple.', cloud
    assert not cloud[-1]['status']['signed_in'], cloud

    # An unbundled library never starts Sparkle and says why updates are unavailable.
    native.mobius_native_update_status.restype = ctypes.c_void_p
    native.mobius_native_update.argtypes = [ctypes.c_uint8]
    pointer = native.mobius_native_update_status()
    try:
        update = json.loads(ctypes.string_at(pointer))
    finally:
        native.mobius_native_free(pointer)
    assert not update['supported'] and update['stage'] == 'idle', update
    assert 'development builds' in update['reason'], update
    native.mobius_native_update(0)
    native.mobius_native_update(1)
    assert events() == [], 'Unavailable updates must not start a check or an install'
    print('Native ABI, protocol validation, disabled desktop control, headless Cloud sign-in and updates passed.')

    metadata = json.loads(subprocess.check_output([
        'cargo', 'metadata', '--locked', '--format-version', '1',
    ], cwd=root))
    cef_version = next(package['version'].split('+')[1] for package in metadata['packages']
                       if package['name'] == 'cef-dll-sys')
    architecture = 'aarch64' if platform.machine() == 'arm64' else platform.machine()
    cef = (pathlib.Path(os.environ.get('CEF_PATH', root / 'target/cef')) / cef_version
           / f'cef_macos_{architecture}' / 'Chromium Embedded Framework.framework'
           / 'Chromium Embedded Framework')
    assert cef.is_file(), f'Build the pinned Chromium SDK first: {cef}'
    # A scratch directory outside Documents avoids FileProvider metadata on signed test bundles.
    scratch = subprocess.check_output([str(root / 'scripts/swiftpm-cache.sh')], text=True).strip()
    subprocess.run([
        'swift', 'test', '--package-path', str(root / 'platform'),
        '--scratch-path', scratch, '--jobs', os.environ.get('NUM_JOBS', '2'),
        '-Xswiftc', '-warnings-as-errors',
    ], check=True, env={**os.environ, 'MOBIUS_CEF_FRAMEWORK': str(cef)})
