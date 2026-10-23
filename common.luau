--!strict
-- What both halves share: where the Rust daemon (`daemon/`) lives and
-- listens, starting it, and calling its JSON API. The daemon does the work
-- (sign-in, search, and running Spotify's Soloist, which plays the audio);
-- the Luau halves only draw it and send it commands.

local common = {}

common.PORT = 47821
common.BASE = "http://127.0.0.1:" .. common.PORT
common.KIND = "spotify.player"

local DIR = tern.plugin.dir
common.DAEMON_DIR = DIR .. "/daemon"
common.BIN = common.DAEMON_DIR .. "/target/release/tern-spotify"
common.CACHE = tern.plugin.data .. "/librespot"
common.LOG = tern.plugin.data .. "/daemon.log"
-- The official Soloist executable (`soloist`) and the user's API key
-- (`api-key`) go here; the daemon keeps Soloist's data and caches beside them.
common.SOLOIST = tern.plugin.data .. "/soloist"

-- What `/quality?level=` takes (daemon/src/soloist/prefs.rs `Quality`), best
-- first: `label` and `detail` in the picker, `short` on the block's chip.
export type Quality = { level: string, label: string, short: string, detail: string }
common.QUALITIES = {
	{ level = "lossless", label = "Lossless", short = "Lossless", detail = "FLAC, when Spotify has it" },
	{ level = "very_high", label = "Very high", short = "320 kbps", detail = "Ogg Vorbis, 320 kbps" },
	{ level = "high", label = "High", short = "160 kbps", detail = "Ogg Vorbis, 160 kbps" },
	{ level = "normal", label = "Normal", short = "96 kbps", detail = "Ogg Vorbis, 96 kbps" },
	{ level = "low", label = "Low", short = "24 kbps", detail = "AAC, 24 kbps" },
} :: { Quality }

function common.quality(level: string): Quality?
	for _, q in common.QUALITIES do
		if q.level == level then
			return q
		end
	end
	return nil
end

export type Track = {
	uri: string,
	name: string,
	artists: { string },
	-- In `artists`' order; empty for episodes and local files.
	artist_uris: { string }?,
	album: string,
	album_uri: string?,
	cover: string?,
	duration_ms: number,
}

-- What plays the audio (daemon/src/soloist/mod.rs `PlayerStatus`).
export type Player = {
	-- stopped | setup | starting | pairing | ready | restarting | error | expired
	state: string,
	message: string?,
	binary: string,
	api_key_file: string,
	expires_in_days: number?,
	quality: string, -- a `QUALITIES` level (asked for)
	-- What the playing track is decoded from, when Soloist said.
	format: { codec: string, bitrate_kbps: number?, sample_rate_hz: number? }?,
}

-- The daemon's `/status` (see daemon/src/engine.rs `Status`).
export type Status = {
	phase: string, -- signed_out | signing_in | connecting | ready | error
	error: string?,
	user: string?,
	-- The account's profile name (`user` is an opaque id).
	display_name: string?,
	device: string,
	player: Player,
	active: boolean,
	playing: boolean,
	loading: boolean,
	track: Track?,
	position_ms: number,
	volume: number,
	shuffle: boolean,
	repeat_mode: string, -- off | context | track
	version: string,
}

-- One row of `/search`, `/library`, `/open` or `/queue` (daemon/src/catalog.rs `Item`).
export type Item = {
	kind: string,
	uri: string,
	name: string,
	detail: string,
	context: string?,
	duration_ms: number?,
	-- Its cover (an artist's photo, a playlist's picture), thumbnail-sized.
	image: string?,
}

-- What an opened page shows above its rows (daemon/src/catalog.rs `Head`).
export type Head = {
	kind: string, -- album | artist | playlist | collection
	label: string, -- Album, Single, Playlist, Artist
	name: string,
	detail: string,
	image: string?,
}

export type Listing = { title: string?, context: string?, items: { Item }, note: string?, head: Head? }

-- The outcome of a call: the decoded body, or why there is none. `down`
-- means nothing listens on the port (the daemon isn't running).
export type Reply = { ok: boolean, body: any, error: string?, down: boolean }

local function decode(result: FetchResult): Reply
	if result.error then
		return { ok = false, body = nil, error = result.error, down = true }
	end
	local ok, body = pcall(tern.json.decode, result.body)
	if not ok then
		return { ok = false, body = nil, error = "bad reply from the player: " .. string.sub(result.body, 1, 120), down = false }
	end
	if result.status >= 400 then
		local msg = if type(body) == "table" and type(body.error) == "string" then body.error else "HTTP " .. result.status
		return { ok = false, body = body, error = msg, down = false }
	end
	return { ok = true, body = body, error = nil, down = false }
end

-- Calls `path` (`"/status"`, `"/seek?ms=1000"`) with `method`; `body` is
-- encoded as JSON. `cb` gets the reply (and, in the window, a fresh cx).
function common.call(method: string, path: string, body: any?, cb: (Reply, WindowCx?) -> ())
	local opts: FetchOptions = { method = method, timeout_ms = 20000 }
	if body ~= nil then
		opts.body = tern.json.encode(body)
		opts.headers = { ["Content-Type"] = "application/json" }
	end
	tern.fetch(common.BASE .. path, opts, function(result: FetchResult, cx: WindowCx?)
		cb(decode(result), cx)
	end)
end

-- Percent-encodes `s` for a query string.
function common.encode(s: string): string
	return (string.gsub(s, "[^%w%-%._~]", function(c: string): string
		return string.format("%%%02X", string.byte(c))
	end))
end

function common.built(): boolean
	return tern.fs.exists(common.BIN)
end

-- Starts the daemon detached (it outlives the plugin's VMs: music keeps
-- playing across reloads). A second start while one runs exits at once: the
-- port is taken. `cb` runs once the shell has forked it.
function common.start(cb: ((ok: boolean, err: string?) -> ())?)
	pcall(tern.fs.mkdir, common.CACHE)
	pcall(tern.fs.mkdir, common.SOLOIST)
	local script = 'nohup "$0" serve --port "$1" --cache "$2" --soloist "$3" >>"$4" 2>&1 </dev/null &'
	local ok, err = pcall(function()
		tern.process.run(
			{ "/bin/sh", "-c", script, common.BIN, tostring(common.PORT), common.CACHE, common.SOLOIST, common.LOG },
			{ timeout_ms = 5000 },
			function(result: ProcessResult, _)
				if cb then
					cb(result.status == 0, if result.status == 0 then nil else result.stderr)
				end
			end
		)
	end)
	if not ok and cb then
		cb(false, tostring(err))
	end
end

-- The last `bytes` of the daemon's log, for an error card.
function common.log_tail(bytes: number): string
	local ok, text = pcall(tern.fs.read, common.LOG)
	if not ok or type(text) ~= "string" then
		return ""
	end
	local s = text :: string
	if #s > bytes then
		s = string.sub(s, #s - bytes + 1)
		s = string.match(s, "\n(.*)$") or s
	end
	return s
end

-- `ms` as `m:ss` (or `h:mm:ss`).
function common.clock(ms: number): string
	local s = math.max(0, math.floor(ms / 1000))
	if s >= 3600 then
		return string.format("%d:%02d:%02d", s // 3600, (s // 60) % 60, s % 60)
	end
	return string.format("%d:%02d", s // 60, s % 60)
end

-- "Title — Artist, Artist".
function common.describe(t: Track): string
	if #t.artists == 0 then
		return t.name
	end
	return t.name .. " — " .. table.concat(t.artists, ", ")
end

-- `text` cut to `max` characters, the last one an ellipsis.
function common.clip(text: string, max: number): string
	local length = utf8.len(text)
	if length == nil or length <= max then
		return text
	end
	local cut = utf8.offset(text, max) or (#text + 1)
	return string.sub(text, 1, cut - 1) .. "…"
end

return common
