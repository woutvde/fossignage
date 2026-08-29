#!/usr/bin/env python3
"""
Fossignage native display player.

A low-resource, dependency-free (stdlib only) player for single-system
installs (e.g. a Raspberry Pi running Raspbian Lite). It talks to the same
Flask backend (app.py) that the browser display client uses:

  - POST /api/display/register   (pair / reconnect with a 4-char code)
  - GET  /api/display/<code>     (poll playlist + heartbeat)
  - POST /api/unlink_display     (optional, via --unlink)

Playback is delegated to native processes so there is no browser overhead:

  - video : omxplayer (Pi HW accel) -> vlc -> ffplay
  - image : feh (X) -> fbi (console framebuffer)
  - url   : chromium --kiosk (optional, heavy)

The pairing code is persisted to a file so the display survives reboots
without re-pairing, mirroring the browser client's localStorage behaviour.

Usage:
  python3 player.py [--server http://127.0.0.1:5000] [--code-file PATH]
                    [--poll-interval 2] [--unlink] [--debug]
"""

import argparse
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

# ---------------------------------------------------------------- defaults

DEFAULT_SERVER = os.environ.get("SIGNAGE_SERVER", "http://127.0.0.1:5000")
DEFAULT_CODE_FILE = os.environ.get("SIGNAGE_CODE_FILE",
                                   os.path.expanduser("~/.fossignage_display_code"))
DEFAULT_LOG_DIR = os.environ.get("SIGNAGE_LOG_DIR", "/var/log/fossignage")
IMAGE_EXTS = (".png", ".jpg", ".jpeg", ".webp")
ANIMATED_EXTS = (".gif",)  # need a video-style player to animate
VIDEO_EXTS = (".mp4", ".webm", ".ogg", ".mov", ".mkv", ".avi")


def setup_logging(log_dir=DEFAULT_LOG_DIR):
    """Log to <log_dir>/player.log, falling back to stderr if unavailable
    (e.g. no permission). Keeps the last ~1 MB per file, one rotated copy."""
    global _LOG_PATH
    try:
        os.makedirs(log_dir, exist_ok=True)
        _LOG_PATH = os.path.join(log_dir, "player.log")
    except OSError:
        _LOG_PATH = None


def _rotate_if_needed(path, max_bytes=1024 * 1024):
    try:
        if os.path.exists(path) and os.path.getsize(path) > max_bytes:
            os.replace(path, path + ".1")
    except OSError:
        pass


def log(msg):
    line = f"[player] {time.strftime('%Y-%m-%d %H:%M:%S')} {msg}"
    print(line, flush=True)
    if _LOG_PATH:
        try:
            _rotate_if_needed(_LOG_PATH)
            with open(_LOG_PATH, "a", encoding="utf-8") as f:
                f.write(line + "\n")
        except OSError:
            pass


_LOG_PATH = None


def get_lan_ip():
    """Best-effort local network IP (no traffic actually sent)."""
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.connect(("8.8.8.8", 80))  # never sends packets; just picks a route
        ip = s.getsockname()[0]
        s.close()
        return ip
    except OSError:
        return "127.0.0.1"


def http_json(url, payload=None, timeout=5):
    """GET (payload=None) or POST JSON, returning parsed JSON. Raises on failure."""
    data = json.dumps(payload).encode() if payload is not None else None
    req = urllib.request.Request(url, data=data, method="POST" if data else "GET")
    if data:
        req.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode())


# ---------------------------------------------------------------- players

def find_binary(*names):
    for name in names:
        path = shutil.which(name)
        if path:
            return path
    return None


