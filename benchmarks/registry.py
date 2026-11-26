"""Time a contextual CLI merge against an isolated registry of independent packages."""
import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path


class Registry(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    # Flush headers and body together at the end of each loopback request.
    wbufsize = -1

    def log_message(self, *args):
        pass

    def do_GET(self):
        name = self.path.lstrip("/")
        if name not in dependencies:
            self.send_error(404)
            return
        requests.append(name)
        version = dependencies[name]
        manifest = {"name": name, "version": version, "dist": {
            "integrity": "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=",
            "tarball": f"{registry}/{name}/-/{name}-{version}.tgz",
        }}
        payload = json.dumps({"name": name, "dist-tags": {"latest": version},
                              "time": {version: "2020-01-01T00:00:00Z"}, "versions": {version: manifest}}).encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


binary = str(Path(shutil.which(sys.argv[1]) or sys.argv[1]).resolve())
dependencies = {f"fixture-{i}": "1.0.0" for i in range(int(sys.argv[2]))}
requests = []
with tempfile.TemporaryDirectory() as directory, HTTPServer(("127.0.0.1", 0), Registry) as server:
    root = Path(directory)
    registry = f"http://127.0.0.1:{server.server_port}"
    config = {"registry": registry, "minimumReleaseAge": 0}
    (root / "lock").write_text(json.dumps({"lockfileVersion": "9.0", "importers": {".": {}}}))
    (root / "context").write_text(json.dumps({"target": "ours", "manifests": {".": {"dependencies": dependencies}},
                                              "configuration": config, "configurations": [config] * 3, "patches": {}}))
    threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.01}, daemon=True).start()
    command = [binary, "merge", "lock", "lock", "lock", "--context", "context", "--allow-network"]
    env = {"PATH": os.environ["PATH"], "HOME": directory, "NO_PROXY": "127.0.0.1"}
    samples = 1
    try:
        if sys.argv[3:] == ["--hyperfine"]:
            samples = 13  # Two warmups and eleven measured runs.
            subprocess.run(["hyperfine", "--shell=none", "--warmup", "2", "--runs", "11",
                            shlex.join(command)], cwd=root, env=env, check=True)
        else:
            start = time.perf_counter_ns()
            subprocess.run(command, cwd=root, stdout=subprocess.DEVNULL, env=env, check=True)
            print((time.perf_counter_ns() - start) / 1_000_000)
    finally:
        server.shutdown()
    if sorted(requests) != sorted(list(dependencies) * samples):
        raise ValueError("each missing package must be requested once per merge")
