# Sends WebSocket commands (protocol v1) to the box and prints the replies.
# Usage: python3 -I scripts/ws-command.py HOST COMMAND_JSON...
import socket, base64, os, json, sys, time
host = sys.argv[1]; cmds = sys.argv[2:]
s = socket.create_connection((host, 80), timeout=10)
key = base64.b64encode(os.urandom(16)).decode()
s.sendall((f"GET /ws HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
           f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
buf = b""
while b"\r\n\r\n" not in buf: buf += s.recv(1024)
head, buf = buf.split(b"\r\n\r\n", 1)
print(head.split(b"\r\n")[0].decode())
def send(text):
    data = text.encode(); mask = os.urandom(4)
    hdr = bytes([0x81]) + (bytes([0x80 | len(data)]) if len(data) < 126 else bytes([0x80 | 126]) + len(data).to_bytes(2, "big"))
    s.sendall(hdr + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(data)))
def frames(t_end):
    global buf
    while time.time() < t_end:
        try:
            chunk = s.recv(8192)
            if not chunk: return
            buf += chunk
        except socket.timeout:
            continue
        while len(buf) >= 2:
            n = buf[1] & 0x7f; off = 2
            if n == 126: n = int.from_bytes(buf[2:4], "big"); off = 4
            elif n == 127: n = int.from_bytes(buf[2:10], "big"); off = 10
            if len(buf) < off + n: break
            payload = buf[off:off+n]; buf = buf[off+n:]
            yield payload.rstrip(b"\0").decode("utf-8", "replace")
s.settimeout(0.5)
for m in frames(time.time() + 2):
    t = json.loads(m).get("type") if m.startswith("{") else m
    print("<", t, m[:120] if t in ("hello", "busy", "error", "ota") else "")
for c in cmds:
    send(c); print(">", c)
    for m in frames(time.time() + 3):
        t = json.loads(m).get("type") if m.startswith("{") else m
        if t == "status":
            print("< status ota:", json.loads(m).get("ota"))
        elif t not in ("beat",):
            print("<", t, m[:200])
