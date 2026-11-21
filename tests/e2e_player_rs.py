#!/usr/bin/env python3
"""End-to-end test: real Flask backend (app.py) on loopback + the compiled
native Rust player (player-rs/target/release/fossignage-player) running with
the headless display backend.

Verifies the whole wire path with the actual server:
  classic mode : register -> link -> upload -> playlist -> activate -> play
                 -> --unlink
  standalone   : register (auto-linked) -> playlist -> play

Run from the repo root (builds nothing; expects the release binary, CI builds
it first):
    python3 tests/e2e_player_rs.py
"""

import json
import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
import urllib.request
import zlib

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BINARY = os.path.join(REPO, "player-rs", "target", "release", "fossignage-player")


def log(msg):
    print(f"[e2e] {msg}", flush=True)


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def png_solid(w, h, rgb):
    """Minimal solid-color PNG encoder (stdlib only)."""
    def chunk(tag, data):
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
    raw = b"".join(b"\x00" + bytes(rgb) * w for _ in range(h))
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw))
            + chunk(b"IEND", b""))


def http_json(url, payload=None, method=None):
    data = json.dumps(payload).encode() if payload is not None else None
    req = urllib.request.Request(url, data=data, method=method or ("POST" if data else "GET"))
    if data:
        req.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(req, timeout=10) as resp:
        return json.loads(resp.read().decode())


def upload_png(server, name, w, h, rgb):
    boundary = "----fossignagee2e"
    png = png_solid(w, h, rgb)
    body = (
        f"--{boundary}\r\n"
        f'Content-Disposition: form-data; name="name"\r\n\r\n{name}\r\n'
        f"--{boundary}\r\n"
        f'Content-Disposition: form-data; name="file"; filename="{name}.png"\r\n'
        f"Content-Type: image/png\r\n\r\n"
    ).encode() + png + f"\r\n--{boundary}--\r\n".encode()
    req = urllib.request.Request(server + "/api/upload_media", data=body, method="POST")
    req.add_header("Content-Type", f"multipart/form-data; boundary={boundary}")
    with urllib.request.urlopen(req, timeout=15) as resp:
        return json.loads(resp.read().decode())["media"]


