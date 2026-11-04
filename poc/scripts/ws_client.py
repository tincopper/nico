#!/usr/bin/env python3
"""Minimal RFC 6455 client for the Pingora WebSocket spike.

Stdlib only: the Rust spikes intentionally avoid hyper/axum, and curl cannot
speak WebSocket. Prints one line per observed event so its output can be pasted
into FINDINGS.md as evidence.

Usage: python3 ws_client.py [host:port] [path]
"""

import base64
import hashlib
import os
import socket
import struct
import sys
import time

HOSTPORT = sys.argv[1] if len(sys.argv) > 1 else "127.0.0.1:18080"
PATH = sys.argv[2] if len(sys.argv) > 2 else "/ws"
GUID = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


def log(msg):
    print(f"{time.strftime('%H:%M:%S')} {msg}", flush=True)


def read_exact(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise EOFError(f"eof after {len(buf)}/{n} bytes")
        buf += chunk
    return buf


def read_head(sock):
    data = b""
    while b"\r\n\r\n" not in data:
        chunk = sock.recv(4096)
        if not chunk:
            raise EOFError("eof before response head")
        data += chunk
    head, _, rest = data.partition(b"\r\n\r\n")
    return head.decode("latin-1"), rest


def send_frame(sock, opcode, payload):
    mask = os.urandom(4)
    n = len(payload)
    header = bytes([0x80 | opcode])
    if n < 126:
        header += bytes([0x80 | n])
    elif n <= 0xFFFF:
        header += bytes([0x80 | 126]) + struct.pack("!H", n)
    else:
        header += bytes([0x80 | 127]) + struct.pack("!Q", n)
    masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    sock.sendall(header + mask + masked)


def recv_frame(sock):
    hdr = read_exact(sock, 2)
    fin = hdr[0] & 0x80 != 0
    opcode = hdr[0] & 0x0F
    masked = hdr[1] & 0x80 != 0
    n = hdr[1] & 0x7F
    if n == 126:
        n = struct.unpack("!H", read_exact(sock, 2))[0]
    elif n == 127:
        n = struct.unpack("!Q", read_exact(sock, 8))[0]
    mask = read_exact(sock, 4) if masked else None
    payload = read_exact(sock, n)
    if mask:
        payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    return opcode, fin, payload


def main():
    host, port = HOSTPORT.rsplit(":", 1)
    sock = socket.create_connection((host, int(port)), timeout=5)
    key = base64.b64encode(os.urandom(16)).decode()
    request = (
        f"GET {PATH} HTTP/1.1\r\n"
        f"Host: {HOSTPORT}\r\n"
        "Upgrade: websocket\r\n"
        "Connection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\n"
        "Sec-WebSocket-Version: 13\r\n"
        "\r\n"
    )
    t0 = time.monotonic()
    sock.sendall(request.encode())
    head, rest = read_head(sock)
    log(f"ws.response_head ({time.monotonic() - t0:.3f}s)")
    for line in head.split("\r\n"):
        log(f"  | {line}")
    expected = base64.b64encode(hashlib.sha1(key.encode() + GUID).digest()).decode()
    got = ""
    for line in head.split("\r\n")[1:]:
        if line.lower().startswith("sec-websocket-accept:"):
            got = line.split(":", 1)[1].strip()
    log(f"ws.accept_match={got == expected} expected={expected} got={got}")
    if rest:
        log(f"ws.unexpected_prefix_bytes={len(rest)}")

    for opcode, payload in ((0x1, b"hello-ws-1"), (0x2, b"\x00\x01\x02\x03\x04\x05\x06\x07")):
        send_frame(sock, opcode, payload)
        got_opcode, fin, echoed = recv_frame(sock)
        log(
            f"ws.echo opcode_sent={opcode} opcode_recv={got_opcode} fin={fin} "
            f"len={len(echoed)} match={echoed == payload} payload={echoed!r}"
        )

    send_frame(sock, 0x9, b"ping-payload")
    got_opcode, _fin, echoed = recv_frame(sock)
    log(f"ws.pong opcode={got_opcode} payload={echoed!r}")

    send_frame(sock, 0x8, struct.pack("!H", 1000))
    got_opcode, _fin, echoed = recv_frame(sock)
    log(f"ws.close opcode={got_opcode} payload={echoed.hex()}")
    sock.close()
    log("ws.client.done")


if __name__ == "__main__":
    main()
