# spotify

A Tern plugin that plays Spotify the way the desktop app does. A Rust
daemon built on [librespot](https://github.com/librespot-org/librespot)
(`daemon/`) does the work: it signs in, plays audio on this Mac as a Spotify
Connect device named **Tern**, reads the catalog and your library, and owns
macOS's Now Playing and media keys. The Luau halves are only the front end.
They draw the daemon's state and send it commands over a JSON API on
`127.0.0.1:47821`.

Spotify Premium is required: librespot can't play on free accounts.

## Install

```sh
cd daemon && cargo build --release      # first build: a few minutes
tern plugin link /path/to/spotify
```

If you skip the build, the player block offers a **Build it now** button
that runs the same `cargo build`.

## Use

- **Spotify: Open Player** (palette) opens the **Spotify** block. It starts
  the daemon if nothing answers on the port. The first time, press **Sign in
  with Spotify** (or ⏎). Spotify's page opens in your browser. After you
  approve, the daemon keeps a reusable credential in the plugin's data
  folder and signs in on its own from then on.
- The block is a browser over a player bar laid out like the desktop app's:
  the song (cover, name, artists, album, ♡) on the left, shuffle, previous,
  play, next and repeat over the seek bar in the middle, lyrics and volume
  on the right; a narrow pane wraps its parts. The browser starts at **Your Library**: Liked Songs,
  then your playlists, albums and artists, most recently used first, as in
  the desktop app's sidebar. Every row shows its cover (artists' photos
  round), its name over its artists or owner, and its length. ⏎ (or a
  double-click) on a playlist, album, artist or Liked Songs opens it. An
  opened page starts with a header like the desktop app's: the big cover,
  the kind, the name, who and how many songs (an artist: monthly
  listeners), and ▶, which plays the whole page. An album's rows are
  numbered instead of repeating its cover. ⏎ or a double-click on a song
  plays it inside that list, so what follows it there plays next; a click
  only chooses the row. ⌫ goes back. The title line shows where you are
  (`Your Library › Liked Songs`); click a page in it to go back there. The
  song playing now is marked ♪.
- Covers load from Spotify's CDN a few at a time: the playing song's first,
  then the page's, then its rows' from the chosen one on, so a long
  playlist fills in where you look. Rows use Spotify's 64 px thumbnails.
- In the player bar, click an artist or the album to open it, click anywhere
  on the progress bar to seek there, ♡ (`f`) saves the song to Liked Songs,
  and ♫ (`y`) shows its lyrics. Synced lyrics keep the line being sung
  highlighted and centred, and clicking a line seeks to it. When Spotify
  has only unsynced lyrics, the page shows them and says so. The lyrics
  page follows the song as playback moves on.
- Play/pause, shuffle, repeat, volume, seeking and **Play here** show their
  effect at once, without waiting for the daemon. The daemon's status takes
  over once it reports the same thing. A command the daemon refuses goes
  back at once (the dock says why). One that librespot drops, as it does
  while Tern isn't the active device, goes back after 3 s.
- The keyboard's media keys, AirPods, Control Center and the lock screen
  show and control what plays, as they do the desktop app.
