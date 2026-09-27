"""Listen for button replies on an ntfy topic.

Alert buttons POST a tiny text body ("<command> <incident-id>", e.g. "veto x1Y2z3") to
``notify.ntfy.reply_topic``. We hold an outbound streaming subscription to that
topic (``GET /<topic>/json``), so replies arrive whether your phone is on the
home Wi-Fi or on LTE, with nothing exposed to the internet.

Every command must carry the id of the *current* incident; anything else
(old alerts, replays, junk someone posted to the topic) is ignored.
"""

from __future__ import annotations

import json
import logging
import threading
import time
from typing import Callable

import httpx

from .config import NtfyConfig

log = logging.getLogger(__name__)

COMMANDS = ("veto", "act", "stop", "resume", "mute")


def parse_command(text: str) -> tuple[str, str] | None:
    parts = (text or "").strip().split()
    if len(parts) != 2 or parts[0].lower() not in COMMANDS:
        return None
    return parts[0].lower(), parts[1]


class NtfyReplyListener:
    def __init__(
        self,
        cfg: NtfyConfig,
        handler: Callable[[str, str], str | None],
        transport: httpx.BaseTransport | None = None,
    ):
        self.cfg = cfg
        self.handler = handler
        self.backoff_s = cfg.reply_reconnect_s
        headers = {"Authorization": f"Bearer {cfg.token}"} if cfg.token else {}
        self._client = httpx.Client(headers=headers, transport=transport, timeout=httpx.Timeout(10.0, read=120.0))
        self._since = str(int(time.time()))  # never act on replies older than our start
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None
        self.connected = False
        self.received = 0

    @property
    def url(self) -> str:
        return f"{self.cfg.url.rstrip('/')}/{self.cfg.reply_topic}/json"

    def start(self) -> None:
        self._thread = threading.Thread(target=self._run, name="ntfy-replies", daemon=True)
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()

    def _run(self) -> None:
        while not self._stop.is_set():
            try:
                self.stream_once()
            except Exception as exc:
                log.warning("Replies: ntfy stream error (%s); reconnecting in %.0fs", exc, self.backoff_s)
            self.connected = False
            self._stop.wait(self.backoff_s)

    def stream_once(self) -> None:
        with self._client.stream("GET", self.url, params={"since": self._since}) as r:
            r.raise_for_status()
            self.connected = True
            log.info("Replies: listening on ntfy topic %s", self.cfg.reply_topic)
            for line in r.iter_lines():
                if self._stop.is_set():
                    return
                self.handle_line(line)

    def handle_line(self, line: str) -> str | None:
        if not line.strip():
            return None
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            return None
        if msg.get("event") != "message":
            return None  # open / keepalive
        if msg.get("id"):
            self._since = msg["id"]  # resume after this message on reconnect
        cmd = parse_command(msg.get("message", ""))
        if not cmd:
            log.info("Replies: ignoring unrecognized message on reply topic")
            return None
        self.received += 1
        return self.handler(*cmd)
