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

  - video : omxplayer (Pi HW accel) -> vlc -> mpv -> ffplay
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

# How long a freshly launched player gets to paint its first frame before the
# previous item's process is killed. The old item keeps rendering underneath
# in the meantime, so queue transitions swap instantly instead of flashing
# black while the next player starts up.
SETTLE_IMAGE = 0.5   # feh maps its window almost immediately
SETTLE_VIDEO = 3.0   # decoder init + first frame of a local/LAN stream
SETTLE_URL = 6.0     # chromium cold start is the slowest


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
    """Owns the subprocess (or timer) currently rendering an item.

    Transitions overlap: the next item is launched while the previous one is
    still on screen, and the old process is only retired once the new one has
    had time to paint over it (see _swap), so there is no black gap.
    """

    def __init__(self, server, standalone=False):
        self.server = server
        self.standalone = standalone
        self.proc = None          # subprocess currently on screen
        self.proc_dir = None      # scratch dir backing it (chromium profile)
        self.old_proc = None      # previous subprocess, dying in the background
        self.old_dir = None
        self.timer = None         # threading.Timer for image/url durations
        self._lock = threading.Lock()
        self._cache = {}          # url -> pre-downloaded temp file
        self._cache_lock = threading.Lock()
        self.on_video_end = lambda: None  # set by DisplayClient

    # -- lifecycle ---------------------------------------------------------

    def stop(self):
        """Tear down everything on (or coming onto) screen."""
        with self._lock:
            if self.timer:
                self.timer.cancel()
                self.timer = None
            procs = [(self.proc, self.proc_dir), (self.old_proc, self.old_dir)]
            self.proc = self.old_proc = None
            self.proc_dir = self.old_dir = None
        for proc, scratch_dir in procs:
            if proc:
                self._kill(proc)
                self._cleanup_dir(scratch_dir)

    def _kill(self, proc):
        """Terminate a player process group (omxplayer spawns children)."""
        if proc.poll() is not None:
            return
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
        except (ProcessLookupError, PermissionError, OSError):
            try:
                proc.terminate()
            except OSError:
                pass
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
            except OSError:
                pass

    def _cleanup_dir(self, path):
        if path:
            shutil.rmtree(path, ignore_errors=True)

    def _start(self, cmd, wait_for_exit=False, env=None, scratch_dir=None):
        log("exec: " + " ".join(cmd))
        kwargs = dict(stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                      start_new_session=True)
        if env:
            kwargs["env"] = env
        try:
            proc = subprocess.Popen(cmd, **kwargs)
        except OSError as e:
            log(f"failed to launch {cmd[0]}: {e}")
            return False
        with self._lock:
            self.proc = proc
            self.proc_dir = scratch_dir
        if wait_for_exit:
            threading.Thread(target=self._wait_video, args=(proc,),
                             daemon=True).start()
        return True

    def _wait_video(self, proc):
        """Advance the playlist when the video process exits naturally."""
        proc.wait()
        with self._lock:
            current = self.proc is proc  # not superseded by a stop()/new item
            if current:
                self.proc = None
        # If the player died almost instantly it failed to play the file
        # (bad flag, unsupported format, no X connection...). Log it loudly
        # and advance anyway so one broken item can't stall the playlist.
        if proc.returncode not in (0, None):
            log(f"player exited with code {proc.returncode}")
        if current:
            # A killed process was already replaced on purpose; advancing here
            # too would skip straight past the item that was just started.
            self.on_video_end()

    def _swap(self, cmd, settle, wait_for_exit=False, env=None, scratch_dir=None):
        """Start the next item while the current one is still on screen, then
        retire the old process once the new one has had a moment to paint.
        This keeps queue transitions gap-free instead of flashing black."""
        with self._lock:
            old, old_dir = self.proc, self.proc_dir
            stale = self.old_proc
            if self.timer:
                self.timer.cancel()
                self.timer = None
            self.proc = self.proc_dir = None
            self.old_proc, self.old_dir = old, old_dir
        if stale is not None:
            self._kill(stale)  # superseded swap; don't leak it on screen
        if not self._start(cmd, wait_for_exit=wait_for_exit, env=env,
                           scratch_dir=scratch_dir):
            self.stop()
            self.on_video_end()
            return False
        if old is not None:
            threading.Thread(target=self._retire, args=(old, old_dir, settle),
                             daemon=True).start()
        return True

    def _retire(self, proc, scratch_dir, settle):
        """Wait for the new item to paint, then take the old one off screen."""
        deadline = time.time() + settle
        while time.time() < deadline:
            with self._lock:
                cur = self.proc
            if cur is not None and cur.poll() is not None:
                break  # new item already died; stop covering for it
            time.sleep(0.05)
        self._kill(proc)
        self._cleanup_dir(scratch_dir)

    def _arm_timer(self, duration):
        self.timer = threading.Timer(duration, self.on_video_end)
        self.timer.daemon = True
        self.timer.start()

    # -- rendering ---------------------------------------------------------

    def show_idle(self, code):
        """Full-screen status card: pairing code, or server address in
        standalone mode."""
        # Render the card before stopping the current item so the swap to
        # idle doesn't sit on a black screen while ImageMagick runs.
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
        self.stop()
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
            self._start([fbi, "-T", "1", "-d", "/dev/fb0", "-noverbose", "-a", img],
                        env=env)
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
        # No stop() here: the next item is launched while this one is still on
        # screen and the old process is retired once the new one has painted
        # (see _swap), so there is no black gap between queue items.

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
            # No --blank: it blacks the screen during startup, which defeats
            # the overlap swap (the previous item must stay visible until the
            # first frame is decoded).
            cmd = [omx, "--no-osd", "-o", "both", full_url]
            if loop:
                cmd.insert(1, "--loop")
        elif vlc:
            # VLC first: RPi OS builds it with mmal hardware decoding, which
            # plays 1080p smoothly where mpv's GL pipeline (dumb-mode FBOs +
            # drm_prime copy-back) drops frames. Deliberately minimal flags:
            # a plain window works on bare X without a window manager (the
            # old wallpaper mode is what used to segfault).
            cmd = [vlc, "--intf", "dummy", "--no-osd", "--no-video-title-show",
                   "--play-and-exit", "--fullscreen", full_url]
            if loop:
                cmd.append("--loop")
            return self._swap(cmd, SETTLE_VIDEO, wait_for_exit=True)
        elif mpv:
            # mpv fallback (mostly non-Pi systems): handles bare X (no WM)
            # reliably via its own fullscreen window.
            # --vo=gpu renders through OpenGL/EGL instead of the default
            # software X11 path (falls back to x11 when GL is unavailable);
            # --hwdec=auto picks up vaapi/nvdec where present, else software.
            # (--hwdec takes a single value, not a list, so v4l2m2m can't be
            # chained onto auto; on Pi systems VLC above does hw decoding.)
            w, h = self._screen_size()
            cmd = [mpv, "--no-border", "--fullscreen", "--no-osd-bar",
                   "--cursor-autohide=no",
                   "--vo=gpu,x11",
                   "--hwdec=auto",
                   "--geometry={}x{}+0+0".format(w, h),
                   "--autofit-larger={}x{}".format(w, h),
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
        self._swap(cmd, SETTLE_VIDEO, wait_for_exit=True)

    def _show_image(self, url, duration):
        # Downloaded while the previous item is still on screen (or served
        # from the prefetch cache) so launching the viewer is the only wait.
        local = self._ensure_local(url)
        if not local:
            self.on_video_end()
            return
        feh = find_binary("feh")
        fbi = find_binary("fbi")
        if feh:
            if self._swap([feh, "-F", "-Z", "-Y", "--hide-pointer", local],
                          SETTLE_IMAGE):
                self._arm_timer(duration)
        elif fbi:
            # fbi owns the console tty: starting the next instance before the
            # old one exits would let the dying one clobber the framebuffer,
            # so stop-then-start here (the download above already removed the
            # slow part of the gap).
            self.stop()
            if self._start([fbi, "-T", "1", "-d", "/dev/fb0", "-noverbose",
                            "-a", local]):
                self._arm_timer(duration)
            else:
                self.on_video_end()
        else:
            log("no image viewer found (install feh or fbi)")
            self.on_video_end()

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
        # Fresh profile dir per instance: with the overlap swap two kiosk
        # instances can be alive briefly, and chromium's single-instance
        # profile lock would make the new one defer to the dying one. The
        # retired instance's dir is removed in _retire.
        data_dir = tempfile.mkdtemp(prefix="fossignage-chrome-")
        cmd = [chromium, "--kiosk", "--noerrdialogs", "--disable-infobars",
               "--disable-session-crashed-bubble", "--no-first-run",
               f"--window-size={w},{h}", "--window-position=0,0",
               "--disable-gpu" if not find_binary("omxplayer") else "--enable-gpu-rasterization",
               "--check-for-update-interval=31536000",
               "--user-data-dir=" + data_dir, url]
        if self._swap(cmd, SETTLE_URL, scratch_dir=data_dir):
            self._arm_timer(duration)

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
        """Download server-hosted media to a temp file so viewers can read it.
        Results are cached per URL so pre-fetched items swap in instantly."""
        with self._cache_lock:
            cached = self._cache.get(url)
        if cached and os.path.exists(cached):
            return cached
        if url.startswith("http"):
            full_url = url
        else:
            full_url = self.server + url
        path = None
        try:
            ext = os.path.splitext(full_url.split("?")[0])[1] or ".img"
            fd, path = tempfile.mkstemp(suffix=ext, prefix="fossignage_")
            os.close(fd)
            with urllib.request.urlopen(full_url, timeout=15) as resp, open(path, "wb") as f:
                f.write(resp.read())
        except (urllib.error.URLError, OSError) as e:
            log(f"failed to download {full_url}: {e}")
            if path:
                try:
                    os.unlink(path)
                except OSError:
                    pass
            return None
        with self._cache_lock:
            self._cache[url] = path
        return path

    def prefetch(self, url):
        """Pre-download an image in the background so the transition from the
        current item (typically a video, whose window is already gone by the
        time we advance) can start without a network wait."""
        threading.Thread(target=self._ensure_local, args=(url,),
                         daemon=True).start()

    def clear_image_cache(self):
        """Drop cached downloads (the playlist changed; replaced media must
        not be served stale from the cache)."""
        with self._cache_lock:
            paths = list(self._cache.values())
            self._cache.clear()
        for path in paths:
            try:
                os.unlink(path)
            except OSError:
                pass



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
            self.player.clear_image_cache()
            self._play_current()
        elif self.state != "playing":
            # Same playlist as before but playback was interrupted (e.g. reboot)
            self._play_current()

    def _set_state(self, new_state):
        if new_state == self.state:
            return
        self.state = new_state
        if new_state in ("idle", "ready"):
            # show_idle renders the card before stopping the old item
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
        self._prefetch_next()

    def _prefetch_next(self):
        """Download the next image in the background while the current item
        plays, so the transition when a video ends is instant."""
        if len(self.items) < 2:
            return
        nxt = self.items[(self.index + 1) % len(self.items)]
        url = nxt.get("url", "")
        if url.lower().endswith(IMAGE_EXTS):
            self.player.prefetch(url)

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
