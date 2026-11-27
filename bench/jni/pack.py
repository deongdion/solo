"""XORs the pk_code and pk_data sections of an ELF file in place (see packed.c)."""
import struct
import sys

KEY = 0x5A


def sections(data):
    is64 = data[4] == 2
    if is64:
        shoff, = struct.unpack_from('<Q', data, 0x28)
        shentsize, shnum, shstrndx = struct.unpack_from('<HHH', data, 0x3A)
    else:
        shoff, = struct.unpack_from('<I', data, 0x20)
        shentsize, shnum, shstrndx = struct.unpack_from('<HHH', data, 0x2E)
    entries = []
    for i in range(shnum):
        at = shoff + i * shentsize
        if is64:
            name, kind, _, _, offset, size = struct.unpack_from('<IIQQQQ', data, at)
        else:
            name, kind, _, _, offset, size = struct.unpack_from('<IIIIII', data, at)
        entries.append((name, kind, offset, size))
    strings = entries[shstrndx][2]
    for name, kind, offset, size in entries:
        end = data.index(b'\0', strings + name)
        yield data[strings + name:end].decode(), kind, offset, size


def main(path):
    data = bytearray(open(path, 'rb').read())
    done = set()
    for name, kind, offset, size in sections(data):
        if name in ('pk_code', 'pk_data') and kind != 8:  # not SHT_NOBITS
            for i in range(offset, offset + size):
                data[i] ^= KEY
            done.add(name)
    if done != {'pk_code', 'pk_data'}:
        sys.exit(f'{path}: sections missing, found {sorted(done)}')
    open(path, 'wb').write(data)


if __name__ == '__main__':
    main(sys.argv[1])
