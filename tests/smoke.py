"""Smoke-test the plugin in real Tern; needs a desktop session and the built daemon
(`cargo build --release` in daemon/).

Runs an isolated Tern (its own config folder, daemon and control socket, so your own
Tern and plugins are untouched) and links this folder as a plugin. Screenshots go to
.dev/shots/.

    python3 tests/smoke.py          # signed out: no account involved
    python3 tests/smoke.py --live   # signed in, with a copy of your stored sign-in

The default run opens the player block, checks that it starts the Rust daemon and shows
the sign-in card, that commands are refused while signed out (and the Soloist player
stays stopped), and that Quit Player and Start again work.

`--live` copies the credential your own player stored (`tern plugin dir`'s plugin-data)
into the isolated config, and links your Soloist executable and copies your API key
(plugin-data/spotify/soloist/), so it needs one sign-in done in your Tern, Soloist set
up there, and your own player stopped (the port is shared). The isolated Soloist starts
with no session of its own, so the run checks that the daemon pairs it with the
account. It browses the library into Liked Songs and back, plays a song there, searches
through the `/` prompt, opens an artist, plays it with `a`, queues a song, switches
between Lossless and 320 kbps (checking the format Soloist decodes), and pauses. It
plays a few seconds of audio.
"""
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
TERN = os.environ.get("TERN", shutil.which("tern") or "/Applications/Tern.app/Contents/MacOS/tern")
ARTIFACTS = ROOT / ".dev"
ARTIFACTS.mkdir(exist_ok=True)
MAIN = ".sf-main[data-surface='plugin.spotify.player']"
DOCK = ".sf-dock[data-surface='plugin.spotify.player']"
SELECTED = f"{MAIN} .sel"
TRAIL = f"{MAIN} .sp-list-title"
PORT = 47821
BIN = ROOT / "daemon/target/release/tern-spotify"
LIVE = "--live" in sys.argv[1:]
# What `--live` copies from your own player's data: the reusable credential, the
# device id it belongs with, and the persisted-query hashes.
REAL_DATA = Path.home() / "Library/Application Support/Tern/plugin-data/spotify/librespot"
SEED = ("credentials.json", "device_id", "pathfinder.json")
# Your Soloist executable (linked) and API key (copied); never its session.
REAL_SOLOIST = REAL_DATA.parent / "soloist"
# 晴天 (Jay Chou): Spotify has line-synced lyrics for it.
SYNCED_TRACK = "spotify:track:5pIcwtJYNJx93l420oR2Vm"


def api(path, method="GET"):
    req = urllib.request.Request(f"http://127.0.0.1:{PORT}{path}", method=method)
    with urllib.request.urlopen(req, timeout=20) as r:
        return json.loads(r.read())


def port_open():
    with socket.socket() as s:
        s.settimeout(0.5)
        return s.connect_ex(("127.0.0.1", PORT)) == 0


def publish(out: Path, config: Path):
    """Copies a run's shots and logs (Tern's and the daemon's) into .dev/ once Tern has
    stopped: Tern reloads a linked plugin on any change in its folder."""
    for name in ("shots", "smoke-logs"):
        shutil.rmtree(ARTIFACTS / name, ignore_errors=True)
        if (out / name).exists():
            shutil.copytree(out / name, ARTIFACTS / name)
    shutil.copy(out / "smoke-process.log", ARTIFACTS / "smoke-process.log")
    daemon_log = config / "plugin-data" / "spotify" / "daemon.log"
    if daemon_log.exists():
        shutil.copy(daemon_log, ARTIFACTS / "smoke-daemon.log")


assert BIN.exists(), f"build the daemon first: cargo build --release in {ROOT / 'daemon'}"
assert not port_open(), f"something already listens on 127.0.0.1:{PORT} (a running player?); quit it first"
if LIVE:
    assert (REAL_DATA / "credentials.json").exists(), f"--live needs a sign-in stored in {REAL_DATA}"
    assert (REAL_SOLOIST / "soloist").exists() and (REAL_SOLOIST / "api-key").exists(), \
        f"--live needs the Soloist executable and your API key in {REAL_SOLOIST}"