- Playback is at 320 kbit/s Ogg Vorbis (Premium's "Very high"), the best
  librespot can get: Spotify's lossless FLAC files need decryption keys it
  can't obtain. Loudness normalisation is on and only scales a track,
  never past its peak, like the desktop app's "Normal" level (librespot's
  default runs a limiter on every sample). Samples stay 32-bit float to
  the output device. A device at 44.1 kHz gets them untouched; any other
  rate (a Mac's speakers run at 48 kHz) gets them through an FFT resampler
  (`daemon/src/output.rs`), where librespot's own output resamples by
  linear interpolation and rounds to 16 bits. Played songs are cached (up
  to 2 GB), so a replay starts at once. The volume slider is a log curve
  over 30 dB (50% ≈ −15 dB). librespot's default 60 dB curve puts 50% at
  −30 dB, far quieter than the desktop app. About half a second of audio
  waits ahead of the device. A pause from Tern (the block, the palette,
  media keys) silences it at once and keeps that half second for the
  resume. librespot pauses only after that half second has played out, and
  a pause from another device still does.
- Tern is a real Spotify Connect device. Phones and the desktop app can send
  playback to it, and **Play here** (`t`) takes over playback from whatever
  device is playing now.

| Key | Does |
| --- | --- |
| `Space` | Play/pause (takes over playback when another device plays) |
| `n` / `p` | Next / previous |
| `←` / `→` | Seek 10 s |
| `+` / `-` | Volume ±5 % (or click the volume bar: each segment is 5 %) |
| `1`–`9` / `0` | Volume 10–90 % / 100 % |
| `m` | Mute / unmute (or click 🔈) |
| `f` | ♡: add the playing song to Liked Songs, or remove it |
| `y` | Lyrics of the playing song (again, or ⌫, closes them) |
| `?` | Show every key in the dock (the dock shows a one-line summary) |
| `s` / `r` | Shuffle / repeat (off → all → one) |
| `/` | Search (Tern's prompt field, which takes IME input); pasting text also searches |
| `l` | Your Library |
| `↑` `↓` / `j` `k`, `g` `G` | Choose a row |
| `⏎` | Open the chosen playlist/album/artist, or play the chosen song in its list |
| `a` / `⇧⏎` | Play the chosen playlist/album/artist without opening it; on a song, play the whole list |
| `⌫` / `Esc` | Back |
| `t` | Play here |

Palette commands work without the block: **Play/Pause** (`⌃⌥P`), **Next
Track** (`⌃⌥N`), **Previous Track** (`⌃⌥B`), **Search…** and **Play from
Library…** (a picker), **Play Here**, **Sign Out** and **Quit Player**. While
Tern is the active device, the status line shows what is playing; click it
to pause.

The daemon outlives the plugin, so music keeps playing when the block
closes or the plugin reloads. **Quit Player** stops it. When the daemon
stops, an open block says so and offers **Start again** instead of
restarting it on its own.

## Catalog: the desktop app's route, not the Web API

Search, the library and what is inside a playlist, album or artist are read
from "pathfinder", the GraphQL API Spotify's desktop and web apps use. The
daemon authorizes it with its own session (login5 token and client token).
The public Web API (api.spotify.com) isn't used: its rate limit counts per
client id, and librespot's id is shared by every librespot-based player in
the world, so it answers 429 most of the time.

Pathfinder takes persisted queries, named by a hash that changes when
Spotify ships a new client. When a hash is stale (HTTP 412), the daemon
reads the current ones from the web player's public JavaScript on
open.spotify.com, keeps them in `pathfinder.json` and retries once. If
pathfinder fails anyway, each call falls back to what librespot reads over
the session itself, and a note above the list says so:
- search: the search context (`spotify:search:<query>`), tracks only;
- the library: your rootlist (playlists only);
- a playlist, album or artist: its context's tracks, named from track
  metadata.

## Daemon

```
tern-spotify serve --cache DIR [--port PORT] [--name NAME]
```

`DIR` keeps `credentials.json`, the device id, the volume, `pathfinder.json`
and the audio cache (`audio/`). The plugin passes `<plugin data>/librespot`
and logs to `<plugin data>/daemon.log`. Media keys and Now Playing reach the
daemon on its main thread, which runs a CoreFoundation run loop. The server
runs on tokio's threads.
The routes are listed at the top of `daemon/src/api.rs`. Every command
answers with the status after it; a refused one answers
`{"error": "…"}` with a 4xx/5xx code.

`Cargo.lock` pins `vergen` to 9.0.x. With 9.1, the build script of
librespot-core 0.8.0 fails (two `vergen-lib` versions). The plugin's build
button runs `cargo build --release --locked`.

## Development

```sh
tern plugin types .                  # writes tern.d.luau for luau-lsp
cd daemon && cargo clippy --release
python3 tests/smoke.py               # real Tern: isolated config, daemon and window
python3 tests/smoke.py --live        # the same, signed in (plays a few seconds)
```

`tests/smoke.py` needs a desktop session and the built daemon. It never
touches your own Tern config. It links the plugin into a temporary config
folder, and seeds that folder's omp state so Tern doesn't show its "Let Tern
keep omp up to date?" dialog. By default it stays signed out. It checks that
the block starts the daemon and shows the sign-in card, that commands are
refused while signed out, and that **Quit Player** / **Start again** work.

`--live` copies the sign-in your own player stored into the temporary
config. It needs your player stopped, since the port is shared. It then
browses the library (checking rows show covers) into Liked Songs (its
header) and back, plays songs there with ⏎ and a double-click, searches
through the `/` prompt, opens an artist, plays it with `a`, opens an album
(numbered rows) and plays it from its header's ▶, and pauses.

Files: `common.luau` holds what both halves share (port, paths, API calls,
starting the daemon). `host.luau` is the block, `window.luau` the palette
commands, keys, status line and search prompt route, and `spotify.css` the
block's style.