class NativePlayer:
    """Owns the subprocess (or timer) currently rendering an item."""

    def __init__(self, server, standalone=False):
        self.server = server
        self.standalone = standalone
        self.proc = None          # subprocess currently on screen
        self.timer = None         # threading.Timer for image/url durations
        self._lock = threading.Lock()

    # -- lifecycle ---------------------------------------------------------

    def stop(self):
        with self._lock:
            if self.timer:
                self.timer.cancel()
                self.timer = None
            if self.proc:
                try:
                    # Terminate the whole process group (omxplayer spawns children)
                    os.killpg(os.getpgid(self.proc.pid), signal.SIGTERM)
                except (ProcessLookupError, PermissionError, OSError):
                    try:
                        self.proc.terminate()
                    except OSError:
                        pass
                try:
                    self.proc.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    try:
                        os.killpg(os.getpgid(self.proc.pid), signal.SIGKILL)
                    except OSError:
                        pass
                self.proc = None

    def _start(self, cmd, wait_for_exit=False, env=None):
        log("exec: " + " ".join(cmd))
        kwargs = dict(stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                      start_new_session=True)
        if env:
            kwargs["env"] = env
        try:
            self.proc = subprocess.Popen(cmd, **kwargs)
        except OSError as e:
            log(f"failed to launch {cmd[0]}: {e}")
            self.proc = None
            return False
        if wait_for_exit:
            threading.Thread(target=self._wait_video, args=(self.proc,),
                             daemon=True).start()
        return True

    def _wait_video(self, proc):
        """Advance the playlist when the video process exits naturally."""
        proc.wait()
        with self._lock:
            if self.proc is proc:  # not superseded by a stop()/new item
                self.proc = None
        # If the player died almost instantly it failed to play the file
        # (bad flag, unsupported format, no X connection...). Log it loudly
        # and advance anyway so one broken item can't stall the playlist.
        if proc.returncode not in (0, None):
            log(f"player exited with code {proc.returncode}")
        self.on_video_end()

    # -- rendering ---------------------------------------------------------

    def show_idle(self, code):
        """Full-screen status card: pairing code, or server address in
        standalone mode."""
        self.stop()
        if self.standalone:
            # Replace loopback with the machine's LAN IP so the address is
            # actually usable from another computer.
            from urllib.parse import urlparse
            parsed = urlparse(self.server)
            host = parsed.hostname
            if host in ("127.0.0.1", "localhost", "0.0.0.0"):
                port = parsed.port or 80
                display_addr = f"http://{get_lan_ip()}:{port}"
            else:
                display_addr = self.server
            img = self._render_idle_image(None, display_addr)
        else:
            img = self._render_idle_image(code)
        if not img:
            if self.standalone:
                log(f"idle: waiting for content (server {self.server})")
            else:
                log(f"idle: waiting for pairing (code {code})")
            return
        feh = find_binary("feh")
        fbi = find_binary("fbi")
        if feh:
            self._start([feh, "-F", "-Z", "-Y", "--hide-pointer", img])
        elif fbi:
            env = dict(os.environ, TTY="/dev/tty1")
            try:
                self.proc = subprocess.Popen(
                    [fbi, "-T", "1", "-d", "/dev/fb0", "-noverbose", "-a", img],
                    env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                    start_new_session=True)
            except OSError:
                self.proc = None
        else:
            log(f"idle (no image viewer found): code {code}")

    def _render_idle_image(self, code, display_addr=None):
        """Render a dark card with the pairing code (or server address in
        standalone mode) using ImageMagick if present."""
        convert = find_binary("convert", "magick")
        if not convert:
            return None
        path = os.path.join(tempfile.gettempdir(), "fossignage_idle.png")
        if code:
            heading = "Fossignage Display - enter this code on the Operator Console:"
            big_text = code
            big_size = 220
        else:
            heading = "Fossignage Standalone Display - upload content at:"
            big_text = display_addr or self.server
            # Shrink long URLs so they fit on screen (1920px wide card)
            big_size = max(48, min(160, int(1700 * 1.9 / max(len(big_text), 1))))
        cmd = [
            convert,
            "-size", "1920x1080", "xc:#0f172a",
            "-font", "DejaVu-Sans", "-pointsize", "40", "-fill", "#94a3b8",
            "-gravity", "center", "-annotate", "+0-160", heading,
            "-pointsize", str(big_size), "-fill", "#38bdf8",
            "-annotate", "+0+40", big_text,
            path,
        ]
        try:
            subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                           timeout=10, check=True)
            return path
        except (subprocess.SubprocessError, OSError):
            return None

    def show_item(self, item, single_item_playlist):
        url = item.get("url", "")
        mtype = item.get("type", "")
        duration = int(item.get("duration") or 8)
        self.stop()

        if mtype == "video" or url.lower().endswith(VIDEO_EXTS + ANIMATED_EXTS):
            # Animated GIFs go through the video path too: feh/fbi only show
            # the first frame, while mpv/vlc/ffplay animate them properly.
            self._play_video(url, loop=single_item_playlist)
        elif url.lower().endswith(IMAGE_EXTS):
            self._show_image(url, duration)
        else:
            self._show_url(url, duration)

    def _play_video(self, url, loop):
        full_url = url if url.startswith("http") else self.server + url
        omx = find_binary("omxplayer")
        vlc = find_binary("vlc")
        mpv = find_binary("mpv")
        ffplay = find_binary("ffplay")
        if omx:
            cmd = [omx, "--no-osd", "--blank", "-o", "both", full_url]
            if loop:
                cmd.insert(1, "--loop")
        elif vlc:
            # VLC first. On bare X (no WM) VLC 3 cannot embed its video
            # window normally ("parent window not available"), so we use
            # wallpaper mode: draw the video directly onto the root window.
            # --width/--height + autoscale keep the video fitted to screen.
            w, h = self._screen_size()
            env = dict(os.environ,
                       DISPLAY=os.environ.get("DISPLAY", ":0"),
                       XDG_RUNTIME_DIR="/tmp/xdg")
            os.makedirs("/tmp/xdg", exist_ok=True)
            cmd = [vlc, "--intf", "dummy", "--no-osd", "--no-video-title-show",
                   "--no-mouse-events", "--play-and-exit",
                   "--no-autocrop", "--crop=none",
                   "--aspect-ratio=default",
                   "--video-wallpaper",
                   "--autoscale",
                   f"--width={w}", f"--height={h}"]
            if loop:
                cmd.append("--loop")
            cmd.append(full_url)
            return self._start(cmd, wait_for_exit=True, env=env)
        elif mpv:
            cmd = [mpv, "--no-border", "--fullscreen", "--no-osd-bar",
                   "--cursor-autohide=no",
                   # stretch to fill the screen regardless of aspect ratio
                   "--panscan=1.0",
                   "--loop-file=" + ("inf" if loop else "no"),
                   "--really-quiet", full_url]
        elif ffplay:
            cmd = [ffplay, "-noborder", "-alwaysontop", "-autoexit",
                   "-loglevel", "quiet", "-window_title", "fossignage",
                   "-fs", "-left", "0", "-top", "0"]
            if loop:
                cmd.append("-loop")
                cmd.append("0")
            cmd.append(full_url)
        else:
            log("no video player found (install omxplayer, vlc or ffmpeg)")
            self.on_video_end()
            return
        self._start(cmd, wait_for_exit=True)

    def _show_image(self, url, duration):
        local = self._ensure_local(url)
        if not local:
            self.on_video_end()
            return
        feh = find_binary("feh")
        fbi = find_binary("fbi")
        if feh:
            ok = self._start([feh, "-F", "-Z", "-Y", "--hide-pointer", local])
        elif fbi:
            ok = self._start([fbi, "-T", "1", "-d", "/dev/fb0", "-noverbose",
                              "-a", local])
        else:
            log("no image viewer found (install feh or fbi)")
            ok = False
        if ok:
            self.timer = threading.Timer(duration, self.on_video_end)
            self.timer.daemon = True
            self.timer.start()

    def _show_url(self, url, duration):
        chromium = find_binary("chromium", "chromium-browser", "google-chrome")
        if not chromium:
            log(f"no chromium found; cannot display URL {url}")
            self.on_video_end()
            return
        # --start-maximized only works when X reports screen sizes via
        # XRANDR/WM hints; with a bare X (no WM) chromium can open windowed.
        # Query the real geometry and force it with --window-size instead.
        w, h = self._screen_size()
        cmd = [chromium, "--kiosk", "--noerrdialogs", "--disable-infobars",
               "--disable-session-crashed-bubble", "--no-first-run",
               f"--window-size={w},{h}", "--window-position=0,0",
               "--disable-gpu" if not find_binary("omxplayer") else "--enable-gpu-rasterization",
               "--check-for-update-interval=31536000", url]
        if self._start(cmd):
            self.timer = threading.Timer(duration, self.on_video_end)
            self.timer.daemon = True
            self.timer.start()

    def _screen_size(self):
        """Actual display resolution via xrandr/xdpyr, with sane fallbacks."""
        # Preferred: xrandr current mode (marked with *)
        try:
            out = subprocess.run(["xrandr", "--current"], capture_output=True,
                                 text=True, timeout=5).stdout
            for line in out.splitlines():
                if "*" in line:
                    dims = line.split()[0]  # e.g. "3840x2160"
                    w, h = dims.split("x")
                    return int(w), int(h)
        except (OSError, ValueError, subprocess.SubprocessError) as e:
            log(f"xrandr query failed: {e}")
        # Fallback: xdpyinfo reports the root window dimensions
        try:
            out = subprocess.run(["xdpyinfo"], capture_output=True,
                                 text=True, timeout=5).stdout
            for line in out.splitlines():
                if line.startswith("  dimensions:"):
                    dims = line.split()[1]  # e.g. "3840x2160"
                    w, h = dims.split("x")
                    return int(w), int(h)
        except (OSError, ValueError, subprocess.SubprocessError) as e:
            log(f"xdpyinfo query failed: {e}")
        return 1920, 1080

    def _ensure_local(self, url):
        """Download server-hosted media to a temp file so viewers can read it."""
        if url.startswith("http"):
            full_url = url
        else:
            full_url = self.server + url
        try:
            ext = os.path.splitext(full_url.split("?")[0])[1] or ".img"
            fd, path = tempfile.mkstemp(suffix=ext, prefix="fossignage_")
            os.close(fd)
            with urllib.request.urlopen(full_url, timeout=15) as resp, open(path, "wb") as f:
                f.write(resp.read())
            return path
        except (urllib.error.URLError, OSError) as e:
            log(f"failed to download {full_url}: {e}")
            return None



