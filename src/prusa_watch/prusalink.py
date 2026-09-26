"""Minimal PrusaLink v1 client for Buddy firmware (Core One / MK4 / XL).

Auth: PrusaLink on Buddy firmware uses HTTP Digest with username ``maker`` and
the password shown on the printer under Settings > Network > PrusaLink. The
same secret is also accepted as an ``X-Api-Key`` header (what PrusaSlicer
uses), selectable via ``printer.auth: apikey``.

Endpoints used (spec: prusa3d/Prusa-Link-Web spec/openapi.yaml):
  GET    /api/v1/status              printer state + current job id
  GET    /api/v1/job                 job details (file name, progress)
  PUT    /api/v1/job/{id}/pause
  PUT    /api/v1/job/{id}/resume
  DELETE /api/v1/job/{id}            stop
"""

from __future__ import annotations

import logging
from dataclasses import dataclass

import httpx

log = logging.getLogger(__name__)


class PrusaLinkError(RuntimeError):
    pass


@dataclass
class PrinterStatus:
    state: str  # IDLE, BUSY, PRINTING, PAUSED, FINISHED, STOPPED, ERROR, ATTENTION, READY
    job_id: int | None
    progress: float | None
    time_printing: int | None
    temp_nozzle: float | None
    temp_bed: float | None
    raw: dict


class PrusaLink:
    def __init__(
        self,
        host: str,
        password: str,
        username: str = "maker",
        auth: str = "digest",
        scheme: str = "http",
        timeout_s: float = 5.0,
        transport: httpx.BaseTransport | None = None,
    ):
        base = host if host.startswith(("http://", "https://")) else f"{scheme}://{host}"
        headers = {"Accept": "application/json"}
        client_auth = None
        if auth == "apikey":
            headers["X-Api-Key"] = password
        else:
            client_auth = httpx.DigestAuth(username, password)
        self._client = httpx.Client(
            base_url=base.rstrip("/"), headers=headers, auth=client_auth, timeout=timeout_s, transport=transport
        )

    def close(self) -> None:
        self._client.close()

    def _request(self, method: str, path: str) -> httpx.Response:
        try:
            r = self._client.request(method, path)
        except httpx.HTTPError as exc:
            raise PrusaLinkError(f"{method} {path}: {exc}") from exc
        if r.status_code == 401:
            raise PrusaLinkError(f"{method} {path}: 401 Unauthorized (check printer.password / printer.auth)")
        if r.status_code >= 400:
            raise PrusaLinkError(f"{method} {path}: HTTP {r.status_code} {r.text[:200]}")
        return r

    def status(self) -> PrinterStatus:
        data = self._request("GET", "/api/v1/status").json()
        printer = data.get("printer", {}) or {}
        job = data.get("job", {}) or {}
        return PrinterStatus(
            state=str(printer.get("state", "UNKNOWN")).upper(),
            job_id=job.get("id"),
            progress=job.get("progress"),
            time_printing=job.get("time_printing"),
            temp_nozzle=printer.get("temp_nozzle"),
            temp_bed=printer.get("temp_bed"),
            raw=data,
        )

    def job(self) -> dict | None:
        r = self._request("GET", "/api/v1/job")
        if r.status_code == 204 or not r.content:
            return None
        return r.json()

    def job_name(self) -> str | None:
        try:
            j = self.job()
        except PrusaLinkError:
            return None
        if not j:
            return None
        f = j.get("file") or {}
        return f.get("display_name") or f.get("name")

    def pause(self, job_id: int) -> None:
        self._request("PUT", f"/api/v1/job/{job_id}/pause")
        log.warning("PrusaLink: paused job %s", job_id)

    def resume(self, job_id: int) -> None:
        self._request("PUT", f"/api/v1/job/{job_id}/resume")
        log.info("PrusaLink: resumed job %s", job_id)

    def stop(self, job_id: int) -> None:
        self._request("DELETE", f"/api/v1/job/{job_id}")
        log.warning("PrusaLink: stopped job %s", job_id)
