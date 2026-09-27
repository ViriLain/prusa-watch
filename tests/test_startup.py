"""Startup ergonomics: .env loading, model download, CLI wiring."""

import os

import httpx
import pytest

from prusa_watch import __main__ as cli
from prusa_watch import model as model_mod
from prusa_watch.dotenv import load_dotenv, parse_dotenv
from prusa_watch.model import ModelDownloadError, download_model


@pytest.fixture
def clean_env():
    saved = dict(os.environ)
    try:
        yield os.environ
    finally:
        os.environ.clear()
        os.environ.update(saved)


# ---------------------------------------------------------------- .env parsing
def test_parse_dotenv_subset():
    text = """
# comment
PRUSALINK_PASSWORD=abc123
export NTFY_TOPIC = my-topic   # trailing comment
QUOTED="has # hash and spaces"
SINGLE='x=y'
URL=http://192.168.1.10:8484
EMPTY=
not a line
"""
    assert parse_dotenv(text) == {
        "PRUSALINK_PASSWORD": "abc123",
        "NTFY_TOPIC": "my-topic",
        "QUOTED": "has # hash and spaces",
        "SINGLE": "x=y",
        "URL": "http://192.168.1.10:8484",
        "EMPTY": "",
    }


def test_load_dotenv_shell_wins_empty_skipped_first_file_wins(tmp_path):
    a = tmp_path / "a.env"
    b = tmp_path / "b.env"
    a.write_text("ONE=from-a\nTWO=from-a\nEMPTY=\n")
    b.write_text("TWO=from-b\nTHREE=from-b\n")
    env = {"ONE": "from-shell"}
    loaded = load_dotenv([a, b, tmp_path / "missing.env", a], env)
    assert loaded == [a.resolve(), b.resolve()]
    assert env == {"ONE": "from-shell", "TWO": "from-a", "THREE": "from-b"}


# ---------------------------------------------------------------- model download
def _transport(body: bytes, status: int = 200):
    def handler(request):
        return httpx.Response(status, content=body, headers={"Content-Length": str(len(body))})

    return httpx.MockTransport(handler)


def test_download_model_writes_atomically(tmp_path):
    out = tmp_path / "models" / "m.onnx"
    download_model(out, transport=_transport(b"x" * 2_000_000), progress=None, verify_onnx=False)
    assert out.stat().st_size == 2_000_000
    assert not list(out.parent.glob("*.part"))


def test_download_model_noop_when_present(tmp_path):
    out = tmp_path / "m.onnx"
    out.write_bytes(b"existing")

    def boom(request):
        raise AssertionError("should not download")

    download_model(out, transport=httpx.MockTransport(boom), progress=None)
    assert out.read_bytes() == b"existing"


@pytest.mark.parametrize("body,status", [(b"<html>nope</html>", 200), (b"denied", 403)])
def test_download_model_rejects_bad_responses(tmp_path, body, status):
    out = tmp_path / "m.onnx"
    with pytest.raises(ModelDownloadError):
        download_model(out, transport=_transport(body, status), progress=None, verify_onnx=False)
    assert not out.exists()
    assert not list(tmp_path.glob("*.part"))


def test_download_model_rejects_non_onnx(tmp_path):
    pytest.importorskip("onnxruntime")
    out = tmp_path / "m.onnx"
    with pytest.raises(ModelDownloadError, match="not a loadable ONNX"):
        download_model(out, transport=_transport(b"\0" * 2_000_000), progress=None)
    assert not out.exists()


# ---------------------------------------------------------------- CLI
def test_env_file_next_to_config_feeds_config(tmp_path, capsys, clean_env):
    clean_env.pop("PW_TEST_PASSWORD", None)
    (tmp_path / "config.yaml").write_text(
        "printer: {host: 192.168.1.50, password: '${PW_TEST_PASSWORD}'}\ncamera: {url: 'rtsp://192.168.1.51/live'}\n"
    )
    (tmp_path / ".env").write_text("PW_TEST_PASSWORD=s3cret\n")
    rc = cli.main(["config", "-c", str(tmp_path / "config.yaml"), "--show-secrets"])
    out = capsys.readouterr()
    assert rc == 0
    assert "password: s3cret" in out.out
    assert "loaded" in out.err and ".env" in out.err


def test_shell_env_overrides_env_file(tmp_path, capsys, clean_env):
    clean_env["PW_TEST_PASSWORD"] = "from-shell"
    (tmp_path / "config.yaml").write_text(
        "printer: {host: h, password: '${PW_TEST_PASSWORD}'}\ncamera: {url: 'rtsp://c/live'}\n"
    )
    (tmp_path / ".env").write_text("PW_TEST_PASSWORD=from-file\n")
    cli.main(["config", "-c", str(tmp_path / "config.yaml"), "--show-secrets"])
    assert "password: from-shell" in capsys.readouterr().out


def test_missing_config_gives_a_hint(tmp_path, capsys, clean_env):
    rc = cli.main(["check", "-c", str(tmp_path / "nope.yaml")])
    assert rc == 2
    assert "cp config.example.yaml" in capsys.readouterr().err


def test_fetch_model_uses_configured_path(tmp_path, monkeypatch, clean_env):
    target = tmp_path / "weights" / "w.onnx"
    (tmp_path / "config.yaml").write_text(f"detector: {{model_path: '{target}'}}\n")
    calls = []

    def fake_download(path, force=False, **_):
        calls.append((str(path), force))
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(b"m")

    monkeypatch.setattr(model_mod, "download_model", fake_download)
    assert cli.main(["fetch-model", "-c", str(tmp_path / "config.yaml")]) == 0
    assert calls == [(str(target), False)]
    # present -> no second download unless --force
    assert cli.main(["fetch-model", "-c", str(tmp_path / "config.yaml")]) == 0
    assert len(calls) == 1
    assert cli.main(["fetch-model", "-c", str(tmp_path / "config.yaml"), "--force"]) == 0
    assert calls[-1] == (str(target), True)


def test_ensure_model_reports_failure(tmp_path, monkeypatch, capsys):
    def fail(path, force=False, **_):
        raise ModelDownloadError("model download failed: boom")

    monkeypatch.setattr(model_mod, "download_model", fail)
    assert cli._ensure_model(str(tmp_path / "m.onnx")) is False
    assert "prusa-watch fetch-model" in capsys.readouterr().err
