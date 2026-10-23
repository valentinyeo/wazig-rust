"""Synthetic WhatsApp hub, compatible with Zig's /v1/wazig fixture protocol.
Only runs on Actions. No accounts, numbers, images, or messages are real.
"""
import argparse
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import struct
import threading
import time
from urllib.parse import parse_qs, unquote, urlsplit
import zlib

TOKEN = "fixture-key-not-a-secret"
CHATS = [f"personal:1555{i:07d}@s.whatsapp.net" for i in range(1, 201)]
CHATS[2] = "personal:120363000000003@g.us"
LOCK = threading.Lock()
REQUESTS = {}
LARGE_DIR = None


def chat_rows(large=False):
    rows = []
    for i, chat in enumerate(CHATS[:5] if large else CHATS):
        name = ("Fixture Alice" if i == 0 else "Fixture Official Group" if i == 2
                else "Fixture Push Name" if i == 3 else f"Fixture Contact {i + 1}")
        # Equal timestamps exercise name and JID tie-breaks; blank activity sorts last.
        stamp = "" if i == 199 else (NOW-timedelta(minutes=(i // 3))).isoformat().replace("+00:00", "Z")
        rows.append(dict(jid=chat, provider="whatsapp", account="personal", name=name,
                         kind="group" if i == 2 else "direct", unread_count=i % 3,
                         archived=chat in ARCHIVED, pinned=False, last_message_ts=stamp))
    return rows


def expected_rows(large=False):
    # A separate oracle follows chat_order.zig: descending timestamp, UTF-16 name, JID.
    rows = sorted(chat_rows(large), key=lambda c: (c["name"].encode("utf-16-be"), c["jid"]))
    return sorted(rows, key=lambda c: c["last_message_ts"], reverse=True)

SENDS = {}
ARCHIVED = set()
NOW = datetime.now(timezone.utc)


def png():
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    rows = b"".join(b"\x00" + b"\x40\xb0\x90" * 480 for _ in range(200))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", 480, 200, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def respond(self, data, status=200, content_type="application/json"):
        payload = json.dumps(data).encode() if content_type == "application/json" else data
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def authorized(self):
        if self.headers.get("Authorization") == f"Bearer {TOKEN}":
            return True
        self.respond({"error": "unauthorized"}, 401)
        return False

    def message(self, chat, message_id, text, **extra):
        return dict(MsgID=message_id, ChatJID=chat, Text=text, DisplayText=text,
                    SenderJID=CHATS[1], SenderName="Fixture Alice", FromMe=False,
                    Timestamp=NOW.isoformat().replace("+00:00", "Z"), **extra)

    def do_GET(self):
        url = urlsplit(self.path)
        if url.path == "/fixture/avatar":
            self.respond(png(), content_type="image/png")
            return
        if not self.authorized():
            return
        path = url.path
        large = getattr(self.server, "large", False)
        run = getattr(self.server, "run", "default")
        query = parse_qs(url.query)
        if path == "/v1/wazig/chats":
            archived = query.get("archived", ["false"])[0] == "true"
            with LOCK:
                rows = [row for row in chat_rows(large) if row["archived"] == archived]
            # Do not accidentally test a pre-sorted backend response.
            rows = rows[::2][::-1] + rows[1::2]
            if not large:
                rows += [dict(jid="personal:0@s.whatsapp.net", name="", kind="unknown", provider="whatsapp"),
                         dict(jid="personal:status@broadcast", name="Status", provider="whatsapp"),
                         dict(jid="opaque-junk", name="opaque-junk", kind="unknown", provider="whatsapp")]
            self.respond(rows)
        elif path == "/v1/wazig/messages":
            chat = query.get("chat", [""])[0]
            if chat not in CHATS:
                self.respond({"error": "unknown synthetic chat"}, 404)
                return
            if large:
                index = CHATS.index(chat)
                rows = [self.message(chat, f"L{index * 10 + j:02d}", f"Synthetic large photo {index * 10 + j}",
                                     MediaType="image", MimeType="image/jpeg", Filename=f"large-{index * 10 + j:02d}.jpg") for j in range(10)]
                self.respond(list(reversed(rows)))
                return
            rows = [self.message(chat, "HELLO", "Hello from the synthetic WhatsApp fixture 👋"),
                    self.message(chat, "QUOTE", "Reply to our plans", quoted_msg_id="HELLO",
                                 quoted_sender_jid=CHATS[1]),
                    self.message(chat, "IMAGE", "A synthetic green image", MediaType="image",
                                 MimeType="image/png", Filename="fixture.png"),
                    self.message(chat, "OUT", "This is a fake outgoing message"),
                    self.message(chat, "REPLY", "Thanks! 🦀", ReactionEmoji="👍")]
            rows[1]["DisplayText"] = "> Fixture Alice: Hello from the fixture\n\nReply to our plans"
            rows[3]["FromMe"] = True
            rows[3]["SenderName"] = "You"
            with LOCK:
                for client_id, row in SENDS.items():
                    if row["chat"] == chat:
                        message = self.message(chat, client_id, row["text"])
                        message.update(FromMe=True, SenderName="You")
                        rows.append(message)
            self.respond(list(reversed(rows)))
        elif path.startswith("/v1/wazig/media/"):
            message_id = unquote(path.rsplit("/", 1)[1])
            with LOCK:
                REQUESTS.setdefault(run, set()).add(path)
            if large:
                time.sleep(0.05)
                self.respond((LARGE_DIR / f"{message_id}.jpg").read_bytes(), content_type="image/jpeg")
            else:
                self.respond(png(), content_type="image/png")
        elif path.startswith("/v1/wazig/avatar/"):
            self.respond(png(), content_type="image/png")
        elif path == "/v1/wazig/events":
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Connection", "close")
            self.end_headers()
            try:
                # A quiet SSE connection, like an idle real account.
                condition = threading.Condition()
                with condition:
                    condition.wait(timeout=180)
                self.wfile.write(b": fixture heartbeat\n\n")
            except (ConnectionError, BrokenPipeError):
                pass
            self.close_connection = True
        elif path == "/fixture/state":
            with LOCK:
                self.respond({"sends": list(SENDS.values()), "media_requests": {k: sorted(v) for k, v in REQUESTS.items()}})
        elif path == "/fixture/expected":
            self.respond(expected_rows(large))
        else:
            self.respond({"error": "unknown fixture route"}, 404)

    def do_POST(self):
        if not self.authorized():
            return
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        if body.get("chat") not in CHATS:
            self.respond({"error": "not a synthetic chat"}, 400)
            return
        path = self.path
        if path.startswith("/large/"):
            path = "/" + path.split("/", 3)[3]
        if path == "/v1/wazig/send":
            if set(body) != {"chat", "text", "reply_to", "client_id"} or not body["client_id"]:
                self.respond({"error": "invalid send contract"}, 400)
                return
            with LOCK:
                SENDS.setdefault(body["client_id"], body)
            self.respond(dict(client_id=body["client_id"], msg_id=body["client_id"],
                              status="sent", ts=NOW.isoformat()))
        elif path == "/v1/wazig/archive":
            with LOCK:
                if body["archived"]:
                    ARCHIVED.add(body["chat"])
                else:
                    ARCHIVED.discard(body["chat"])
            self.respond({"status": "ok"})
        elif path in ("/v1/wazig/mark-read", "/v1/wazig/react"):
            self.respond({"status": "ok"})
        else:
            self.respond({"error": "unknown fixture write"}, 404)


if __name__ == "__main__":
    if os.environ.get("GITHUB_ACTIONS") != "true":
        raise SystemExit("This fake backend runs only on GitHub Actions")
    parser = argparse.ArgumentParser()
    parser.add_argument("--port-file", type=Path, required=True)
    args = parser.parse_args()
    from PIL import Image, ImageDraw
    LARGE_DIR = args.port_file.parent / "large-images"
    LARGE_DIR.mkdir()
    for i in range(50):
        image = Image.new("RGB", (4000, 3000), (40 + i * 3, 100, 150))
        draw = ImageDraw.Draw(image)
        for y in range(0, 3000, 60):
            draw.rectangle((i * 20, y, 3999 - i * 10, y + 15), fill=(180, (y // 12 + i) % 256, 70))
        image.save(LARGE_DIR / f"L{i:02d}.jpg", quality=90)
        image.close()
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    for run in ("rust", "zig"):
        large_server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        large_server.large = True
        large_server.run = run
        threading.Thread(target=large_server.serve_forever, daemon=True).start()
        args.port_file.with_name(f"large-{run}-port.txt").write_text(str(large_server.server_port))
    args.port_file.write_text(str(server.server_port))
    server.serve_forever()