with tempfile.TemporaryDirectory(prefix="tern-spotify-", dir="/tmp") as temporary:
    work = Path(temporary)
    config = work / "config"
    config.mkdir()
    # A fresh config makes Tern offer to install and manage omp ("Let Tern keep omp up
    # to date?"). Mark the offer as made and declined, as the real config does once
    # answered, so the isolated window never asks.
    (config / "omp").mkdir()
    (config / "omp" / "state.json").write_text(
        json.dumps({"managed": False, "checked": 0, "no_build": False, "offered": True}), encoding="utf-8")
    if LIVE:
        seeded = config / "plugin-data" / "spotify" / "librespot"
        seeded.mkdir(parents=True)
        for name in SEED:
            if (REAL_DATA / name).exists():
                shutil.copy(REAL_DATA / name, seeded / name)
        solo = seeded.parent / "soloist"
        solo.mkdir(mode=0o700)
        (solo / "soloist").symlink_to((REAL_SOLOIST / "soloist").resolve())
        shutil.copy(REAL_SOLOIST / "api-key", solo / "api-key")
        (solo / "api-key").chmod(0o600)
    zdot = work / "zsh"
    zdot.mkdir()
    (zdot / ".zshrc").write_text("PROMPT='%~ %# '\n", encoding="utf-8")
    out = work / "artifacts"
    out.mkdir()
    env = dict(os.environ, TERN_CONFIG_DIR=str(config),
               TERN_DAEMON_SOCKET=str(work / "daemon.sock"),
               STENCIL_LOG_DIR=str(out / "smoke-logs"),
               ZDOTDIR=str(zdot))
    link = subprocess.run([TERN, "plugin", "link", str(ROOT)], env=env, check=True,
                          text=True, capture_output=True)
    print("$ tern plugin link .\n" + link.stdout.strip())
    control = str(work / "control.sock")

    def ctl(command):
        result = subprocess.run([TERN, "ctl", "--control", control, command],
                                env=env, text=True, capture_output=True, timeout=40)
        reply = json.loads(result.stdout)
        assert result.returncode == 0 and reply["ok"], (command, reply)
        return reply

    def texts(selector):
        return [n.get("text", "") for n in ctl(f"tree {selector}")["nodes"]]

    def wait_text(selector, needle, timeout=20):
        deadline = time.monotonic() + timeout
        while True:
            found = texts(selector)
            if any(needle in t for t in found):
                return found
            assert time.monotonic() < deadline, (selector, needle, found)
            time.sleep(0.25)

    def keys(*combos):
        for combo in combos:
            ctl(f"key {combo}")

    def wait_status(check, why, timeout=25):
        deadline = time.monotonic() + timeout
        while True:
            status = api("/status")
            if check(status):
                return status
            assert time.monotonic() < deadline, (why, status)
            time.sleep(0.3)

    def api_post(path, body):
        req = urllib.request.Request(f"http://127.0.0.1:{PORT}{path}", method="POST",
                                     data=json.dumps(body).encode(), headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=20) as r:
            return json.loads(r.read())

    def clock(ms):
        s = int(ms // 1000)
        return f"{s // 60}:{s % 60:02d}"

    def selected_line(timeout=0):
        """The selected (sung) lyric line, waiting up to `timeout` for one."""
        deadline = time.monotonic() + timeout
        while True:
            found = texts(f"{MAIN} [data-role='sp-lyrics'] .sel")
            if found or time.monotonic() >= deadline:
                return found[0] if found else ""
            time.sleep(0.2)

    def selected():
        found = texts(SELECTED)
        return found[0] if found else ""

    def select(needle, limit=60):
        """Moves the list's selection down from the top to the first row holding `needle`."""
        keys("Home")
        deadline = time.monotonic() + 30
        while not selected():
            assert time.monotonic() < deadline, "the list shows no selected row"
            time.sleep(0.1)
        for _ in range(limit):
            if needle in selected():
                return selected()
            before = selected()
            keys("ArrowDown")
            deadline = time.monotonic() + 5
            while selected() == before:
                if time.monotonic() > deadline:
                    raise AssertionError(f"↓ didn't move past {before!r} looking for {needle!r}")
                time.sleep(0.1)
        raise AssertionError(f"no row holds {needle!r}")

    def signed_out_checks():
        ctl("plugins run plugin.spotify.open")
        wait_text(MAIN, "Sign in with Spotify")
        ctl("shot spotify-signin")
        status = api("/status")
        assert status["phase"] == "signed_out" and status["device"] == "Tern", status
        assert status["player"]["state"] == "stopped", status["player"]
        print("PASS: the block starts the daemon and shows the sign-in card;"
              f" /status says {status['phase']}, device {status['device']!r}, player {status['player']['state']}")

        for path in ("/toggle", "/next", "/queue", "/search?q=x", "/library"):
            method = "POST" if path in ("/toggle", "/next") else "GET"
            try:
                api(path, method)
                raise AssertionError(f"{path} answered while signed out")
            except urllib.error.HTTPError as e:
                body = json.loads(e.read())
                assert e.code == 409 and "not signed in" in body["error"], (path, e.code, body)
        print("PASS: playback, the queue, search and the library refuse with 409 while signed out")

        logs = list(config.rglob("daemon.log"))
        device_ids = list(config.rglob("librespot/device_id"))
        assert logs and device_ids, (logs, device_ids)
        assert len(device_ids[0].read_text().strip()) == 40
        soloist = device_ids[0].parent.parent / "soloist"
        assert soloist.is_dir() and soloist.stat().st_mode & 0o077 == 0, oct(soloist.stat().st_mode)
        assert not (soloist / "data").exists(), list(soloist.iterdir())
        print(f"PASS: the daemon keeps its log and device id in the plugin data dir ({logs[0].parent});"
              " Soloist's dir is private and holds no session while signed out")

        ctl("plugins run plugin.spotify.quit")
        deadline = time.monotonic() + 10
        while port_open():
            assert time.monotonic() < deadline, "the daemon did not quit"
            time.sleep(0.2)
        wait_text(MAIN, "The player stopped.")
        wait_text(f"{MAIN} .sf-badge", "Start again")
        ctl("shot spotify-stopped")
        print("PASS: Spotify: Quit Player stops the daemon; the block says it stopped and offers Start again")

        ctl(f"click {MAIN} .sf-badge")
        wait_text(MAIN, "Sign in with Spotify")
        assert port_open()
        print("PASS: Start again restarts the daemon")

    def live_checks():
        ctl("plugins run plugin.spotify.open")
        wait_text(TRAIL, "Your Library", timeout=60)
        wait_text(MAIN, "Liked Songs", timeout=30)
        assert api("/status")["phase"] == "ready"
        deadline = time.monotonic() + 20
        while len(ctl(f"tree {MAIN} .sp-thumb .sf-image")["nodes"]) < 3:
            assert time.monotonic() < deadline, "the library's rows show no covers"
            time.sleep(0.3)
        thumbs = len(ctl(f"tree {MAIN} .sp-thumb .sf-image")["nodes"])
        ctl("shot spotify-library")
        print(f"PASS: signed in with the stored credential; the block shows Your Library with Liked Songs,"
              f" {thumbs} rows with their covers")

        # The isolated Soloist has no session: the daemon pairs it with the account.
        status = wait_status(lambda s: s["player"]["state"] == "ready", "Soloist signs in", timeout=60)
        log = next(config.rglob("daemon.log")).read_text()
        assert "Soloist took the pairing" in log, "the isolated Soloist signed in without being paired"
        key = (REAL_SOLOIST / "api-key").read_text().strip()
        assert key not in log, "the API key is in the daemon's log"
        assert key not in json.dumps(status), "the API key is in /status"
        print(f"PASS: the daemon starts Soloist and pairs it with the signed-in account (no Spotify app);"
              f" the build expires in {status['player']['expires_in_days']} days; the key is in neither the log nor /status")

        select("Liked Songs")
        keys("Enter")
        wait_text(TRAIL, "Your Library › Liked Songs")
        wait_text(f"{MAIN} .sp-hero-name", "Liked Songs")
        wait_text(f"{MAIN} .sp-hero-detail", "songs")
        assert ctl(f"tree {MAIN} .sp-hero-play")["nodes"], "Liked Songs' header has no ▶"
        deadline = time.monotonic() + 15
        while not ctl(f"tree {MAIN} .sp-hero-art .sf-image")["nodes"]:
            assert time.monotonic() < deadline, "Liked Songs' header shows no cover"
            time.sleep(0.3)
        deadline = time.monotonic() + 20
        while "Liked Songs" in selected() or selected() == "":
            assert time.monotonic() < deadline, ("Liked Songs' rows did not load", selected())
            time.sleep(0.2)
        keys("ArrowDown")
        time.sleep(0.3)
        row = selected()
        keys("Enter")
        status = wait_status(lambda s: s["playing"] and s["track"] and s["track"]["name"] in row,
                             f"the chosen row {row!r} plays")
        wait_text(SELECTED, "♪")
        ctl("shot spotify-liked-playing")
        print(f"PASS: ⏎ opens Liked Songs (its header shows the cover, name and song count); ⏎ on a song"
              f" plays it there ({status['track']['name']!r}), and its row shows ♪")
        assert status["active"], status
        status = wait_status(lambda s: s["player"]["format"], "Soloist names the format it plays")
        wait_text(DOCK, "Playing on Tern")
        print(f"PASS: Soloist is the active device and plays from {status['player']['format']}")

        rows = f"{MAIN} [data-role='sp-rows'] .sp-row"
        row = texts(f"{rows}:nth-child(4) .sp-row-name")[0].rstrip("…")
        ctl(f"click {rows}:nth-child(4)")
        deadline = time.monotonic() + 5
        while row not in selected():
            assert time.monotonic() < deadline, ("a click doesn't choose the row", row, selected())
            time.sleep(0.1)
        ctl(f"dblclick {rows}:nth-child(4)")
        status = wait_status(lambda s: s["playing"] and s["track"] and s["track"]["name"].startswith(row),
                             f"double-clicking {row!r} plays it")
        print(f"PASS: a click chooses a row, a double-click plays it ({row!r})")

        keys("Backspace")
        wait_text(TRAIL, "Your Library")
        assert not any("›" in t for t in texts(TRAIL)), texts(TRAIL)
        print("PASS: ⌫ goes back to Your Library")

        ctl('type "/"')
        time.sleep(1)
        ctl('type "beyond"')
        keys("Enter")
        wait_text(TRAIL, "Your Library › Search: beyond")
        album = select("Album ›")
        artist = select("Artist ›")
        ctl("shot spotify-search")
        print(f"PASS: / asks in Tern's prompt and the block shows the search, albums ({album!r}) and"
              f" artists ({artist!r}) among the rows")

        keys("Enter")
        wait_text(TRAIL, "Search: beyond › ")
        top_track = select("")
        assert "›" not in top_track, f"the artist page starts with {top_track!r}, not a top track"
        ctl('type "a"')
        status = wait_status(lambda s: s["playing"] and s["track"] and s["track"]["uri"] != "" and
                             any(a.lower() in artist.lower() for a in s["track"]["artists"]),
                             f"a plays the artist {artist!r}")
        ctl("shot spotify-artist")
        print(f"PASS: ⏎ opens the artist; a plays it ({status['track']['name']!r} by {status['track']['artists']})")

        # Soloist names a rounded duration first and the file's own once it
        # opens it: aim at whatever the bar shows now.
        deadline = time.monotonic() + 10
        while True:
            dur = api("/status")["track"]["duration_ms"]
            hit = f"{DOCK} .sp-seek-hit[title='Seek to {clock((29.5 / 60) * dur)}']"
            if ctl(f"tree {hit}")["nodes"]:
                break
            assert time.monotonic() < deadline, ("no seek target", hit, texts(f"{DOCK} .sp-seek"))
            time.sleep(0.2)
        # The click targets cover exactly the bar drawn under them: a click lands
        # on the share of the track it shows (the time beside it once took a
        # fifth of their row, and clicks seeked short by up to that much).
        bar = ctl(f"dump {DOCK} .sp-seek .sf-progress .track")["elements"][0]["rect"]
        hits = ctl(f"dump {DOCK} .sp-seek-hits")["elements"][0]["rect"]
        assert abs(bar[0] - hits[0]) <= 1 and abs(bar[2] - hits[2]) <= 1, ("seek targets don't cover the bar", bar, hits)
        ctl(f"click {hit}")
        status = wait_status(lambda s: abs(s["position_ms"] - dur / 2) < 4000, "clicking mid-bar seeks to the middle")
        print(f"PASS: the seek targets cover the bar ({bar[2]:.0f} px); clicking its middle seeks to"
              f" {status['position_ms'] / 1000:.0f} s of {dur / 1000:.0f} s")

        # Lyrics: 海闊天空's are unsynced, 晴天's line-synced (Musixmatch). The
        # page opens on the first and follows playback to the second.
        ctl('type "y"')
        wait_text(TRAIL, "Lyrics: ")
        lyrics = f"{MAIN} [data-role='sp-lyrics']"
        unsynced = any("aren't synced" in t for t in texts(MAIN)) or not selected_line(timeout=3)
        assert unsynced, "海闊天空's lyrics show a sung line, but Spotify has them unsynced"
        api_post("/load", {"uri": "spotify:album:0E5yojmnEdbs68b1wM6Kla", "track": SYNCED_TRACK})
        wait_status(lambda s: s["track"] and s["track"]["uri"] == SYNCED_TRACK and s["playing"], "晴天 plays")
        api("/seek?ms=60000", "POST")
        wait_text(TRAIL, "Lyrics: 晴天")
        line = selected_line(timeout=15)
        assert line, "晴天's synced lyrics show no sung line"
        deadline = time.monotonic() + 20
        while selected_line() in ("", line):
            assert time.monotonic() < deadline, f"the sung line stays at {line!r}"
            time.sleep(0.3)
        assert texts(lyrics), "no lyrics list"
        ctl("shot spotify-lyrics")
        print(f"PASS: y shows lyrics: unsynced ones without a sung line; on the next song the page follows to"
              f" synced lyrics and the sung line moves on ({line!r} → {selected_line()!r})")
        ctl('type "y"')
        wait_text(TRAIL, "Search: beyond › ")
        assert not any("Lyrics:" in t for t in texts(TRAIL)), texts(TRAIL)

        track = api("/status")["track"]
        ctl(f"click {DOCK} .sp-artist.sp-link")
        wait_text(TRAIL, f"› {track['artists'][0]}")
        keys("Backspace")
        wait_status(lambda s: s["track"] and s["track"].get("album_uri"), "the album's URI arrives")
        time.sleep(1.2)
        ctl(f"click {DOCK} .sp-album-name.sp-link")
        wait_text(TRAIL, track["album"])
        wait_text(f"{MAIN} .sp-hero-label", "")
        nums = texts(f"{MAIN} .sp-num")
        assert nums and nums[0] in ("1", "♪", "‖"), f"the album's rows aren't numbered: {nums[:3]}"
        assert not ctl(f"tree {MAIN} .sp-thumb")["nodes"], "the album's rows repeat its cover"
        ctl("shot spotify-album")
        first = texts(f"{MAIN} [data-role='sp-rows'] .sp-row:nth-child(1) .sp-row-name")[0].rstrip("…")
        ctl(f"click {MAIN} .sp-hero-play")
        wait_status(lambda s: s["playing"] and s["track"] and s["track"]["name"].startswith(first),
                    f"the album header's ▶ plays it from {first!r}")
        crumbs = texts(f"{TRAIL} .sp-crumb")
        ctl(f"click {TRAIL} .sp-crumb.sp-link:nth-last-child(3)")
        wait_text(TRAIL, crumbs[-2])
        assert not any(crumbs[-1] in t for t in texts(TRAIL)), ("the crumb didn't go back", texts(TRAIL))
        ctl(f"click {TRAIL} .sp-crumb.sp-link:first-child")
        deadline = time.monotonic() + 5
        while texts(TRAIL) != ["Your Library"]:
            assert time.monotonic() < deadline, ("the first crumb doesn't go back to Your Library", texts(TRAIL))
            time.sleep(0.1)
        print(f"PASS: clicking the artist ({track['artists'][0]!r}) and album ({track['album']!r}) in Now Playing"
              f" opens them; the album page has its header and numbered rows, and its ▶ plays it from {first!r};"
              f" clicking {crumbs[-2]!r} in the title line goes back there, and 'Your Library' all the way back")
        track = api("/status")["track"]

        uri = track["uri"]
        was = api(f"/saved?uri={uri}")["saved"]
        def toggled_to(on):
            ctl('type "f"')
            deadline = time.monotonic() + 10
            while api(f"/saved?uri={uri}")["saved"] != on:
                assert time.monotonic() < deadline, f"f didn't set Liked Songs to {on}"
                time.sleep(0.3)
        try:
            # The block asks /saved once per track; ♡ waits for that answer.
            time.sleep(1.5)
            toggled_to(not was)
            wait_text(f"{DOCK} .sp-btn", "♥" if not was else "♡")
            toggled_to(was)
        finally:
            if api(f"/saved?uri={uri}")["saved"] != was:
                api(f"/save?uri={uri}&on={str(was).lower()}", "POST")
        print(f"PASS: f toggles ♡ (Liked Songs: {was} → {not was} → {was})")

        # q queues the chosen song; u shows it in Up next.
        select("Liked Songs")
        keys("Enter")
        wait_text(TRAIL, "Your Library › Liked Songs")
        deadline = time.monotonic() + 20
        while "Liked Songs" in selected() or selected() == "":
            assert time.monotonic() < deadline, ("Liked Songs' rows did not load", selected())
            time.sleep(0.2)
        keys("ArrowDown", "ArrowDown")
        time.sleep(0.3)
        row = selected()
        ctl('type "q"')
        deadline = time.monotonic() + 10
        while True:
            queued = [i["name"] for i in api("/queue")["items"] if i["detail"].startswith("In queue")]
            if any(name in row for name in queued):
                break
            assert time.monotonic() < deadline, ("q didn't queue the chosen song", row, queued)
            time.sleep(0.3)
        ctl('type "u"')
        wait_text(TRAIL, "Up next")
        wait_text(f"{MAIN} [data-role='sp-rows']", queued[0])
        ctl("shot spotify-up-next")
        ctl('type "u"')
        wait_text(TRAIL, "Your Library › Liked Songs")
        assert not any("Up next" in t for t in texts(TRAIL)), texts(TRAIL)
        print(f"PASS: q queues the chosen song ({queued[0]!r}); u shows it in Up next and u again closes it")

        # The quality chip restarts Soloist for the other quality. Each quality has
        # its own cache: the song cached as FLAC plays as Vorbis at 320 kbit/s.
        mouse = "spotify:track:0aNdtLrUvMMSKZ2EqEFYJK"  # has a lossless version
        def plays_as(codec):
            return wait_status(lambda s: s["player"]["state"] == "ready" and s["playing"] and s["track"]
                               and s["track"]["uri"] == mouse
                               and (s["player"]["format"] or {}).get("codec") == codec,
                               f"the song plays from {codec}", timeout=60)["player"]["format"]
        assert api("/status")["player"]["quality"] == "lossless"
        api_post("/load", {"uri": mouse})
        flac = plays_as("flac")
        wait_text(DOCK, "FLAC")
        badge = f"{DOCK} .sf-badge:nth-last-child(2)"
        assert texts(badge) == ["Quality: Lossless"], texts(badge)
        ctl(f"click {badge}")
        wait_text(badge, "Quality: 320 kbps")
        vorbis = plays_as("vorbis")
        assert vorbis["bitrate_kbps"] == 320, vorbis
        wait_text(DOCK, "Ogg Vorbis 320 kbps")
        ctl("shot spotify-quality-320")
        ctl(f"click {badge}")
        wait_text(badge, "Quality: Lossless")
        plays_as("flac")
        print(f"PASS: the quality chip switches Lossless ({flac}) → 320 kbps ({vorbis}) → Lossless; the same"
              " song plays from FLAC, then Vorbis 320 (not the cached FLAC), then FLAC, and the dock says which")

        print("DEBUG before", {k: api("/status")[k] for k in ("playing", "active")}, api("/status")["player"]["quality"])
        keys("Space")
        time.sleep(2)
        st = api("/status")
        print("DEBUG after", st["playing"], st["player"]["quality"], st["player"]["state"], texts(f"{DOCK} .sp-play"))
        print("DEBUG log", [l[-160:] for l in next(config.rglob("daemon.log")).read_text().splitlines()[-8:]])
        wait_status(lambda s: not s["playing"], "Space pauses")
        print("PASS: Space pauses")

        # The ▶ button's click reaches the block (an action named "toggle" never
        # did) and shows its effect before the daemon reports it.
        ctl(f"click {DOCK} .sp-play")
        wait_text(f"{DOCK} .sp-play", "⏸", timeout=0.5)
        wait_status(lambda s: s["playing"], "clicking ▶ plays")
        ctl(f"click {DOCK} .sp-play")
        wait_text(f"{DOCK} .sp-play", "▶", timeout=0.5)
        wait_status(lambda s: not s["playing"], "clicking ⏸ pauses")
        print("PASS: clicking ▶/⏸ plays and pauses, the button flipping at once")

        volume = f"{DOCK} .sp-volume"
        before = api("/status")["volume"]
        try:
            ctl(f"click {DOCK} .sp-vseg[title='Volume 30%']")
            wait_text(volume, "30%", timeout=2)
            wait_status(lambda s: s["volume"] == 30, "clicking the 30 % segment sets the volume")
            lit = len(ctl(f"tree {DOCK} .sp-vseg.sp-on")["nodes"])
            assert lit == 6, f"{lit} segments lit at 30 %, not 6"
            ctl("shot spotify-volume")
            ctl('type "7"')
            wait_status(lambda s: s["volume"] == 70, "7 sets 70 %")
            ctl('type "m"')
            wait_status(lambda s: s["volume"] == 0, "m mutes")
            wait_text(f"{volume} .sp-btn", "🔇")
            ctl(f"click {volume} .sp-btn")
            wait_status(lambda s: s["volume"] == 70, "clicking 🔇 unmutes to 70 %")
            print("PASS: the volume bar sets 30 % (6 of 20 segments lit), 7 sets 70 %, m mutes,"
                  " clicking 🔇 unmutes to 70 %")
        finally:
            api(f"/volume?percent={before}", "POST")

        # A narrow pane wraps instead of cutting things off: the window at 900 px
        # leaves the block's pane about 420 px wide.
        ctl("resize 900 900")
        time.sleep(1.5)
        x, _, w, _ = ctl(f"dump {MAIN}")["elements"][0]["rect"]
        right = x + w
        for part in (f"{DOCK} .sp-vbar", f"{DOCK} .sp-vol", f"{DOCK} .sp-seek", f"{DOCK} .sp-btn",
                     f"{MAIN} .sp-list-head .sf-badge"):
            boxes = [e["rect"] for e in ctl(f"dump {part}")["elements"]]
            assert boxes, f"no {part} in the narrow pane"
            past = [b for b in boxes if b[0] + b[2] > right + 1]
            assert not past, f"{part} ends past the pane's right edge ({right:.0f} px): {past}"
        keys_box = ctl(f"dump {DOCK} .sp-keys")["elements"][0]["rect"]
        assert keys_box[0] + keys_box[2] <= right + 1, f"the key line ends past the pane: {keys_box}"
        assert keys_box[3] < 24, f"the key line is {keys_box[3]:.0f} px tall: more than one line"
        ctl("shot spotify-narrow")
        print(f"PASS: in a {w:.0f} px pane the controls, volume bar, seek bar and chips wrap inside it;"
              " the key help is one line")

    with (out / "smoke-process.log").open("w") as log:
        daemon = subprocess.Popen([TERN, "daemon"], env=env, stdout=log, stderr=log)
        window = None
        try:
            deadline = time.monotonic() + 30
            while not Path(env["TERN_DAEMON_SOCKET"]).exists():
                assert daemon.poll() is None, "daemon exited; see .dev/smoke-process.log"
                assert time.monotonic() < deadline, "daemon did not open its socket"
                time.sleep(0.1)
            window = subprocess.Popen([TERN, "--control", control, "--out", str(out / "shots"),
                                       str(work)], env=env, stdout=log, stderr=log)
            deadline = time.monotonic() + 30
            while not Path(control).exists():
                assert window.poll() is None, "Tern exited; see .dev/smoke-process.log"
                assert time.monotonic() < deadline, "Tern did not open its control endpoint"
                time.sleep(0.1)
            ctl("ready")
            reload = subprocess.run([TERN, "plugin", "reload", "--json"], env=env,
                                    text=True, capture_output=True)
            catalog = json.loads(reload.stdout)
            assert not catalog["problems"], catalog
            plugin = [p for p in catalog["plugins"] if p["id"] == "spotify"]
            assert plugin and plugin[0]["status"] == "ready", [p.get("status") for p in plugin]
            print("PASS: plugin spotify loads (status ready, no problems)")

            if LIVE:
                live_checks()
            else:
                signed_out_checks()
            assert "keep omp up to date" not in json.dumps(ctl("system")), "Tern is offering to manage omp"
            print("PASS: the isolated window shows no omp offer dialog")
        finally:
            if port_open():
                try:
                    api("/quit", "POST")
                except Exception:
                    pass
            if window is not None:
                try:
                    ctl("quit")
                except Exception:
                    window.kill()
                window.wait(timeout=20)
            daemon.terminate()
            daemon.wait(timeout=20)
            publish(out, config)
