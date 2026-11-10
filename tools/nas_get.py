#!/usr/bin/env python3
"""Read the modem's current network preference, including voice domain."""
import struct
import subprocess

import sys
import os
import u60env
ADB = u60env.ADB
u60env.WORK_DIR.mkdir(parents=True, exist_ok=True)
ELF = str(u60env.WORK_DIR / "nas_get")
BASE = 0x401000
ENTRY = 0x400000 + 176
port = int(sys.argv[1]) if len(sys.argv) > 1 else 67
msgid = int(sys.argv[2], 16) if len(sys.argv) > 2 else 0x34
extra = bytes.fromhex(sys.argv[3]) if len(sys.argv) > 3 else b""
pkt = bytes([0x00, 0x05, 0x00, msgid & 0xFF, (msgid >> 8) & 0xFF]) + struct.pack("<H", len(extra)) + extra


def movz(rd, imm):
    return 0xD2800000 | ((imm & 0xFFFF) << 5) | rd


def movr(rd, rm):
    return 0xAA000000 | (rm << 16) | (31 << 5) | rd


def adr(rd, pc, target):
    off = target - pc
    return ((off & 3) << 29) | 0x10000000 | (((off >> 2) & 0x7FFFF) << 5) | rd


def st(rt, rn):
    return 0xF9000000 | (rn << 5) | rt


data = bytearray(0x5000)
struct.pack_into("<HHII", data, 0xB00, 42, 0, 3, port)
data[0xB20:0xB20 + len(pkt)] = pkt
struct.pack_into("<qq", data, 0xB80, 3, 0)
code = bytearray()


def emit(word):
    code.extend(struct.pack("<I", word))


def here():
    return ENTRY + len(code)


for word in (movz(0, 42), movz(1, 2), movz(2, 0), movz(8, 198)):
    emit(word)
emit(0xD4000001)
emit(movr(19, 0))
emit(movr(0, 19))
emit(movz(1, 1))
emit(movz(2, 20))
emit(adr(3, here(), BASE + 0xB80))
emit(movz(4, 16))
emit(movz(8, 208))
emit(0xD4000001)
if port == 65 and os.environ.get("U60_BIND_VOICE") == "1":
    bind = bytes.fromhex("0011004400040001010000")
    data[0xC00:0xC00 + len(bind)] = bind
    emit(movr(0, 19))
    emit(adr(1, here(), BASE + 0xC00))
    emit(movz(2, len(bind)))
    emit(movz(3, 0))
    emit(adr(4, here(), BASE + 0xB00))
    emit(movz(5, 12))
    emit(movz(8, 206))
    emit(0xD4000001)
    emit(movr(0, 19))
    emit(adr(1, here(), BASE + 0xD00))
    emit(movz(2, 0x100))
    for reg in (3, 4, 5):
        emit(movz(reg, 0))
    emit(movz(8, 207))
    emit(0xD4000001)
    emit(adr(1, here(), BASE + 0x60))
    emit(st(0, 1))
emit(movr(0, 19))
emit(adr(1, here(), BASE + 0xB20))
emit(movz(2, len(pkt)))
emit(movz(3, 0))
emit(adr(4, here(), BASE + 0xB00))
emit(movz(5, 12))
emit(movz(8, 206))
emit(0xD4000001)
emit(adr(1, here(), BASE))
emit(st(0, 1))
# Notifications may precede the response; collect separate bounded packets.
for index, dest, size in ((i, 0x1000 + i * 0x900, 0x900) for i in range(6)):
    emit(movr(0, 19))
    emit(adr(1, here(), BASE + dest))
    emit(movz(2, size))
    emit(movz(3, 0))
    emit(movz(4, 0))
    emit(movz(5, 0))
    emit(movz(8, 207))
    emit(0xD4000001)
    emit(adr(1, here(), BASE + 8 + index * 8))
    emit(st(0, 1))
emit(movz(0, 1))
emit(adr(1, here(), BASE))
emit(movz(2, len(data)))
emit(movz(8, 64))
emit(0xD4000001)
emit(movz(0, 0))
emit(movz(8, 93))
emit(0xD4000001)

blob = bytearray(0x1000 + len(data))
blob[0:64] = struct.pack(
    "<16sHHIQQQIHHHHHH",
    b"\x7fELF\x02\x01\x01" + b"\x00" * 9,
    2, 183, 1, ENTRY, 64, 0, 0, 64, 56, 2, 64, 0, 0,
)
blob[64:120] = struct.pack("<IIQQQQQQ", 1, 5, 0, 0x400000, 0x400000, 0x1000, 0x1000, 0x1000)
blob[120:176] = struct.pack("<IIQQQQQQ", 1, 6, 0x1000, BASE, BASE, len(data), len(data), 0x1000)
blob[176:176 + len(code)] = code
blob[0x1000:] = data
open(ELF, "wb").write(blob)
subprocess.run([ADB, "push", ELF, "/tmp/nas_get"], check=True)
subprocess.run([ADB, "shell", "chmod 755 /tmp/nas_get"], check=True)
proc = subprocess.run([ADB, "exec-out", "/tmp/nas_get"], capture_output=True)
raw = proc.stdout
send, n1, n2 = struct.unpack_from("<qqq", raw, 0)
print("send", send, "n1", n1, "n2", n2)
if port == 65 and os.environ.get("U60_BIND_VOICE") == "1":
    bind_len = struct.unpack_from("<q", raw, 0x60)[0]
    print("bind", raw[0xD00:0xD00 + max(0, min(bind_len, 0x100))].hex())


def tlvs(buf):
    if len(buf) < 7:
        return
    msg = struct.unpack_from("<H", buf, 3)[0]
    body = buf[7:7 + min(struct.unpack_from("<H", buf, 5)[0], len(buf) - 7)]
    print("msgid", hex(msg), "body", len(body))
    i = 0
    while i + 3 <= len(body):
        tlv = body[i]
        ln = struct.unpack_from("<H", body, i + 1)[0]
        val = body[i + 3:i + 3 + ln]
        print(f"  TLV {tlv:#04x} {val.hex()}")
        i += 3 + ln


for index in range(6):
    n = struct.unpack_from("<q", raw, 8 + index * 8)[0]
    if n <= 0:
        continue
    off = 0x1000 + index * 0x900
    got = raw[off:off + min(n, 0x900)]
    label = "response" if got and got[0] == 2 else "indication"
    print(label, got.hex())
    tlvs(got)
