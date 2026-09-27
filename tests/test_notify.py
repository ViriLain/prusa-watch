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


def test_webhook_json_and_ntfy_token_and_buttons():
    cfg = NotifyConfig()
    cfg.webhook.url = "http://ha.lan:8123/api/webhook/prusa"
    cfg.ntfy.topic = "t"
    cfg.ntfy.token = "tk_123"
    reqs, t = capture()
    ev_ = ev(action_taken="paused", buttons=["resume", "mute", "stop"], incident_id="abc", priority=4)
    Notifier(cfg, public_url="http://watch.lan:8484", transport=t).send(ev_, blocking=True)
    ntfy, hook = reqs
    assert ntfy.headers["Authorization"] == "Bearer tk_123" and ntfy.headers["Priority"] == "4"
    acts = ntfy.headers["Actions"]
    # no reply topic -> dashboard fallback URLs; label with a comma is quoted
    assert "http, Resume, http://watch.lan:8484/api/incident/resume?id=abc" in acts
    assert '"False alarm: resume + mute"' not in acts  # no comma in that label, no quoting needed
    body = json.loads(hook.read())
    assert body["kind"] == "failure" and body["has_image"] and body["image_url"].endswith("/frame.jpg")
    assert body["command_urls"]["veto"] == "http://watch.lan:8484/api/incident/veto?id=abc"


def test_channel_selection():
    cfg = NotifyConfig()
    cfg.ntfy.topic, cfg.webhook.url = "t", "http://hook"
    reqs, t = capture()
    n = Notifier(cfg, transport=t)
    n.send(ev(), channels=["webhook"], blocking=True)
    n.send(ev(), channels=[], blocking=True)
    n.send(ev(), channels=["discord"], blocking=True)  # not configured -> dropped
    assert [r.url.host for r in reqs] == ["hook"]


def test_reply_topic_buttons():
    cfg = NotifyConfig()
    cfg.ntfy.topic, cfg.ntfy.reply_topic = "t", "r"
    n = Notifier(cfg)
    acts = n.ntfy_actions(ev(buttons=["keep", "act", "stop"], incident_id="i1", next_action="stop"))
    assert acts == [
        "http, Keep printing, https://ntfy.sh/r, method=POST, body=veto i1, clear=true",
        "http, Stop now, https://ntfy.sh/r, method=POST, body=act i1, clear=true",
        "http, Cancel print, https://ntfy.sh/r, method=POST, body=stop i1, clear=true",
    ]
    assert n.ntfy_actions(ev(buttons=["keep"])) == []  # no incident id -> no reply buttons


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
