"""Notifications: ntfy (default, self-hostable), Discord webhook, generic JSON webhook.

Sends run on a background thread so a slow notification endpoint can never
delay a pause.
"""

from __future__ import annotations

import json
import logging
import threading
from dataclasses import dataclass, field
from datetime import datetime, timezone

import httpx

from .config import NotifyConfig

log = logging.getLogger(__name__)


@dataclass
class Event:
    kind: str  # "warning" | "failure" | "camera_down" | "camera_up" | "info"
    title: str
    message: str
    printer: str
    job_id: int | None = None
    job_name: str | None = None
    score: float | None = None
    action_taken: str | None = None  # "paused" | "stopped" | None
    image_jpeg: bytes | None = None
    ts: str = field(default_factory=lambda: datetime.now(timezone.utc).isoformat())

    def to_json(self) -> dict:
        d = {k: v for k, v in self.__dict__.items() if k != "image_jpeg"}
        d["has_image"] = self.image_jpeg is not None
        return d


def _ascii(s: str) -> str:
    # HTTP headers must be latin-1; keep it simple and safe.
    return s.encode("ascii", "replace").decode("ascii")


class Notifier:
    def __init__(
        self,
        cfg: NotifyConfig,
        public_url: str = "",
        control_token: str = "",
        transport: httpx.BaseTransport | None = None,
    ):
        self.cfg = cfg
        self.public_url = public_url.rstrip("/")
        self._q = f"?token={control_token}" if control_token else ""
        self._client = httpx.Client(timeout=15, transport=transport)
        self.sent = 0
        self.errors = 0

    @property
    def enabled(self) -> bool:
        return bool(self.cfg.ntfy.topic or self.cfg.discord.webhook_url or self.cfg.webhook.url)

    def send(self, event: Event, blocking: bool = False) -> None:
        if not self.enabled:
            log.info("Notify (no channels configured): %s - %s", event.title, event.message)
            return
        if blocking:
            self._send_all(event)
        else:
            threading.Thread(target=self._send_all, args=(event,), name="notify", daemon=True).start()

    def _send_all(self, event: Event) -> None:
        for name, fn, enabled in (
            ("ntfy", self._ntfy, bool(self.cfg.ntfy.topic)),
            ("discord", self._discord, bool(self.cfg.discord.webhook_url)),
            ("webhook", self._webhook, bool(self.cfg.webhook.url)),
        ):
            if not enabled:
                continue
            try:
                fn(event)
                self.sent += 1
            except Exception as exc:
                self.errors += 1
                log.error("Notify via %s failed: %s", name, exc)

    # -- channels ---------------------------------------------------------
    def _ntfy(self, e: Event) -> None:
        c = self.cfg.ntfy
        url = f"{c.url.rstrip('/')}/{c.topic}"
        prio = {"failure": c.priority_failure, "warning": c.priority_warning, "camera_down": 3}.get(e.kind, 3)
        tags = {"failure": "rotating_light,printer", "warning": "warning,printer", "camera_down": "no_entry_sign,camera"}.get(
            e.kind, "printer"
        )
        headers = {"Title": _ascii(e.title), "Priority": str(prio), "Tags": tags}
        if c.token:
            headers["Authorization"] = f"Bearer {c.token}"
        if self.public_url:
            # ntfy allows max 3 actions; tapping the notification opens the dashboard.
            base, q = self.public_url, self._q
            sep = "&" if q else "?"
            actions = []
            if e.kind == "failure" and e.action_taken == "paused":
                actions.append(f"http, Resume, {base}/api/resume{q}, method=POST, clear=true")
                actions.append(f"http, False alarm: resume + mute, {base}/api/resume{q}{sep}mute=1, method=POST, clear=true")
            elif e.kind in ("failure", "warning"):
                actions.append(f"view, Dashboard, {base}")
                actions.append(f"http, Pause, {base}/api/pause{q}, method=POST, clear=true")
            if e.kind in ("failure", "warning"):
                actions.append(f"http, Cancel print, {base}/api/stop{q}, method=POST, clear=true")
            if actions:
                headers["Actions"] = "; ".join(actions)
            headers["Click"] = base
        if e.image_jpeg:
            headers["Filename"] = "frame.jpg"
            headers["Message"] = _ascii(e.message)
            r = self._client.put(url, content=e.image_jpeg, headers=headers)
        else:
            r = self._client.post(url, content=e.message.encode("utf-8"), headers=headers)
        r.raise_for_status()

    def _discord(self, e: Event) -> None:
        color = {"failure": 0xE5484D, "warning": 0xF5A524, "camera_down": 0x8B8D98}.get(e.kind, 0x3E63DD)
        embed = {"title": e.title, "description": e.message, "color": color, "timestamp": e.ts}
        if e.image_jpeg:
            embed["image"] = {"url": "attachment://frame.jpg"}
        payload = {"embeds": [embed]}
        if e.image_jpeg:
            r = self._client.post(
                self.cfg.discord.webhook_url,
                data={"payload_json": json.dumps(payload)},
                files={"files[0]": ("frame.jpg", e.image_jpeg, "image/jpeg")},
            )
        else:
            r = self._client.post(self.cfg.discord.webhook_url, json=payload)
        r.raise_for_status()

    def _webhook(self, e: Event) -> None:
        body = e.to_json()
        if self.public_url and e.image_jpeg:
            body["image_url"] = f"{self.public_url}/frame.jpg"
        r = self._client.post(self.cfg.webhook.url, json=body)
        r.raise_for_status()
