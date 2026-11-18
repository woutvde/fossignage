"""Smoke tests for the Fossignage Flask backend (classic + standalone modes).

Run from the repo root:
    python3 -m unittest discover -s tests -t . -v
or:
    python3 tests/test_app.py

The suite points DATA_FILE/UPLOAD_FOLDER at a temporary directory, so the
real data.json and static/media are never touched.
"""

import base64
import io
import math
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import app as app_module

# 1x1 transparent PNG
PNG_1PX = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk"
    "+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="
)

FFMPEG = shutil.which("ffmpeg")
FFPROBE = shutil.which("ffprobe")


def make_video(path, duration, codec="mpeg4", size="320x240"):
    """Generate a small test video (lavfi testsrc)."""
    subprocess.run(
        [FFMPEG, "-y", "-f", "lavfi",
         "-i", f"testsrc=duration={duration}:size={size}:rate=30",
         "-c:v", codec, "-pix_fmt", "yuv420p", path],
        check=True, capture_output=True, timeout=120)


class FossignageTestCase(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="fossignage-test-")
        app_module.DATA_FILE = os.path.join(cls.tmp, "data.json")
        app_module.UPLOAD_FOLDER = os.path.join(cls.tmp, "media")
        os.makedirs(app_module.UPLOAD_FOLDER, exist_ok=True)
        app_module.app.config["UPLOAD_FOLDER"] = app_module.UPLOAD_FOLDER
        app_module.state["displays"] = []
        app_module.state["media"] = []
        app_module.state["playlists"] = []
        app_module.state["standalone_playlist_id"] = None
        app_module.transcode_jobs.clear()
        app_module.save_state()
        cls.client = app_module.app.test_client()

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def setUp(self):
        self.client = type(self).client
        app_module.app.config["STANDALONE"] = False

    # -- pages -----------------------------------------------------------

    def test_pages_render(self):
        for path, marker in [
            ("/", "Operator Console"),
            ("/operator", "Operator Console"),
            ("/display", "displayCodeLabel"),
        ]:
            res = self.client.get(path)
            self.assertEqual(res.status_code, 200, path)
            self.assertIn(marker, res.get_data(as_text=True))

    def test_state_shape(self):
        res = self.client.get("/api/state")
        self.assertEqual(res.status_code, 200)
        data = res.get_json()
        for key in ("standalone", "displays", "media", "playlists",
                    "upload_limits", "url_playback"):
            self.assertIn(key, data)
        # free space must serialize as a finite number — a bare Infinity
        # token is invalid JSON and breaks JSON.parse in the browser
        self.assertNotIn(b"Infinity", res.get_data())
        self.assertTrue(math.isfinite(data["upload_limits"]["free"]))

    # -- classic pairing flow ---------------------------------------------

    def test_register_link_playlist_poll_delete(self):
        res = self.client.post("/api/display/register", json={"code": ""})
        self.assertEqual(res.status_code, 200)
        code = res.get_json()["code"]
        self.assertEqual(len(code), 4)

        # re-registering with the same code reconnects
        res = self.client.post("/api/display/register", json={"code": code})
        self.assertTrue(res.get_json()["reconnected"])

        # linking: unknown code 404s, known code succeeds
        res = self.client.post("/api/link_display", json={"code": "ZZZZ"})
        self.assertEqual(res.status_code, 404)
        res = self.client.post("/api/link_display", json={"code": code})
        self.assertTrue(res.get_json()["success"])

        # upload an image
        res = self.client.post(
            "/api/upload_media",
            data={"name": "Test PNG", "file": (io.BytesIO(PNG_1PX), "test.png")},
            content_type="multipart/form-data")
        self.assertEqual(res.status_code, 200)
        media = res.get_json()["media"]
        self.assertEqual(media["type"], "image")
        self.assertTrue(media["url"].startswith("/static/media/"))
        media_id = media["id"]

        # the uploaded file is actually served
        res = self.client.get(media["url"])
        self.assertEqual(res.status_code, 200)

        # build a playlist and activate it on the display
        res = self.client.post("/api/save_playlist",
                               json={"name": "Smoke",
                                     "items": [{"media_id": media_id,
                                                "duration": 5}]})
        playlist_id = res.get_json()["playlist"]["id"]
        res = self.client.post("/api/activate_playlist",
                               json={"code": code, "playlist_id": playlist_id})
        self.assertTrue(res.get_json()["success"])

        # the display polls and receives the playlist content
        data = self.client.get(f"/api/display/{code}").get_json()
        self.assertTrue(data["linked"])
        self.assertEqual(len(data["media"]), 1)
        self.assertEqual(data["media"][0]["id"], media_id)
        self.assertEqual(data["media"][0]["duration"], 5)

        # the operator state sees the display as linked and online
        state = self.client.get("/api/state").get_json()
        disp = next(d for d in state["displays"] if d["code"] == code)
        self.assertTrue(disp["linked"])
        self.assertTrue(disp["online"])

        # delete the media: gone from state, from playlists, and from disk
        disk_path = os.path.join(app_module.UPLOAD_FOLDER,
                                 media["url"].rsplit("/", 1)[-1])
        self.assertTrue(os.path.exists(disk_path))
        res = self.client.post("/api/delete_media", json={"id": media_id})
        self.assertTrue(res.get_json()["success"])
        state = self.client.get("/api/state").get_json()
        self.assertNotIn(media_id, [m["id"] for m in state["media"]])
        pl = next(p for p in state["playlists"] if p["id"] == playlist_id)
        self.assertEqual(pl["items"], [])
        self.assertFalse(os.path.exists(disk_path))

    # -- standalone mode ----------------------------------------------------

    def test_standalone_mode(self):
        app_module.app.config["STANDALONE"] = True
        try:
            state = self.client.get("/api/state").get_json()
            self.assertTrue(state["standalone"])

            # explicit linking is disabled in standalone mode
            res = self.client.post("/api/link_display", json={"code": "ABCD"})
            self.assertEqual(res.status_code, 403)

            # displays register already linked
            code = self.client.post("/api/display/register",
                                    json={"code": ""}).get_json()["code"]
            state = self.client.get("/api/state").get_json()
            disp = next(d for d in state["displays"] if d["code"] == code)
            self.assertTrue(disp["linked"])

            # choose the standalone playlist
            pid = self.client.post("/api/save_playlist",
                                   json={"name": "SA", "items": []}
                                   ).get_json()["playlist"]["id"]
            res = self.client.post("/api/set_active_playlist",
                                   json={"playlist_id": pid})
            self.assertTrue(res.get_json()["success"])
            res = self.client.post("/api/set_active_playlist",
                                   json={"playlist_id": "p_nope"})
            self.assertEqual(res.status_code, 404)

            # polling falls back to the standalone playlist
            data = self.client.get(f"/api/display/{code}").get_json()
            self.assertTrue(data["linked"])
            self.assertEqual(data["media"], [])
        finally:
            app_module.app.config["STANDALONE"] = False

    # -- transcode lifecycle -------------------------------------------------

    def test_delete_media_removes_transcoding_leftover(self):
        filename = "abc123_clip.mp4"
        file_path = os.path.join(app_module.UPLOAD_FOLDER, filename)
        leftover = file_path + ".transcoding.mp4"
        with open(file_path, "wb") as f:
            f.write(b"fake video")
        with open(leftover, "wb") as f:
            f.write(b"partial transcode")
        media_id = "m_leftover"
        app_module.state["media"].append({
            "id": media_id, "name": "clip",
            "url": f"/static/media/{filename}", "type": "video"})
        app_module.save_state()
        try:
            res = self.client.post("/api/delete_media", json={"id": media_id})
            self.assertTrue(res.get_json()["success"])
            self.assertFalse(os.path.exists(file_path))
            self.assertFalse(os.path.exists(leftover))
            self.assertNotIn(media_id,
                             [m["id"] for m in app_module.state["media"]])
        finally:
            app_module.state["media"] = [m for m in app_module.state["media"]
                                         if m["id"] != media_id]
            app_module.save_state()

    def _upload_video(self, name, duration, codec="mpeg4"):
        src = os.path.join(self.tmp, f"{name}.mp4")
        make_video(src, duration=duration, codec=codec)
        with open(src, "rb") as f:
            payload = f.read()
        res = self.client.post(
            "/api/upload_media",
            data={"name": name, "file": (io.BytesIO(payload), f"{name}.mp4")},
            content_type="multipart/form-data")
        return res.get_json()["media"]

    def _wait_for_job(self, media_id, timeout=120):
        deadline = time.time() + timeout
        while time.time() < deadline:
            jobs = self.client.post("/api/transcode_status",
                                    json={"ids": [media_id]}).get_json()["jobs"]
            if media_id in jobs and not jobs[media_id]["running"]:
                return jobs[media_id]
            time.sleep(0.5)
        self.fail(f"transcode job {media_id} did not finish within {timeout}s")

    @unittest.skipUnless(FFMPEG and FFPROBE, "ffmpeg/ffprobe not installed")
    def test_transcode_rewrites_video_to_h264(self):
        media = self._upload_video("clip", duration=0.5)
        self.assertEqual(media["type"], "video")
        self.assertEqual(media["video_codec"], "mpeg4")
        self.assertTrue(media["needs_transcode"])

        res = self.client.post("/api/transcode_media", json={"id": media["id"]})
        self.assertTrue(res.get_json()["success"])

        job = self._wait_for_job(media["id"])
        self.assertIsNone(job.get("error"), job)
        self.assertEqual(job["progress"], 100)

        state = self.client.get("/api/state").get_json()
        m = next(m for m in state["media"] if m["id"] == media["id"])
        self.assertEqual(m["video_codec"], "h264")
        self.assertFalse(m["needs_transcode"])

    @unittest.skipUnless(FFMPEG and FFPROBE, "ffmpeg/ffprobe not installed")
    def test_delete_during_transcode_does_not_resurrect_file(self):
        # Long enough that the transcode is still running when we delete.
        media = self._upload_video("race", duration=20)
        media_id = media["id"]
        disk_path = os.path.join(app_module.UPLOAD_FOLDER,
                                 media["url"].rsplit("/", 1)[-1])

        res = self.client.post("/api/transcode_media", json={"id": media_id})
        self.assertTrue(res.get_json()["success"])
        self.assertTrue(app_module.transcode_jobs[media_id]["running"])

        res = self.client.post("/api/delete_media", json={"id": media_id})
        self.assertTrue(res.get_json()["success"])

        self._wait_for_job(media_id)
        # the deleted media must not come back on disk after the job finishes
        self.assertFalse(os.path.exists(disk_path))
        state = self.client.get("/api/state").get_json()
        self.assertNotIn(media_id, [m["id"] for m in state["media"]])


if __name__ == "__main__":
    unittest.main()
