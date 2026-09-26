import hashlib
import json

import httpx
import pytest

from prusa_watch.prusalink import PrusaLink, PrusaLinkError

STATUS = {
    "job": {"id": 42, "progress": 12.0, "time_remaining": 3600, "time_printing": 600},
    "printer": {"state": "PRINTING", "temp_nozzle": 215.1, "temp_bed": 60.0},
}


class FakeBuddy:
    """Mimics Buddy firmware's PrusaLink: digest auth (user 'maker') + X-Api-Key."""

    REALM = "Printer API"
    NONCE = "abc123"

    def __init__(self, password="secret"):
        self.password = password
        self.calls = []

    def _digest_ok(self, request: httpx.Request) -> bool:
        auth = request.headers.get("authorization", "")
        if not auth.startswith("Digest "):
            return False
        parts = dict(
            p.strip().split("=", 1) for p in auth[len("Digest "):].split(",")
        )
        parts = {k: v.strip('"') for k, v in parts.items()}
        ha1 = hashlib.md5(f"maker:{self.REALM}:{self.password}".encode()).hexdigest()
        ha2 = hashlib.md5(f"{request.method}:{parts['uri']}".encode()).hexdigest()
        if "qop" in parts:
            expected = hashlib.md5(f"{ha1}:{parts['nonce']}:{parts['nc']}:{parts['cnonce']}:{parts['qop']}:{ha2}".encode()).hexdigest()
        else:
            expected = hashlib.md5(f"{ha1}:{parts['nonce']}:{ha2}".encode()).hexdigest()
        return parts.get("username") == "maker" and parts.get("response") == expected

    def __call__(self, request: httpx.Request) -> httpx.Response:
        authed = request.headers.get("x-api-key") == self.password or self._digest_ok(request)
        if not authed:
            return httpx.Response(
                401, headers={"WWW-Authenticate": f'Digest realm="{self.REALM}", nonce="{self.NONCE}", qop="auth"'}
            )
        self.calls.append((request.method, request.url.path))
        if request.method == "GET" and request.url.path == "/api/v1/status":
            return httpx.Response(200, json=STATUS)
        if request.method == "GET" and request.url.path == "/api/v1/job":
            return httpx.Response(200, json={"id": 42, "file": {"display_name": "benchy.bgcode"}})
        if request.method == "PUT" and request.url.path in ("/api/v1/job/42/pause", "/api/v1/job/42/resume"):
            return httpx.Response(204)
        if request.method == "DELETE" and request.url.path == "/api/v1/job/42":
            return httpx.Response(204)
        if request.url.path.startswith("/api/v1/job/"):
            return httpx.Response(404, json={"title": "Not Found"})
        return httpx.Response(404)


def client(fake, **kw):
    return PrusaLink("192.168.1.50", fake.password, transport=httpx.MockTransport(fake), **kw)


def test_digest_auth_status_and_job():
    fake = FakeBuddy()
    pl = client(fake)
    st = pl.status()
    assert st.state == "PRINTING" and st.job_id == 42 and st.progress == 12.0
    assert pl.job_name() == "benchy.bgcode"


def test_apikey_auth():
    fake = FakeBuddy()
    pl = client(fake, auth="apikey")
    assert pl.status().state == "PRINTING"


def test_wrong_password_raises_clear_error():
    fake = FakeBuddy()
    pl = PrusaLink("192.168.1.50", "nope", transport=httpx.MockTransport(fake))
    with pytest.raises(PrusaLinkError, match="401"):
        pl.status()


def test_pause_resume_stop_hit_correct_endpoints():
    fake = FakeBuddy()
    pl = client(fake)
    pl.pause(42)
    pl.resume(42)
    pl.stop(42)
    assert ("PUT", "/api/v1/job/42/pause") in fake.calls
    assert ("PUT", "/api/v1/job/42/resume") in fake.calls
    assert ("DELETE", "/api/v1/job/42") in fake.calls


def test_wrong_job_id_raises():
    pl = client(FakeBuddy())
    with pytest.raises(PrusaLinkError, match="404"):
        pl.pause(7)


def test_network_error_wrapped():
    def boom(request):
        raise httpx.ConnectError("no route to host")

    pl = PrusaLink("192.168.1.50", "x", transport=httpx.MockTransport(boom))
    with pytest.raises(PrusaLinkError, match="no route"):
        pl.status()
