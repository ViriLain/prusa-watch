"""Notifications: ntfy (default, self-hostable), Discord webhook, generic JSON webhook.

Which channels, what priority and which buttons each notification gets is
decided by the caller (escalation steps and notify.warning/camera/info config).
Sends run on a background thread so a slow endpoint can never delay a pause.
"""

from __future__ import annotations

import json
import logging
import threading
from dataclasses import dataclass, field
from datetime import datetime, timezone

import httpx

from .config import CHANNELS, NotifyConfig

log = logging.getLogger(__name__)


@dataclass
class Event:
    kind: str  # "incident" | "failure" | "warning" | "camera_down" | "camera_up" | "info"
    title: str
    message: str
    printer: str
    job_id: int | None = None
    job_name: str | None = None
    score: float | None = None
    action_taken: str | None = None  # "paused" | "stopped" | None
    image_jpeg: bytes | None = None
    priority: int = 3  # ntfy 1..5
    buttons: list[str] = field(default_factory=list)  # see escalation.BUTTONS
    # Escalation context
    incident_id: str | None = None
    policy: str | None = None
    next_action: str | None = None
    next_action_ts: float | None = None
    ts: str = field(default_factory=lambda: datetime.now(timezone.utc).isoformat())

    def to_json(self) -> dict:
        d = {k: v for k, v in self.__dict__.items() if k != "image_jpeg"}
        d["has_image"] = self.image_jpeg is not None
        return d


def _ascii(s: str) -> str:
    # HTTP headers must be latin-1; keep it simple and safe.
    return s.encode("ascii", "replace").decode("ascii")


def _q(v: str) -> str:
    # ntfy action header values containing , or ; must be quoted
    return f'"{v}"' if ("," in v or ";" in v) else v


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
        self.control_token = control_token
        self._client = httpx.Client(timeout=cfg.timeout_s, transport=transport)
        self.sent = 0
        self.errors = 0

    @property
    def configured(self) -> list[str]:
        c = self.cfg
        return [
            name
            for name, ok in (("ntfy", bool(c.ntfy.topic)), ("discord", bool(c.discord.webhook_url)), ("webhook", bool(c.webhook.url)))
            if ok
        ]

    @property
    def enabled(self) -> bool:
        return bool(self.configured)

    def send(self, event: Event, channels: list[str] | None = None, blocking: bool = False) -> None:
        """channels=None -> every configured channel; [] -> nothing."""
        targets = [c for c in (self.configured if channels is None else channels) if c in self.configured]
        if not targets:
            log.info("Notify (no channel): %s - %s", event.title, event.message)
            return
        if blocking:
            self._send_all(event, targets)
        else:
            threading.Thread(target=self._send_all, args=(event, targets), name="notify", daemon=True).start()

    def _send_all(self, event: Event, targets: list[str]) -> None:
        fns = {"ntfy": self._ntfy, "discord": self._discord, "webhook": self._webhook}
        for name in CHANNELS:
            if name not in targets:
                continue
            try:
                fns[name](event)
                self.sent += 1
            except Exception as exc:
                self.errors += 1
                log.error("Notify via %s failed: %s", name, exc)

    # -- buttons -----------------------------------------------------------
    def _dashboard_url(self, cmd: str, incident_id: str) -> str:
        q = f"?token={self.control_token}&" if self.control_token else "?"
        return f"{self.public_url}/api/incident/{cmd}{q}id={incident_id}"

    def ntfy_actions(self, e: Event) -> list[str]:
        """Translate button names into ntfy action definitions.

        Replies go to ntfy.reply_topic when set (works from anywhere: prusa-watch
        subscribes outbound); otherwise straight to the dashboard (LAN/VPN only).
        """
        from .escalation import BUTTONS

        c = self.cfg.ntfy
        out = []
        for b in e.buttons:
            cmd, label = BUTTONS[b]
            if b == "act":
                label = "Stop now" if e.next_action == "stop" else "Pause now"
            if cmd is None:  # dashboard link
                if self.public_url:
                    out.append(f"view, {label}, {self.public_url}")
                continue
            if not e.incident_id:
                continue
            if c.reply_topic:
                url = f"{c.url.rstrip('/')}/{c.reply_topic}"
                auth = f", headers.Authorization=Bearer {c.token}" if c.token else ""
                out.append(f"http, {_q(label)}, {url}, method=POST{auth}, body={cmd} {e.incident_id}, clear=true")
            elif self.public_url:
                out.append(f"http, {_q(label)}, {self._dashboard_url(cmd, e.incident_id)}, method=POST, clear=true")
        return out[:3]

    # -- channels ---------------------------------------------------------
    def _ntfy(self, e: Event) -> None:
        c = self.cfg.ntfy
        url = f"{c.url.rstrip('/')}/{c.topic}"
        tags = {
            "failure": "rotating_light,printer",
            "incident": "hourglass_flowing_sand,printer",
            "warning": "warning,printer",
            "camera_down": "no_entry_sign,camera",
            "camera_up": "white_check_mark,camera",
        }.get(e.kind, "printer")
        headers = {"Title": _ascii(e.title), "Priority": str(max(1, min(5, int(e.priority)))), "Tags": tags}
        if c.token:
            headers["Authorization"] = f"Bearer {c.token}"
        actions = self.ntfy_actions(e)
        if actions:
            headers["Actions"] = "; ".join(actions)
        if self.public_url:
            headers["Click"] = self.public_url
        if e.image_jpeg:
            headers["Filename"] = "frame.jpg"
            headers["Message"] = _ascii(e.message)
            r = self._client.put(url, content=e.image_jpeg, headers=headers)
        else:
            r = self._client.post(url, content=e.message.encode("utf-8"), headers=headers)
        r.raise_for_status()

    def _discord(self, e: Event) -> None:
        color = {"failure": 0xE5484D, "incident": 0xE5484D, "warning": 0xF5A524, "camera_down": 0x8B8D98}.get(e.kind, 0x3E63DD)
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
        from .escalation import BUTTONS

        body = e.to_json()
        if self.public_url and e.image_jpeg:
            body["image_url"] = f"{self.public_url}/frame.jpg"
        if e.incident_id and self.public_url:
            body["command_urls"] = {
                BUTTONS[b][0]: self._dashboard_url(BUTTONS[b][0], e.incident_id)
                for b in ("keep", "act", "stop", "resume", "mute")
            }
        r = self._client.post(self.cfg.webhook.url, json=body)
        r.raise_for_status()