# ---------------------------------------------------------------- main loop

class DisplayClient:
    def __init__(self, server, code_file, poll_interval, standalone=False,
                 debug=False):
        self.server = server.rstrip("/")
        self.code_file = code_file
        self.poll_interval = poll_interval
        self.standalone = standalone
        self.debug = debug
        self.code = self._load_code()
        self.player = NativePlayer(self.server, standalone)
        self.player.on_video_end = self.advance
        self.items = []
        self.index = 0
        self.signature = None
        self.state = "idle"  # idle | ready | playing

    # -- code persistence ----------------------------------------------------

    def _load_code(self):
        try:
            with open(self.code_file) as f:
                code = f.read().strip().upper()
                if len(code) == 4:
                    return code
        except OSError:
            pass
        return None

    def _save_code(self, code):
        try:
            with open(self.code_file, "w") as f:
                f.write(code)
        except OSError as e:
            log(f"could not persist code: {e}")

    # -- server communication --------------------------------------------------

    def register(self):
        payload = {"code": self.code or ""}
        if self.standalone:
            payload["standalone"] = True
        try:
            data = http_json(f"{self.server}/api/display/register", payload)
        except (urllib.error.URLError, OSError) as e:
            log(f"server unreachable ({e}); retrying...")
            return False
        self.code = data["code"]
        self._save_code(self.code)
        if self.standalone:
            log(f"registered as display {self.code} (standalone, auto-linked)")
        else:
            log(f"registered as display {self.code}"
                + (" (reconnected)" if data.get("reconnected") else " (new pairing code)"))
        return True

    def poll(self):
        try:
            data = http_json(f"{self.server}/api/display/{self.code}")
        except urllib.error.HTTPError as e:
            if e.code == 404:
                log("server lost our code; re-registering")
                self.code = None
                self.register()
            return
        except (urllib.error.URLError, OSError) as e:
            if self.debug:
                log(f"poll error: {e}")
            return

        if not data.get("linked"):
            self._set_state("idle")
            return
        media = data.get("media") or []
        if not media:
            self._set_state("ready")
            return

        sig = json.dumps([data.get("active_playlist_id"), media], sort_keys=True)
        if sig != self.signature:
            self.signature = sig
            self.items = media
            self.index = 0
            self._play_current()
        elif self.state != "playing":
            # Same playlist as before but playback was interrupted (e.g. reboot)
            self._play_current()

    def _set_state(self, new_state):
        if new_state == self.state:
            return
        self.state = new_state
        if new_state == "idle":
            self.player.stop()
            self.player.show_idle(self.code or "----")
        elif new_state == "ready":
            self.player.stop()
            self.player.show_idle(self.code or "----")
        self.signature = None if new_state != "playing" else self.signature

    # -- playback ----------------------------------------------------------------

    def _play_current(self):
        if not self.items:
            return
        item = self.items[self.index]
        single = len(self.items) == 1
        log(f"playing [{self.index + 1}/{len(self.items)}] "
            f"{item.get('name')} ({item.get('type')})")
        self.state = "playing"
        self.player.show_item(item, single)

    def advance(self):
        """Called when the current item finishes (video end / image timer)."""
        if self.state != "playing" or not self.items:
            return
        # Single-item playlist: keep the item loaded. Videos/GIFs loop in
        # place (loop=True was passed at launch); images just restart their
        # timer. Relaunching would cause a visible flash/reload every cycle.
        if len(self.items) == 1:
            item = self.items[0]
            if item.get("type") != "video" \
                    and not item.get("url", "").lower().endswith(VIDEO_EXTS + ANIMATED_EXTS):
                duration = int(item.get("duration") or 8)
                self.player.timer = threading.Timer(duration, self.advance)
                self.player.timer.daemon = True
                self.player.timer.start()
            return
        self.index = (self.index + 1) % len(self.items)
        self._play_current()

    # -- run loop -------------------------------------------------------------------

    def run(self):
        while not self.code:
            if self.register():
                break
            time.sleep(3)
        if not self.code:
            return

        self.player.show_idle(self.code)
        while True:
            try:
                self.poll()
            except Exception as e:  # never let the loop die
                log(f"unexpected error in poll loop: {e}")
            time.sleep(self.poll_interval)


