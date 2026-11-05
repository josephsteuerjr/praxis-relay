"""The portable Windows package rejects normal and delayed dynamic CRT imports."""
from pathlib import Path
import importlib.util
import struct
import tempfile
import unittest

MODULE = Path(__file__).resolve().parents[1] / 'packaging/windows_pe.py'
spec = importlib.util.spec_from_file_location('relay_windows_pe', MODULE)
pe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pe)


def executable(normal='kernel32.dll', delayed=None, bits=64):
    data = bytearray(2048)
    data[:2] = b'MZ'
    struct.pack_into('<I', data, 0x3c, 0x80)
    data[0x80:0x84] = b'PE\0\0'
    optional_size = 240 if bits == 64 else 224
    struct.pack_into('<HH', data, 0x84, 0x8664 if bits == 64 else 0x14c, 1)
    struct.pack_into('<H', data, 0x94, optional_size)
    optional = 0x98
    struct.pack_into('<H', data, optional, 0x20b if bits == 64 else 0x10b)
    struct.pack_into('<I', data, optional + 60, 0x200)
    directories = optional + (112 if bits == 64 else 96)
    struct.pack_into('<I', data, directories - 4, 16)
    struct.pack_into('<IIII', data, optional + optional_size + 8, 0x600, 0x1000, 0x600, 0x200)
    struct.pack_into('<II', data, directories + 8, 0x1000, 40)
    struct.pack_into('<I', data, 0x200 + 12, 0x1100)
    data[0x300:0x300 + len(normal) + 1] = normal.encode() + b'\0'
    if delayed:
        struct.pack_into('<II', data, directories + 13 * 8, 0x1200, 64)
        struct.pack_into('<II', data, 0x400, 1, 0x1300)
        data[0x500:0x500 + len(delayed) + 1] = delayed.encode() + b'\0'
    return data


class WindowsImports(unittest.TestCase):
    def read(self, data, check=pe.require_portable_runtime):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / 'fixture.exe'
            path.write_bytes(data)
            return check(path)

    def test_system_only_imports_are_portable_for_both_pe_formats(self):
        for bits in (32, 64):
            with self.subTest(bits=bits):
                self.assertEqual(self.read(executable(bits=bits)), ['kernel32.dll'])

    def test_windows_system_ucrt_is_allowed(self):
        self.assertEqual(self.read(executable(normal='api-ms-win-crt-runtime-l1-1-0.dll')),
                         ['api-ms-win-crt-runtime-l1-1-0.dll'])

    def test_dynamic_normal_crt_is_refused(self):
        with self.assertRaisesRegex(ValueError, 'installer-free VC runtime'):
            self.read(executable(normal='VCRUNTIME140.dll'))

    def test_delay_loaded_crt_is_refused(self):
        with self.assertRaisesRegex(ValueError, 'installer-free VC runtime'):
            self.read(executable(delayed='MSVCP140.dll'))

    def test_system_delay_import_is_recorded(self):
        self.assertEqual(self.read(executable(delayed='USER32.dll')), ['kernel32.dll', 'user32.dll'])

    def test_invalid_import_address_is_refused(self):
        data = executable()
        struct.pack_into('<I', data, 0x200 + 12, 0xfffff)
        with self.assertRaisesRegex(ValueError, 'outside sections'):
            self.read(data)

    def test_nonzero_import_address_with_zero_table_size_is_refused(self):
        data = executable(normal='VCRUNTIME140.dll')
        struct.pack_into('<I', data, 0x98 + 112 + 12, 0)
        with self.assertRaisesRegex(ValueError, 'Truncated PE import table'):
            self.read(data)


if __name__ == '__main__':
    unittest.main()
