import json

import httpx

from prusa_watch.config import NotifyConfig
from prusa_watch.notify import Event, Notifier


def capture():
    reqs = []

    def handler(request):
        reqs.append(request)
        return httpx.Response(204)

    return reqs, httpx.MockTransport(handler)


def ev(**kw):
    base = dict(kind="failure", title="core-one: failure", message="paused", printer="core-one", job_id=1, image_jpeg=b"\xff\xd8jpeg")
    base.update(kw)
    return Event(**base)


def test_discord_multipart_with_image():
    cfg = NotifyConfig()
    cfg.discord.webhook_url = "https://discord.example/api/webhooks/1/abc"
    reqs, t = capture()
    Notifier(cfg, transport=t).send(ev(), blocking=True)
    (r,) = reqs
    assert r.headers["content-type"].startswith("multipart/form-data")
    body = r.read()
    assert b"payload_json" in body and b"attachment://frame.jpg" in body and b"\xff\xd8jpeg" in body


def test_webhook_json_and_ntfy_token():
    cfg = NotifyConfig()
    cfg.webhook.url = "http://ha.lan:8123/api/webhook/prusa"
    cfg.ntfy.topic = "t"
    cfg.ntfy.token = "tk_123"
    reqs, t = capture()
    Notifier(cfg, public_url="http://watch.lan:8484", transport=t).send(ev(action_taken="paused"), blocking=True)
    ntfy, hook = reqs
    assert ntfy.headers["Authorization"] == "Bearer tk_123"
    assert "Resume" in ntfy.headers["Actions"]
    body = json.loads(hook.read())
    assert body["kind"] == "failure" and body["has_image"] and body["image_url"].endswith("/frame.jpg")


def test_non_ascii_titles_do_not_crash_headers():
    cfg = NotifyConfig()
    cfg.ntfy.topic = "t"
    reqs, t = capture()
    n = Notifier(cfg, transport=t)
    n.send(ev(title="Druck fehlgeschlagen – Spaghetti ✗"), blocking=True)
    assert n.errors == 0 and len(reqs) == 1


def test_channel_failure_is_isolated():
    cfg = NotifyConfig()
    cfg.ntfy.topic = "t"
    cfg.webhook.url = "http://hook"

    def handler(request):
        if "hook" in str(request.url):
            return httpx.Response(500)
        return httpx.Response(200)

    n = Notifier(cfg, transport=httpx.MockTransport(handler))
    n.send(ev(), blocking=True)
    assert n.sent == 1 and n.errors == 1