def main():
    ap = argparse.ArgumentParser(description="Fossignage native display player")
    ap.add_argument("--server", default=DEFAULT_SERVER)
    ap.add_argument("--code-file", default=DEFAULT_CODE_FILE)
    ap.add_argument("--poll-interval", type=float, default=2.0)
    ap.add_argument("--standalone", action="store_true",
                    help="standalone mode: no pairing; idle screen shows the "
                         "server address instead of a pairing code")
    ap.add_argument("--unlink", action="store_true",
                    help="unlink this display from the server and exit")
    ap.add_argument("--debug", action="store_true")
    ap.add_argument("--log-dir", default=DEFAULT_LOG_DIR,
                    help="directory for player.log (default: %(default)s)")
    args = ap.parse_args()

    setup_logging(args.log_dir)
    client = DisplayClient(args.server, args.code_file, args.poll_interval,
                           args.standalone, args.debug)

    if args.unlink:
        if client.code:
            try:
                http_json(f"{client.server}/api/unlink_display",
                          {"code": client.code})
                log(f"unlinked display {client.code}")
            except (urllib.error.URLError, OSError) as e:
                log(f"unlink failed: {e}")
        try:
            os.remove(args.code_file)
        except OSError:
            pass
        return

    log(f"starting native player -> {args.server}")
    try:
        client.run()
    except KeyboardInterrupt:
        log("stopping")
        client.player.stop()


if __name__ == "__main__":
    main()
