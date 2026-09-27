import json

import httpx

from prusa_watch.config import NtfyConfig
from prusa_watch.replies import NtfyReplyListener, parse_command


def test_parse_command():
    assert parse_command("veto abc123") == ("veto", "abc123")
    assert parse_command("  ACT abc ") == ("act", "abc")
    assert parse_command("stop") is None
    assert parse_command("rm -rf /") is None
    assert parse_command("veto a b") is None


def test_stream_dispatches_messages_and_tracks_since():
    lines = [
        {"id": "o1", "event": "open", "topic": "r"},
        {"id": "k1", "event": "keepalive", "topic": "r"},
        {"id": "m1", "event": "message", "topic": "r", "message": "veto abc"},
        {"id": "m2", "event": "message", "topic": "r", "message": "hello there"},
        {"id": "m3", "event": "message", "topic": "r", "message": "act xyz"},
    ]
    seen_req = []

    def handler(request: httpx.Request):
        seen_req.append(request)
        body = "\n".join(json.dumps(x) for x in lines) + "\n"
        return httpx.Response(200, content=body.encode())

    got = []
    cfg = NtfyConfig(url="https://ntfy.example", reply_topic="reply-secret", token="tk_1")
    lst = NtfyReplyListener(cfg, lambda c, i: got.append((c, i)) or "ok", transport=httpx.MockTransport(handler))
    lst.stream_once()

    assert got == [("veto", "abc"), ("act", "xyz")]
    r = seen_req[0]
    assert r.url.path == "/reply-secret/json"
    assert r.headers["Authorization"] == "Bearer tk_1"
    assert r.url.params["since"].isdigit()  # starts from "now", never replays old replies
    assert lst._since == "m3"  # reconnect resumes after the last message
    assert lst.received == 2


def test_garbage_lines_are_ignored():
    lst = NtfyReplyListener(NtfyConfig(reply_topic="r"), lambda c, i: "ok")
    assert lst.handle_line("") is None
    assert lst.handle_line("not json") is None
    assert lst.handle_line(json.dumps({"event": "message", "message": "veto"})) is None