class Server:
    def __init__(self, workdir, standalone):
        self.dir = os.path.join(workdir, "server-sa" if standalone else "server")
        os.makedirs(self.dir)
        for f in ("app.py", "requirements.txt"):
            shutil.copy(os.path.join(REPO, f), self.dir)
        shutil.copytree(os.path.join(REPO, "templates"), os.path.join(self.dir, "templates"))
        self.port = free_port()
        self.url = f"http://127.0.0.1:{self.port}"
        env = dict(os.environ, SIGNAGE_HOST="127.0.0.1", SIGNAGE_PORT=str(self.port))
        args = [sys.executable, "app.py"]
        if standalone:
            args.append("--standalone")
        self.proc = subprocess.Popen(args, cwd=self.dir, env=env,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        deadline = time.time() + 15
        while time.time() < deadline:
            try:
                http_json(self.url + "/api/state")
                return
            except Exception:
                time.sleep(0.2)
        raise RuntimeError("server did not start")

    def stop(self):
        self.proc.terminate()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def run_player(server_url, workdir, name, standalone=False, extra=(), seconds=4.0,
               code_file=None):
    pdir = os.path.join(workdir, name)
    os.makedirs(pdir, exist_ok=True)
    frames = os.path.join(pdir, "frames")
    logs = os.path.join(pdir, "logs")
    code_file = code_file or os.path.join(pdir, "display_code")
    args = [BINARY, "--server", server_url, "--backend", "headless",
            "--out-dir", frames, "--width", "64", "--height", "48",
            "--poll-interval", "0.2", "--code-file", code_file,
            "--log-dir", logs, "--cache-dir", os.path.join(pdir, "cache")]
    if standalone:
        args.append("--standalone")
    args += list(extra)
    proc = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(seconds)
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        raise RuntimeError("player did not exit on SIGTERM")
    logfile = os.path.join(logs, "player.log")
    text = open(logfile).read() if os.path.exists(logfile) else ""
    return {"code_file": code_file, "frames": frames, "log": text}


def frame_count(frames):
    if not os.path.isdir(frames):
        return 0
    return len([f for f in os.listdir(frames) if f.startswith("frame_")])


def latest_ppm_pixel(frames, w=64, h=48):
    path = os.path.join(frames, "latest.ppm")
    data = open(path, "rb").read()
    header_end = data.index(b"255\n") + 4
    body = data[header_end:]
    i = ((h // 2) * w + w // 2) * 3
    return tuple(body[i:i + 3])


def test_classic_flow(workdir):
    log("classic mode: register/link/upload/playlist/play/unlink")
    srv = Server(workdir, standalone=False)
    try:
        media = upload_png(srv.url, "red", 16, 12, (200, 30, 40))
        assert media["type"] == "image", media
        pl = http_json(srv.url + "/api/save_playlist",
                       {"name": "e2e", "items": [{"media_id": media["id"], "duration": 5}]})["playlist"]

        res = run_player(srv.url, workdir, "classic", seconds=4.0)
        code = open(res["code_file"]).read().strip()
        assert len(code) == 4, f"code file: {code!r}"
        assert "registered as display" in res["log"], res["log"]
        # not linked yet -> no playback
        assert "playing [" not in res["log"], res["log"]
        assert frame_count(res["frames"]) >= 1  # idle card was rendered

        # link + activate, player picks it up on the next poll
        http_json(srv.url + "/api/link_display", {"code": code})
        http_json(srv.url + "/api/activate_playlist", {"code": code, "playlist_id": pl["id"]})
        res = run_player(srv.url, workdir, "classic2", seconds=3.5,
                         code_file=res["code_file"])
        assert "playing [1/1] red (image)" in res["log"], res["log"]
        assert frame_count(res["frames"]) >= 1
        px = latest_ppm_pixel(res["frames"])
        assert px[0] > 170 and px[1] < 70 and px[2] < 80, f"frame pixel {px}"

        # the display is seen as online by the operator API
        state = http_json(srv.url + "/api/state")
        disp = next(d for d in state["displays"] if d["code"] == code)
        assert disp["linked"] and disp["online"], disp

        # unlink via the player CLI
        out = subprocess.run([BINARY, "--server", srv.url, "--unlink",
                              "--code-file", res["code_file"]],
                             capture_output=True, text=True, timeout=15)
        assert out.returncode == 0, out.stderr
        poll = http_json(srv.url + f"/api/display/{code}")
        assert poll["linked"] is False, poll
        log("classic flow OK")
    finally:
        srv.stop()


def test_standalone_flow(workdir):
    log("standalone mode: auto-linked playback")
    srv = Server(workdir, standalone=True)
    try:
        media = upload_png(srv.url, "blue", 16, 12, (30, 40, 220))
        pl = http_json(srv.url + "/api/save_playlist",
                       {"name": "sa", "items": [{"media_id": media["id"], "duration": 5}]})["playlist"]
        http_json(srv.url + "/api/set_active_playlist", {"playlist_id": pl["id"]})
        res = run_player(srv.url, workdir, "standalone", standalone=True, seconds=3.5)
        assert "registered as display" in res["log"], res["log"]
        assert "playing [1/1] blue (image)" in res["log"], res["log"]
        px = latest_ppm_pixel(res["frames"])
        assert px[2] > 190 and px[0] < 70, f"frame pixel {px}"
        log("standalone flow OK")
    finally:
        srv.stop()


def main():
    if not os.path.exists(BINARY):
        print(f"player binary not found at {BINARY}; run `cargo build --release` in player-rs first",
              file=sys.stderr)
        return 2
    workdir = tempfile.mkdtemp(prefix="fossignage-e2e-")
    try:
        test_classic_flow(workdir)
        test_standalone_flow(workdir)
        log("ALL E2E TESTS PASSED")
        return 0
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
