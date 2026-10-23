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
from urllib.parse import parse_qs, unquote, urlsplit
import zlib

TOKEN = "fixture-key-not-a-secret"
CHATS = [f"personal:1555{i:07d}@s.whatsapp.net" for i in range(1, 201)]
LOCK = threading.Lock()
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
        if not self.authorized():
            return
        url = urlsplit(self.path)
        query = parse_qs(url.query)
        if url.path == "/v1/wazig/chats":
            archived = query.get("archived", ["false"])[0] == "true"
            with LOCK:
                rows = [dict(jid=chat, provider="whatsapp", account="personal",
                             name=f"Fixture {'Alice' if i == 0 else 'Contact ' + str(i + 1)}",
                             kind="group" if i % 7 == 6 else "direct", unread_count=i % 3,
                             archived=chat in ARCHIVED,
                             last_message_ts=(NOW-timedelta(minutes=i)).isoformat().replace("+00:00", "Z"))
                        for i, chat in enumerate(CHATS) if (chat in ARCHIVED) == archived]
            self.respond(rows)
        elif url.path == "/v1/wazig/messages":
            chat = query.get("chat", [""])[0]
            if chat not in CHATS:
                self.respond({"error": "unknown synthetic chat"}, 404)
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
        elif url.path.startswith("/v1/wazig/media/"):
            self.respond(png(), content_type="image/png")
        elif url.path.startswith("/v1/wazig/avatar/"):
            self.respond({"error": "no synthetic avatar"}, 404)
        elif url.path == "/v1/wazig/events":
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Connection", "close")
            self.end_headers()
            try:
                # A quiet SSE connection, like an idle real account.
                with threading.Condition() as condition:
                    condition.wait(timeout=180)
                self.wfile.write(b": fixture heartbeat\n\n")
            except (ConnectionError, BrokenPipeError):
                pass
            self.close_connection = True
        elif url.path == "/fixture/state":
            with LOCK:
                self.respond({"sends": list(SENDS.values())})
        else:
            self.respond({"error": "unknown fixture route"}, 404)

    def do_POST(self):
        if not self.authorized():
            return
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        if body.get("chat") not in CHATS:
            self.respond({"error": "not a synthetic chat"}, 400)
            return
        if self.path == "/v1/wazig/send":
            if set(body) != {"chat", "text", "reply_to", "client_id"} or not body["client_id"]:
                self.respond({"error": "invalid send contract"}, 400)
                return
            with LOCK:
                SENDS.setdefault(body["client_id"], body)
            self.respond(dict(client_id=body["client_id"], msg_id=body["client_id"],
                              status="sent", ts=NOW.isoformat()))
        elif self.path == "/v1/wazig/archive":
            with LOCK:
                if body["archived"]:
                    ARCHIVED.add(body["chat"])
                else:
                    ARCHIVED.discard(body["chat"])
            self.respond({"status": "ok"})
        elif self.path in ("/v1/wazig/mark-read", "/v1/wazig/react"):
            self.respond({"status": "ok"})
        else:
            self.respond({"error": "unknown fixture write"}, 404)


if __name__ == "__main__":
    if os.environ.get("GITHUB_ACTIONS") != "true":
        raise SystemExit("This fake backend runs only on GitHub Actions")
    parser = argparse.ArgumentParser()
    parser.add_argument("--port-file", type=Path, required=True)
    args = parser.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    args.port_file.write_text(str(server.server_port))
    server.serve_forever()
