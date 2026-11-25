"""Exercise the review API on a disposable album containing one synthetic JPEG."""

import argparse
import base64
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import tempfile
import time
from urllib.error import HTTPError, URLError
from urllib.parse import urlencode
from urllib.request import ProxyHandler, Request, build_opener


# Generated color blocks, with no EXIF or source photograph. Stored inline
# for the temporary test album.
SAMPLE = """
/9j/4AAQSkZJRgABAQEAYABgAAD/2wBDAAMCAgMCAgMDAwMEAwMEBQgFBQQEBQoHBwYIDAoMDAsKCwsN
DhIQDQ4RDgsLEBYQERMUFRUVDA8XGBYUGBIUFRT/2wBDAQMEBAUEBQkFBQkUDQsNFBQUFBQUFBQUFBQU
FBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBT/wAARCAAwAEADASIAAhEBAxEB/8QA
HwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/8QAtRAAAgEDAwIEAwUFBAQAAAF9AQIDAAQRBRIh
MUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NTY3ODk6Q0RFRkdISUpTVFVW
V1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXG
x8jJytLT1NXW19jZ2uHi4+Tl5ufo6erx8vP09fb3+Pn6/8QAHwEAAwEBAQEBAQEBAQAAAAAAAAECAwQF
BgcICQoL/8QAtREAAgECBAQDBAcFBAQAAQJ3AAECAxEEBSExBhJBUQdhcRMiMoEIFEKRobHBCSMzUvAV
YnLRChYkNOEl8RcYGRomJygpKjU2Nzg5OkNERUZHSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6goOE
hYaHiImKkpOUlZaXmJmaoqOkpaanqKmqsrO0tba3uLm6wsPExcbHyMnK0tPU1dbX2Nna4uPk5ebn6Onq
8vP09fb3+Pn6/9oADAMBAAIRAxEAPwDEoorc8E+Gf+Ex8T2Wkfafsn2nf++8vft2ozfdyM/dx171+o43
GUMuwtXGYqXLTpxcpOzdoxTbdlduyWyTfY/KqNGeIqxo0leUmkl5vRGHRXuH/DM//Uyf+SP/ANsrD8bf
A3/hDvDF7q/9t/a/s2z9z9k2btzqv3t5x97PTtX5pgvFTg/McVSweFxvNUqSUYr2dVXlJpJXcEldvdtL
ufSVuF83w9KVarRtGKbb5o7LV/aPK6KKK/Vz5UKKKKACu4+Cf/JTtG/7bf8AomSuHorx86y7+18sxWW8
/J7anOHNa9ueLje11e172ur90dmDxH1TE0sRa/JJSttezvY+4K4f42f8kx1n/tj/AOjo6+V6K/mnJfAn
+yMzwuZf2lz+xqQny+xtfkkpWv7V2va17O3Zn6PjOOPreGq4f6tbni43572urX+EKKKK/q4/LAooooAK
KKKACiiigAooooAKKKKAP//Z
"""


def check(port, token, album, data_dir):
    origin = f"http://127.0.0.1:{port}"
    opener = build_opener(ProxyHandler({}))

    def request(path, expected, *, data=None, session=None):
        headers = {}
        if data is not None:
            headers.update({"Content-Type": "application/json", "Origin": origin})
        if session is not None:
            headers["Authorization"] = "Bearer " + session
        req = Request(origin + path, data, headers)
        try:
            response = opener.open(req, timeout=3)
        except HTTPError as error:
            response = error
        with response:
            assert response.status == expected, f"{path}: HTTP {response.status}, expected {expected}"
            return response.headers, response.read(1_048_576)

    deadline = time.monotonic() + 45
    while True:
        try:
            assert request("/healthz", 200)[1] == b"firstcut-review"
            break
        except (URLError, TimeoutError):
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.5)

    request("/", 200)
    request("/review.js", 200)
    request("/api/photos", 401)
    payload = json.dumps({"key": token}).encode()
    _, body = request("/api/session", 200, data=payload)
    session = json.loads(body)["session"]
    assert isinstance(session, str) and len(session) >= 32, "missing review session"
    _, body = request("/api/photos", 200, session=session)
    photos = json.loads(body)["photos"]
    assert len(photos) == 1 and photos[0]["path"] == "sample.jpg"
    assert photos[0]["scores"] is None

    query = urlencode({"p": "sample.jpg"})
    for endpoint in ("/thumb", "/image"):
        headers, jpeg = request(f"{endpoint}?{query}", 200, session=session)
        assert headers.get_content_type() == "image/jpeg"
        assert jpeg.startswith(b"\xff\xd8"), f"{endpoint}: expected JPEG"
    request("/api/score/run", 202, data=b'{"no_ai":true}', session=session)
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        _, body = request("/api/job", 200, session=session)
        job = json.loads(body)
        if job["state"] == "done":
            break
        assert job["state"] != "failed", f"scoring failed: {job.get('error', '')}"
        time.sleep(0.25)
    else:
        raise AssertionError("scoring did not finish")
    _, body = request("/api/photos", 200, session=session)
    assert json.loads(body)["photos"][0]["scores"] is not None
    assert not (album / ".firstcut").exists(), "source album must remain zero-write"
    reports = list(data_dir.glob("workspaces/*/report.csv"))
    assert len(reports) == 1 and reports[0].is_file(), "scoring report was not persisted in local data dir"
    print("Verified: authenticated review, JPEG responses, rescoring, and persisted report.")


def run(port, album, data_dir, binary):
    token = secrets.token_hex(32)
    origin = f"http://127.0.0.1:{port}"
    env = {
        **os.environ,
        "FIRSTCUT_ACCESS_TOKEN": token,
        "FIRSTCUT_PUBLIC_URL": origin,
        "FIRSTCUT_DATA_DIR": str(data_dir),
    }
    with subprocess.Popen(
        [str(binary.resolve()), "review", str(album), "--port", str(port), "--no-browser"],
        env=env,
    ) as process:
        try:
            check(port, token, album, data_dir)
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    args = parser.parse_args()
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    with tempfile.TemporaryDirectory(prefix="firstcut-smoke-") as temp:
        base = Path(temp).resolve()
        album = base / "album"
        data_dir = base / "data"
        album.mkdir()
        data_dir.mkdir()
        (album / "sample.jpg").write_bytes(base64.b64decode(SAMPLE))
        run(port, album, data_dir, args.binary)


if __name__ == "__main__":
    main()
