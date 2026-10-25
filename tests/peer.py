#!/usr/bin/env python3
"""Simulated EchoLink station for the qso engine test. Prints a JSON report."""
import json, select, socket, struct, sys

me, client = sys.argv[1], sys.argv[2]
audio = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); audio.bind((me, 5198))
ctrl = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); ctrl.bind((me, 5199))

def sdes(call, name):
    rr = bytes([0xC0, 201, 0, 1, 0, 0, 0, 0])
    items = b""
    for kind, text in ((1, b"CALLSIGN"), (2, f"{call:<15}{name}".encode()), (3, b"CALLSIGN"), (4, b"08:30")):
        items += bytes([kind, len(text)]) + text
    body = bytes([0xC1, 202, 0, 0, 0, 0, 0, 0]) + items + b"\0\0"
    body += b"\0" * (-len(body) % 4)
    return rr + body[:2] + struct.pack(">H", len(body) // 4 - 1) + body[4:]

report = {"sdes_from_client": None, "info": None, "chat": None, "voice": 0, "voice_sizes": set(), "bye": False}
state = "wait_sdes"
while True:
    r, _, _ = select.select([audio, ctrl], [], [], 20)
    if not r:
        report["error"] = f"timeout in state {state}"; break
    sock = r[0]
    data, (ip, port) = sock.recvfrom(2048)
    if ip != client:
        continue
    if sock is ctrl:
        if data[1] == 201 and len(data) > 9 and data[9] == 203:
            report["bye"] = True; break
        if state == "wait_sdes":
            i = 16
            while data[i] != 0:
                if data[i] == 2: report["sdes_from_client"] = data[i+2:i+2+data[i+1]].decode()
                i += 2 + data[i+1]
            ctrl.sendto(sdes("PEER-L", "Simulated node"), (client, 5199))
            state = "wait_info"
        continue
    if data[:7] == b"oNDATA\r":
        report["info"] = data[7:].split(b"\0")[0].decode()
        audio.sendto(b"oNDATA\rpeer info\rline 2\0", (client, 5198))
        audio.sendto(b"oNDATAPEER-L>hi there\r\n\0", (client, 5198))
        frame = b"\xd8" + b"\x55" * 32
        for seq in range(10):
            audio.sendto(bytes([0xC0, 3]) + struct.pack(">H", seq) + b"\0" * 8 + frame * 4, (client, 5198))
    elif data[:6] == b"oNDATA":
        report["chat"] = data[6:].split(b"\0")[0].decode()
    elif data[0] == 0xC0 and data[1] == 3:
        report["voice"] += 1; report["voice_sizes"].add(len(data))
report["voice_sizes"] = sorted(report["voice_sizes"])
print(json.dumps(report))
