"""Exercise the shipped container, real inference and readiness without printer hardware."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Printer(BaseHTTPRequestHandler):
    available = True

    def do_GET(self):
        if not self.available:
            self.send_error(503)
            return
        body = {"printer": {"state": "PRINTING"}, "job": {"id": 1, "progress": 10, "time_printing": 100}}
        if self.path.endswith("/job"):
            body = {"id": 1, "file": {"display_name": "simulated.bgcode"}}
        encoded = json.dumps(body).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def log_message(self, *_):
        pass


server = ThreadingHTTPServer(("127.0.0.1", 0), Printer)
threading.Thread(target=server.serve_forever, daemon=True).start()
name = "prusa-watch-smoke"
web_port = 8484


def get(path, method="GET"):
    request = urllib.request.Request(f"http://127.0.0.1:{web_port}{path}", method=method,
                                     headers={"X-Token": "smoke-test-control"})
    try:
        with urllib.request.urlopen(request, timeout=3) as response:
            return response.status, json.load(response)
    except urllib.error.HTTPError as error:
        return error.code, json.load(error)


with tempfile.TemporaryDirectory() as directory:
    root = Path(directory)
    subprocess.run(["ffmpeg", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=5",
                    "-t", "3", "-c:v", "mjpeg", str(root / "clip.avi")], check=True)
    config = {"printer": {"host": f"127.0.0.1:{server.server_port}", "password": "test", "auth": "apikey",
                          "poll_interval_s": 0.1},
              "camera": {"url": "/smoke/clip.avi"}, "detector": {"interval_s": 0.2},
              "web": {"port": web_port, "token": "smoke-test-control"}, "state_dir": "/smoke/data",
              "escalation": {"default_policy": "watch_only", "schedules": []}}
    (root / "config.json").write_text(json.dumps(config))
    # Run as the invoking user so files written under /smoke stay removable by the temp-dir cleanup.
    subprocess.run(["docker", "run", "-d", "--name", name, "--network", "host", "--user", f"{os.getuid()}:{os.getgid()}",
                    "-v", f"{root}:/smoke",
                    "-e", "PRUSA_WATCH_CONFIG=/smoke/config.json", sys.argv[1]], check=True)
    try:
        deadline = time.monotonic() + 90
        while True:
            try:
                status, _ = get("/healthz")
                if status == 200:
                    break
            except (OSError, ValueError):
                pass
            if time.monotonic() >= deadline:
                raise AssertionError("container never became ready")
            time.sleep(0.2)
        status, state = get("/api/state")
        assert status == 200 and state["frame_num"] >= 1 and state["job"]["action_taken"] is None
        status, result = get("/api/test", "POST")
        assert status == 200 and isinstance(result["detections"], list)
        Printer.available = False
        deadline = time.monotonic() + 5
        while get("/healthz")[0] != 503:
            assert time.monotonic() < deadline, "readiness missed the failed printer"
            time.sleep(0.1)
        assert get("/livez")[0] == 200
        print("Container inference, readiness, failure detection and liveness passed")
    except BaseException:
        subprocess.run(["docker", "logs", name], check=False)
        raise
    finally:
        subprocess.run(["docker", "stop", "--timeout", "20", name], check=False)
        subprocess.run(["docker", "rm", "-f", name], check=False)
        server.shutdown()
