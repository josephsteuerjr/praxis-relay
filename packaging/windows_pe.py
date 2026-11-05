"""Read normal and delay-load DLL imports without an installed PE toolkit."""
from pathlib import Path
import struct


def imports(path: Path) -> list[str]:
    data = path.read_bytes()
    pe = struct.unpack_from('<I', data, 0x3c)[0]
    if data[:2] != b'MZ' or data[pe:pe + 4] != b'PE\0\0':
        raise ValueError(f'Not a PE executable: {path}')
    sections = struct.unpack_from('<H', data, pe + 6)[0]
    optional_size = struct.unpack_from('<H', data, pe + 20)[0]
    optional = pe + 24
    magic = struct.unpack_from('<H', data, optional)[0]
    if magic not in (0x10b, 0x20b):
        raise ValueError(f'Unsupported PE optional header: {path}')
    directories = optional + (112 if magic == 0x20b else 96)
    directory_count = struct.unpack_from('<I', data, directories - 4)[0]
    image_base = struct.unpack_from('<Q' if magic == 0x20b else '<I', data,
                                    optional + (24 if magic == 0x20b else 28))[0]
    headers_size = struct.unpack_from('<I', data, optional + 60)[0]
    ranges = []
    for index in range(sections):
        section = optional + optional_size + index * 40
        virtual_size, address, raw_size, raw = struct.unpack_from('<IIII', data, section + 8)
        ranges.append((address, max(virtual_size, raw_size), raw))

    def offset(address):
        if address < headers_size:
            return address
        for start, size, raw in ranges:
            if start <= address < start + size:
                return raw + address - start
        raise ValueError(f'PE import address outside sections: {path}')

    names = set()
    for directory, width, name_field in ((1, 20, 12), (13, 32, 4)):
        if directory >= directory_count:
            continue
        address, size = struct.unpack_from('<II', data, directories + directory * 8)
        if not address:
            continue
        if size < width:
            raise ValueError(f'Truncated PE import table: {path}')
        start = offset(address)
        for entry in range(start, start + size, width):
            descriptor = data[entry:entry + width]
            if len(descriptor) != width:
                raise ValueError(f'Truncated PE import table: {path}')
            if not any(descriptor):
                break
            name = struct.unpack_from('<I', descriptor, name_field)[0]
            if directory == 13 and not (struct.unpack_from('<I', descriptor)[0] & 1):
                name -= image_base
            position = offset(name)
            end = data.find(b'\0', position, position + 256)
            if end < 0:
                raise ValueError(f'Invalid PE import name: {path}')
            names.add(data[position:end].decode('ascii').lower())
    return sorted(names)


def require_portable_runtime(path: Path) -> list[str]:
    names = imports(path)
    dynamic = [name for name in names
               if name.startswith(('vcruntime', 'msvcp', 'msvcr1', 'concrt'))]
    if dynamic:
        raise ValueError(f'Windows release requires installer-free VC runtime linkage: {path.name}: {dynamic}')
    return names
