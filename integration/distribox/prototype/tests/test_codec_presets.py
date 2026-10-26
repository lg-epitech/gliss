"""Pure trusted-preset/relay admission checks; no native process or listener."""
from pathlib import Path
import struct
import unittest

from integration.distribox.adapter import AccessDenied
from integration.distribox.prototype.gateway import Config, validate_native_hello, validate_packet, validate_command
from integration.distribox.prototype.workers import worker_codec_args


def hello(codec='hybrid-continuity'):
    value = {'type': 'hello', 'codec': codec, 'protocol': 'GLH4' if codec == 'hybrid-continuity' else 'GLS1',
             'source': 'rfb', 'fps_limit': 30}
    if codec == 'hybrid-continuity':
        value['hybrid_config'] = {'base_mode': 'global-cached', 'video_profile': 'camera-scene-off',
                                 'video_usage': 'camera-video-real-time', 'scene_change_detection': False,
                                 'mask_grid': 32, 'base_tile': 64}
    return value


class CodecPresetTests(unittest.TestCase):
    def test_default_and_only_opt_in_native_flags_are_fixed(self):
        self.assertEqual(worker_codec_args('deflate'), ['--codec', 'deflate'])
        self.assertEqual(worker_codec_args('hybrid-continuity'), ['--codec', 'hybrid-continuity',
            '--hybrid-base', 'global-cached', '--hybrid-video-profile', 'camera-scene-off',
            '--hybrid-mask-grid', '32', '--tile-size', '64', '--h264-bitrate', '3000000'])
        for value in ['h264', 'hybrid-grid', '--vnc', {}, None]:
            with self.subTest(value=value), self.assertRaises(ValueError): worker_codec_args(value)
        self.assertEqual(Config(Path('/bin/true'), 'http://127.0.0.1:8080').worker_codec, 'deflate')
        with self.assertRaises(ValueError):
            Config(Path('/bin/true'), 'http://127.0.0.1:8080', worker_codec='h264').validate()

    def test_actual_hello_requires_exact_selected_codec_profile_and_types(self):
        validate_native_hello(hello(), 'hybrid-continuity')
        validate_native_hello(hello('deflate'), 'deflate')
        for key, value in [('source', 'demo'), ('protocol', 'GLH2'), ('fps_limit', 60), ('codec', 'hybrid')]:
            changed = hello(); changed[key] = value
            with self.subTest(key=key), self.assertRaises(AccessDenied): validate_native_hello(changed, 'hybrid-continuity')
        for key, value in [('mask_grid', 16), ('scene_change_detection', 0), ('base_mode', 'global')]:
            changed = hello(); changed['hybrid_config'][key] = value
            with self.subTest(key=key), self.assertRaises(AccessDenied): validate_native_hello(changed, 'hybrid-continuity')
        with self.assertRaises(AccessDenied): validate_native_hello(hello(), 'deflate')

    def test_wrapper_codec_grid_size_and_reserved_bounds_preserve_default_api(self):
        packet = bytearray(88); packet[:4] = b'GLH4'
        struct.pack_into('<HHHH', packet, 12, 512, 288, 32, 1)
        struct.pack_into('<I', packet, 32, 24)
        validate_packet(packet, 'hybrid-continuity')
        with self.assertRaises(AccessDenied): validate_packet(packet)
        for offset, value in [(16, 16), (18, 8), (22, 1), (48, 1)]:
            changed = bytearray(packet); struct.pack_into('<H', changed, offset, value)
            with self.subTest(offset=offset), self.assertRaises(AccessDenied): validate_packet(changed, 'hybrid-continuity')
        with self.assertRaises(AccessDenied): validate_packet(packet[:-1], 'hybrid-continuity')
        legacy = bytearray(24); legacy[:4] = b'GLS1'; struct.pack_into('<HH', legacy, 12, 96, 64)
        validate_packet(legacy)
        with self.assertRaises(AccessDenied): validate_packet(legacy, 'hybrid-continuity')

    def test_browser_controls_cannot_select_or_reconfigure_worker_codec(self):
        capabilities = frozenset({'display','input'})
        for value in ['{"type":"codec","codec":"hybrid-continuity"}',
                      '{"type":"resync","codec":"deflate"}',
                      '{"type":"key","keysym":97,"down":true,"hybrid_mask_grid":16}']:
            with self.assertRaises(AccessDenied): validate_command(value, capabilities)


if __name__ == '__main__':
    unittest.main()
