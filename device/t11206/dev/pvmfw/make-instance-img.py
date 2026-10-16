#!/usr/bin/env python3
"""Makes an empty instance disk for the vendor pvmfw: a raw GPT disk with one partition named
"vm-instance" whose first block holds the instance header the vendor pvmfw checks
("x-kernel-VM-instance" + version 1, pvmfw/src/instance.rs). pvmfw finds it by scanning the
virtio block devices for that GPT name (find_instance_img), writes its record after the header
on the first boot and only verifies it on later boots.

AVF and the vendor pvm-manage build the same layout as a crosvm composite disk; a plain raw file
is enough for crosvm's --block, and pvmfw only reads LBA 1 (header), LBA 2.. (entries) and the
partition (pvmfw/src/gpt.rs). The GPT is still complete (CRCs, protective MBR, backup copy) so
ordinary tools accept it.

    make-instance-img.py OUT [PARTITION_MIB]      (default 10 MiB, as pvm-manage)
"""
import struct
import sys
import uuid
import zlib

LBA = 512
ENTRIES = 128
ENTRY_SIZE = 128
ENTRY_LBAS = ENTRIES * ENTRY_SIZE // LBA  # 32
FIRST = 2048                               # 1 MiB aligned, as sgdisk does
MAGIC = b"x-kernel-VM-instance"
# Linux filesystem data; pvmfw ignores the type, it matches the name only.
TYPE_GUID = uuid.UUID("0fc63daf-8483-4772-8e79-3d69d8477de4")
# Fixed GUIDs keep the output reproducible.
DISK_GUID = uuid.UUID("5a3c1f62-0b2e-4a77-9d1e-6b1c7f2d9e01")
PART_GUID = uuid.UUID("5a3c1f62-0b2e-4a77-9d1e-6b1c7f2d9e02")


def header(current, backup, first_usable, last_usable, entries_lba, entries_crc):
    fields = [b"EFI PART", 0x00010000, 92, 0, 0, current, backup, first_usable, last_usable,
              DISK_GUID.bytes_le, entries_lba, ENTRIES, ENTRY_SIZE, entries_crc]
    fmt = "<8sIIIIQQQQ16sQIII"
    raw = struct.pack(fmt, *fields)
    fields[3] = zlib.crc32(raw) & 0xFFFFFFFF
    return struct.pack(fmt, *fields).ljust(LBA, b"\0")


def main():
    out = sys.argv[1]
    part_mib = int(sys.argv[2]) if len(sys.argv) > 2 else 10
    part_lbas = part_mib * 1024 * 1024 // LBA
    last = FIRST + part_lbas - 1
    total = last + 1 + ENTRY_LBAS + 1  # partition, backup entries, backup header
    total = (total + 2047) // 2048 * 2048

    name = "vm-instance".encode("utf-16-le").ljust(72, b"\0")
    entry = struct.pack("<16s16sQQQ72s", TYPE_GUID.bytes_le, PART_GUID.bytes_le, FIRST, last, 0, name)
    entries = entry.ljust(ENTRIES * ENTRY_SIZE, b"\0")
    ecrc = zlib.crc32(entries) & 0xFFFFFFFF
    first_usable, last_usable = 2 + ENTRY_LBAS, total - 2 - ENTRY_LBAS

    img = bytearray(total * LBA)
    # Protective MBR: one 0xEE partition covering the disk.
    mbr_part = struct.pack("<B3sB3sII", 0, b"\x00\x02\x00", 0xEE, b"\xff\xff\xff", 1,
                           min(total - 1, 0xFFFFFFFF))
    img[446:462] = mbr_part
    img[510:512] = b"\x55\xaa"
    img[LBA:2 * LBA] = header(1, total - 1, first_usable, last_usable, 2, ecrc)
    img[2 * LBA:(2 + ENTRY_LBAS) * LBA] = entries
    backup_entries = total - 1 - ENTRY_LBAS
    img[backup_entries * LBA:(total - 1) * LBA] = entries
    img[(total - 1) * LBA:total * LBA] = header(total - 1, 1, first_usable, last_usable,
                                                 backup_entries, ecrc)
    # The instance header in the partition's first block.
    hdr = MAGIC + struct.pack("<H", 1)
    img[FIRST * LBA:FIRST * LBA + len(hdr)] = hdr
    with open(out, "wb") as f:
        f.write(img)
    print(f"{out}: {total * LBA} bytes, vm-instance LBA {FIRST}..{last} ({part_mib} MiB)")


if __name__ == "__main__":
    main()
