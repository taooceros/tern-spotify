--!strict
-- The `player` block (kind `spotify.player`): the front end of the Rust
-- daemon in `daemon/` (librespot). It polls the daemon's `/status` once a
-- second and draws what is playing, the controls, and a list (Liked Songs
-- and playlists, or search results) to play from. It starts the daemon when
-- nothing answers, and offers to build it when it isn't built yet.
--
-- Keys: Space play/pause (takes playback over from another device when this
-- one isn't active), n/p next/previous, ←/→ seek 10 s, +/- volume, s
-- shuffle, r repeat (off → all → one), / search, l library, ↑↓/jk choose,
-- Enter play the chosen row, t play here (transfer).

local common = require("./common")

local ui = tern.ui

type Status = common.Status
type Item = common.Item
type Listing = common.Listing
type Reply = common.Reply

local POLL_MS = 1000
-- While synced lyrics show: the sung line moves on within this long.
local LYRICS_POLL_MS = 300
-- While the daemon isn't running (not built, or failed to start).
local SLOW_POLL_MS = 3000
-- How long a started daemon may take to answer before the block says so.
local START_GRACE_MS = 10000
local SEEK_MS = 10000
local VOLUME_STEP = 5
-- The volume bar: this many segments, each VOLUME_STEP percent.
local VOLUME_SEGMENTS = 100 // VOLUME_STEP
-- A volume just set shows at once; the daemon reports it within this long.
local VOLUME_PENDING_MS = 2000
-- Images (covers, photos) kept as blobs; past this many, those the playing
-- track and the page no longer show are dropped.
local MAX_IMAGES = 600
-- Images fetched at once; the rest wait in the queue.
local IMAGE_FETCHES = 6
-- Images that arrive together draw in one render.
local IMAGE_RENDER_MS = 120
-- Rows above the chosen one whose images load first, with those below it.
local IMAGE_ROWS_BEFORE = 8
-- The page header (cover, name) takes about this many list lines.
local HERO_LINES = 7
-- Build output shown on failure.
local BUILD_TAIL = 1600

-- The list's node id prefix: rows are `main.browse.list.i<n>` (region,
-- then the keys of the elements holding the list).
local LIST_KEY = "list"
local ROW_ID = "main.browse." .. LIST_KEY .. ".i"
-- The window half's route turns this link into a search prompt for the pane.
local SEARCH_LINK = "tern-spotify:search?pane="

local REPEAT_NEXT: { [string]: string } = { off = "context", context = "track", track = "off" }
local REPEAT_LABEL: { [string]: string } = { off = "Repeat off", context = "Repeat all", track = "Repeat one" }

-- One page of the browser: the library, a search, or what is inside a
-- playlist, album, artist or Liked Songs. Pages stack: ⌫ goes back.
type View = {
	title: string,
	path: string,
	items: { Item },
	-- What the whole page plays as (a playlist, album…), for "Play all".
	context: string?,
	note: string?,
	-- An opened playlist, album, artist or Liked Songs: its cover and name.
	head: common.Head?,
	error: string?,
	busy: boolean,
	sel: number,
	-- A search page, which a new search replaces rather than stacks on.
	search: boolean,
	-- A lyrics page (`lines` instead of `items`) for `track_uri`.
	lyrics: boolean,
	lines: { { ms: number, text: string } },
	synced: boolean,
	track_uri: string?,
}

type State = {
	cx: BlockCx,
	alive: boolean,
	status: Status?,
	-- probe (first poll pending), up, starting, nobinary, building, failed
	reach: string,
	problem: string?,
	started_at: number?,
	build_out: string?,
	views: { View },
	-- Whether the library loaded once since signing in.
	library_loaded: boolean,
	-- The last refused command, until the next one succeeds.
	flash: string?,
	-- A volume set from this block, shown until the daemon reports it.
	volume_pending: number?,
	volume_pending_at: number,
	-- What unmute goes back to.
	unmute_to: number?,
	-- `?`: the dock lists every key instead of the short line.
	help: boolean,
	-- The playing track's place in the library (♡): for `saved_uri`, nil
	-- while unknown.
	saved_uri: string?,
	saved: boolean?,
	-- Image URL → blob; what is being fetched; what waits, most wanted first.
	covers: { [string]: string },
	cover_count: number,
	fetching: { [string]: boolean },
	queue: { string },
	inflight: number,
	render_queued: boolean,
	ticks: number,
}

local CONTAINERS: { [string]: boolean } = { album = true, artist = true, playlist = true, collection = true }

local function top(state: State): View?
	return state.views[#state.views]
end

local function render(state: State)
	if state.alive then
		pcall(function()
			state.cx:render()
		end)
	end
end

-- Whether the block's pane still exists (a closed block's timers would
-- otherwise poll forever).
local function pane_alive(state: State): boolean
	for _, info in tern.pane.list() do
		if info.pane == state.cx.pane then
			return true
		end
	end
	return false
end

-- ── Images ───────────────────────────────────────────────────────────────
-- Covers come from Spotify's CDN, a few at a time: the playing track's
-- first, then the page's, then its rows' from just above the chosen one on
-- (a long playlist fills in where the user looks).

local function render_soon(state: State)
	if state.render_queued then
		return
	end
	state.render_queued = true
	tern.timer(IMAGE_RENDER_MS, function()
		state.render_queued = false
		render(state)
	end)
end

-- Every image shown now, most wanted first.
local function wanted(state: State): { string }
	local urls: { string } = {}
	local s = state.status
	local cover = if s and s.track then s.track.cover else nil
	if cover then
		table.insert(urls, cover)
	end
	local view = top(state)
	if view == nil or view.lyrics then
		return urls
	end
	local head = view.head
	if head and head.image then
		table.insert(urls, head.image)
	end
	local from = math.max(1, view.sel - IMAGE_ROWS_BEFORE)
	local function add(i: number)
		local image = view.items[i].image
		if image then
			table.insert(urls, image)
		end
	end
	for i = from, #view.items do
		add(i)
	end
	for i = 1, from - 1 do
		add(i)
	end
	return urls
end

local function store(state: State, url: string, blob: string)
	if state.cover_count >= MAX_IMAGES then
		local keep: { [string]: string } = {}
		local count = 0
		for _, u in wanted(state) do
			if state.covers[u] and not keep[u] then
				keep[u] = state.covers[u]
				count += 1
			end
		end
		state.covers, state.cover_count = keep, count
	end
	if not state.covers[url] then
		state.cover_count += 1
	end
	state.covers[url] = blob
end

local function pump(state: State)
	while state.inflight < IMAGE_FETCHES and #state.queue > 0 do
		local u = table.remove(state.queue, 1) :: string
		if state.covers[u] or state.fetching[u] then
			continue
		end
		state.fetching[u] = true
		state.inflight += 1
		tern.fetch(u, { timeout_ms = 15000 }, function(result: FetchResult, _)
			state.fetching[u] = nil
			state.inflight -= 1
			if not state.alive then
				return
			end
			if not result.error and result.status == 200 then
				local ok, blob = pcall(function()
					return state.cx:blob(result.body, result.headers["content-type"] or "image/jpeg")
				end)
				if ok then
					store(state, u, blob)
					render_soon(state)
				end
			end
			pump(state)
		end)
	end
end

-- Queues what the playing track and the top page show and isn't kept yet;
-- runs whenever either changes, or the chosen row does.
local function queue_images(state: State)
	local q: { string } = {}
	for _, u in wanted(state) do
		if not state.covers[u] and not state.fetching[u] then
			table.insert(q, u)
		end
	end
	state.queue = q
	pump(state)
end

local load_library: (State) -> ()
-- Runs after every status read: the ♡ and lyrics follow the playing track.
local after_status: (State, Status) -> ()

local function took_status(state: State, s: Status)
	state.reach = "up"
	state.problem = nil
	state.started_at = nil
	state.status = s
	local cover = if s.track then s.track.cover else nil
	if cover and not state.covers[cover] and not state.fetching[cover] then
		queue_images(state)
	end
	after_status(state, s)
	if s.phase == "ready" then
		if not state.library_loaded then
			state.library_loaded = true
			load_library(state)
		end
	else
		state.library_loaded = false
	end
end

local function start_daemon(state: State)
	state.reach = "starting"
	state.started_at = tern.now()
	common.start(function(ok, err)
		if not ok then
			state.reach = "failed"
			state.problem = "Could not start the player: " .. (err or "unknown error")
			render(state)
		end
	end)
end

-- Nothing answered: start the daemon, or say why it isn't running.
local function went_down(state: State)
	if not common.built() then
		if state.reach ~= "building" then
			state.reach = "nobinary"
		end
		return
	end
	if state.reach == "starting" then
		if tern.now() - (state.started_at or 0) > START_GRACE_MS then
			state.reach = "failed"
			state.problem = "The player did not start."
		end
	elseif state.reach == "up" then
		-- It ran and stopped (Spotify: Quit Player, or it crashed): say so
		-- rather than start it again behind the user's back.
		state.reach = "failed"
		state.problem = "The player stopped."
	elseif state.reach ~= "failed" and state.reach ~= "building" then
		start_daemon(state)
	end
end

-- One `/status` read; `again` schedules the next one.
local function poll(state: State, again: boolean)
	if not state.alive then
		return
	end
	common.call("GET", "/status", nil, function(reply: Reply, _)
		if not state.alive then
			return
		end
		if reply.ok then
			took_status(state, reply.body :: Status)
		elseif reply.down then
			went_down(state)
		else
			state.problem = reply.error
		end
		render(state)
		if again then
			state.ticks += 1
			if state.ticks % 5 == 0 and not pane_alive(state) then
				state.alive = false
				return
			end
			local view = top(state)
			local wait = if view and view.lyrics and view.synced then LYRICS_POLL_MS
				elseif state.reach == "up" or state.reach == "starting" or state.reach == "probe" then POLL_MS
				else SLOW_POLL_MS
			tern.timer(wait, function()
				poll(state, true)
			end)
		end
	end)
end

-- Sends a command; its reply is the status right after, and a read shortly
-- after shows its effect.
local function send(state: State, method: string, path: string, body: any?)
	common.call(method, path, body, function(reply: Reply, _)
		if not state.alive then
			return
		end
		if reply.ok then
			state.flash = nil
			took_status(state, reply.body :: Status)
		elseif reply.down then
			went_down(state)
		else
			state.flash = reply.error
		end
		render(state)
		tern.timer(350, function()
			poll(state, false)
		end)
	end)
end

-- Reads `path` into `view`; the reply lands in that page even when another
-- one was opened meanwhile.
local function fill(state: State, view: View)
	view.busy = true
	view.error = nil
	common.call("GET", view.path, nil, function(reply: Reply, _)
		if not state.alive then
			return
		end
		view.busy = false
		if reply.ok and view.lyrics then
			local body = reply.body :: any
			view.lines = body.lines or {}
			view.synced = body.synced == true
			view.note = if #view.lines == 0 then "Spotify has no lyrics for this song."
				elseif not view.synced then "These lyrics aren't synced to the song."
				elseif body.provider and body.provider ~= "" then "Lyrics by " .. body.provider
				else nil
		elseif reply.ok then
			local listing = reply.body :: Listing
			view.items = listing.items or {}
			view.note = listing.note
			view.context = listing.context
			view.head = listing.head
			if listing.title and listing.title ~= "" then
				view.title = listing.title :: string
			end
			view.sel = math.clamp(view.sel, 1, math.max(1, #view.items))
		else
			view.items = {}
			view.lines = {}
			view.note = nil
			view.error = reply.error
		end
		queue_images(state)
		render(state)
	end)
end

local function new_view(title: string, path: string, search: boolean): View
	return {
		title = title,
		path = path,
		items = {},
		context = nil,
		note = nil,
		head = nil,
		error = nil,
		busy = true,
		sel = 1,
		search = search,
		lyrics = false,
		lines = {},
		synced = false,
		track_uri = nil,
	}
end

local function push(state: State, view: View)
	table.insert(state.views, view)
	fill(state, view)
end

load_library = function(state: State)
	state.views = {}
	push(state, new_view("Your Library", "/library", false))
end

local function search(state: State, q: string)
	q = string.match(q, "^%s*(.-)%s*$") or ""
	if q == "" then
		return
	end
	local current = top(state)
	if current and current.search then
		table.remove(state.views)
	end
	push(state, new_view("Search: " .. q, "/search?q=" .. common.encode(q), true))
end

local function back(state: State): boolean
	if #state.views <= 1 then
		return false
	end
	table.remove(state.views)
	queue_images(state)
	return true
end

local function play(state: State, uri: string, track: string?)
	send(state, "POST", "/load", { uri = uri, track = track })
end

-- ⏎ on a row: a track plays inside its context (the page's playlist or
-- album, or its own album from a search); anything else opens.
local function activate(state: State, item: Item)
	if CONTAINERS[item.kind] then
		push(state, new_view(item.name, "/open?uri=" .. common.encode(item.uri), false))
	else
		local ctx = item.context
		play(state, ctx or item.uri, if ctx then item.uri else nil)
	end
end

-- `a` / ⇧⏎: the chosen album, playlist or artist plays without opening;
-- on a track, the whole page plays from its start.
local function play_all(state: State)
	local view = top(state)
	if view == nil then
		return
	end
	local item = view.items[view.sel]
	if item and CONTAINERS[item.kind] then
		play(state, item.uri, nil)
	elseif view.context then
		play(state, view.context :: string, nil)
	end
end

-- The lyrics page of the playing track, on top of the browser; `y` again
-- (or ⌫) closes it.
local function toggle_lyrics(state: State)
	local view = top(state)
	if view and view.lyrics then
		table.remove(state.views)
		return
	end
	local s = state.status
	local t = if s then s.track else nil
	if t == nil then
		state.flash = "Nothing is playing, so there are no lyrics to show."
		return
	end
	local track = t :: common.Track
	local lyrics = new_view("Lyrics: " .. track.name, "/lyrics?uri=" .. common.encode(track.uri), false)
	lyrics.lyrics = true
	lyrics.track_uri = track.uri
	push(state, lyrics)
end

-- The line sung at `ms`: the last one starting at or before it.
local function lyric_at(view: View, ms: number): number
	local at = 1
	for i, line in view.lines do
		if line.ms <= ms then
			at = i
		else
			break
		end
	end
	return at
end

local reveal: (State) -> ()

after_status = function(state: State, s: Status)
	local t = s.track
	if s.phase ~= "ready" or t == nil then
		return
	end
	local track = t :: common.Track
	-- ♡: ask once per track whether it is in Liked Songs.
	if state.saved_uri ~= track.uri then
		state.saved_uri, state.saved = track.uri, nil
		local uri = track.uri
		common.call("GET", "/saved?uri=" .. common.encode(uri), nil, function(reply: Reply, _)
			if state.alive and state.saved_uri == uri and reply.ok then
				state.saved = (reply.body :: any).saved == true
				render(state)
			end
		end)
	end
	-- An open lyrics page follows the song: a new track loads its lyrics,
	-- and the line being sung stays selected and in view.
	local view = top(state)
	if view and view.lyrics then
		if view.track_uri ~= track.uri then
			view.track_uri = track.uri
			view.title = "Lyrics: " .. track.name
			view.path = "/lyrics?uri=" .. common.encode(track.uri)
			view.lines, view.sel = {}, 1
			fill(state, view)
		elseif view.synced and #view.lines > 0 then
			local at = lyric_at(view, s.position_ms)
			if at ~= view.sel then
				view.sel = at
				reveal(state)
			end
		end
	end
end

local function ask_search(state: State)
	state.cx:open(SEARCH_LINK .. state.cx.pane)
end

local function build(state: State)
	state.reach = "building"
	state.build_out = nil
	render(state)
	local home = tern.getenv("HOME") or ""
	local path = home .. "/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:" .. (tern.getenv("PATH") or "/usr/bin:/bin")
	local ok, err = pcall(function()
		tern.process.run(
			{ "cargo", "build", "--release", "--locked" },
			{ cwd = common.DAEMON_DIR, env = { PATH = path }, timeout_ms = 30 * 60 * 1000 },
			function(result: ProcessResult, _)
				if not state.alive then
					return
				end
				if result.status == 0 and common.built() then
					state.reach = "probe"
					state.build_out = nil
					start_daemon(state)
				else
					state.reach = "nobinary"
					local out = result.stderr
					if #out > BUILD_TAIL then
						out = "…" .. string.sub(out, #out - BUILD_TAIL)
					end
					state.build_out = if result.timed_out then "The build took over 30 minutes and was stopped." else out
				end
				render(state)
			end
		)
	end)
	if not ok then
		state.reach = "nobinary"
		state.build_out = "Could not run cargo: " .. tostring(err)
	end
end

-- ── View ─────────────────────────────────────────────────────────────────

local function button(key: string, label: string, act: string, title: string, on: boolean?): Node
	return ui.el("button", {
		key = key,
		class = "sp-btn" .. (if on then " sp-on" else ""),
		title = title,
		aria = title,
		text = label,
		attrs = if on ~= nil then { ["aria-pressed"] = if on then "true" else "false" } else nil,
		actions = { click = act },
	})
end

local function chip(label: string, act: string, tone: string): Node
	local node = ui.badge(label, tone)
	local p: { [string]: any } = node.p or {}
	p.actions = { click = act }
	node.p = p
	return node
end

local function card(title: string, tone: string, children: { Node }): Node
	local c = ui.card({ ui.span(title, "strong") }, children)
	local p: { [string]: any } = c.p or {}
	p.tone = tone
	c.p = p
	return c
end

local function mono_block(text: string): Node
	return ui.el("pre", { class = "sp-log", text = text })
end

local function setup_view(state: State): Node?
	local reach = state.reach
	if reach == "probe" then
		return ui.progress(nil, "Looking for the player…")
	elseif reach == "starting" then
		return ui.progress(nil, "Starting the player…")
	elseif reach == "building" then
		return card("Building the player", "info", {
			ui.text({ ui.span("cargo build --release in ", "muted"), ui.span(common.DAEMON_DIR, "mono") }),
			ui.progress(nil, "Compiling librespot… the first build takes a few minutes"),
		})
	elseif reach == "nobinary" then
		local kids: { Node } = {
			ui.text({ ui.span("The player is a Rust program built on librespot. It is not built yet.") }),
			ui.text({ ui.span("cd " .. common.DAEMON_DIR .. " && cargo build --release", "mono") }),
			ui.row({ chip("Build it now", "build", "accent") }),
		}
		if state.build_out then
			table.insert(kids, ui.text({ ui.span("The last build failed:", "error") }))
			table.insert(kids, mono_block(state.build_out :: string))
		end
		return card("Build the Spotify player", if state.build_out then "error" else "warning", kids)
	elseif reach == "failed" then
		local kids: { Node } = { ui.text({ ui.span(state.problem or "The player is not running.") }) }
		local tail = common.log_tail(1200)
		if tail ~= "" then
			table.insert(kids, ui.text({ ui.span("Its log (" .. common.LOG .. "):", "muted") }))
			table.insert(kids, mono_block(tail))
		end
		table.insert(kids, ui.row({ chip("Start again", "start", "accent") }))
		return card("The Spotify player is not running", "error", kids)
	end
	return nil
end

local function signin_view(state: State, s: Status): Node
	local kids: { Node } = {}
	if s.error then
		table.insert(kids, ui.text({ ui.span(s.error :: string, "warning") }))
	end
	if s.phase == "signing_in" then
		table.insert(kids, ui.progress(nil, "Waiting for the browser: sign in to Spotify there"))
		table.insert(kids, ui.row({ chip("Cancel", "cancel", "neutral") }))
		return card("Signing in to Spotify", "info", kids)
	end
	table.insert(kids, ui.text({
		ui.span("Sign in with your Spotify account (Premium is needed to play). Tern then shows up as a Spotify Connect device named "),
		ui.span(s.device, "strong"),
		ui.span("."),
	}))
	table.insert(kids, ui.row({ chip("Sign in with Spotify", "login", "accent") }))
	table.insert(kids, ui.text({ ui.span("⏎ sign in", "dim") }))
	return card("Spotify", "info", kids)
end

local function connecting_view(s: Status): Node
	if s.phase == "error" then
		return card("Cannot reach Spotify", "error", {
			ui.text({ ui.span(s.error or "Connecting failed.") }),
			ui.row({ chip("Retry now", "retry", "accent"), chip("Sign out", "logout", "neutral") }),
		})
	end
	local label = if s.error then s.error :: string else "Connecting to Spotify…"
	return ui.progress(nil, label)
end

local COVER_PX = 56

local function cover_view(state: State, s: Status): Node
	local t = s.track
	local blob = if t and t.cover then state.covers[t.cover :: string] else nil
	if blob then
		return ui.el("div", { key = "cover", class = "sp-cover" }, {
			ui.node("image", { blob = blob, alt = if t then t.album else "", w = COVER_PX, h = COVER_PX }),
		})
	end
	return ui.el("div", { key = "cover", class = "sp-cover sp-cover-empty", text = "♪" })
end

-- The volume to show: one just set here until the daemon reports it (or
-- the wait runs out), else the daemon's.
local function shown_volume(state: State): number
	local pending = state.volume_pending
	local s = state.status
	if pending and s and s.volume == pending then
		state.volume_pending = nil
	elseif pending and tern.now() - state.volume_pending_at < VOLUME_PENDING_MS then
		return pending
	end
	return if s then s.volume else 0
end

-- 🔈 mutes and unmutes; then one segment per VOLUME_STEP, lit up to the
-- volume, each setting it to its own level when clicked.
local function volume_view(state: State): Node
	local v = shown_volume(state)
	local glyph = if v == 0 then "🔇" elseif v < 34 then "🔈" elseif v < 67 then "🔉" else "🔊"
	local segments: { Node } = {}
	for i = 1, VOLUME_SEGMENTS do
		local level = i * VOLUME_STEP
		segments[i] = ui.el("button", {
			key = "v" .. i,
			class = "sp-vseg" .. (if level <= v then " sp-on" else ""),
			title = "Volume " .. level .. "%",
			aria = "Volume " .. level .. "%",
			actions = { click = "volume=" .. level },
		})
	end
	return ui.el("div", { key = "volume", class = "sp-volume", attrs = { role = "group" }, aria = "Volume" }, {
		button("mute", glyph, "mute", if v == 0 then "Unmute (m)" else "Mute (m)"),
		ui.el("div", { key = "bar", class = "sp-vbar" }, segments),
		ui.el("span", { class = "sp-vol", text = v .. "%" }),
	})
end

-- A name that opens `uri` in the browser when clicked (plain text without one).
local function link(key: string, text: string, uri: string?, class: string): Node
	if uri == nil or uri == "" then
		return ui.el("span", { key = key, class = class, text = text })
	end
	return ui.el("span", {
		key = key,
		class = class .. " sp-link",
		text = text,
		title = "Open " .. text,
		attrs = { role = "link" },
		actions = { click = "open=" .. uri },
	})
end

-- Columns of the seek bar: clicking one seeks to its share of the track.
local SEEK_COLUMNS = 60

-- The progress bar under an invisible row of SEEK_COLUMNS targets.
local function seek_view(pos: number, dur: number): Node
	local label = common.clock(pos) .. " / " .. common.clock(dur)
	local targets: { Node } = {}
	if dur > 0 then
		for i = 1, SEEK_COLUMNS do
			local ms = math.floor((i - 0.5) / SEEK_COLUMNS * dur)
			targets[i] = ui.el("button", {
				key = "s" .. i,
				class = "sp-seek-hit",
				title = "Seek to " .. common.clock(ms),
				aria = "Seek to " .. common.clock(ms),
				actions = { click = "seek=" .. ms },
			})
		end
	end
	return ui.el("div", { key = "seek", class = "sp-seek" }, {
		ui.progress(if dur > 0 then pos / dur else 0, label),
		ui.el("div", { key = "hits", class = "sp-seek-hits" }, targets),
	})
end

-- The player bar under the browser, laid out as the desktop app's: the
-- song (cover, name, artists, album, ♡) on the left, the transport over the
-- seek bar in the middle, lyrics and volume on the right.
local function now_view(state: State, s: Status): Node
	local t = s.track
	local info: { Node } = {}
	if t then
		table.insert(info, ui.el("div", { class = "sp-title", text = t.name, title = t.name }))
		-- Each artist and the album open in the browser, as in the desktop app.
		local names: { Node } = {}
		for i, name in t.artists do
			if i > 1 then
				table.insert(names, ui.el("span", { text = ", " }))
			end
			table.insert(names, link("a" .. i, name, (t.artist_uris or {})[i], "sp-artist"))
		end
		table.insert(info, ui.el("div", { key = "artists", class = "sp-artists" }, names))
		if t.album ~= "" then
			table.insert(info, ui.el("div", { key = "album", class = "sp-album" }, {
				link("album", t.album, t.album_uri, "sp-album-name"),
			}))
		end
	else
		table.insert(info, ui.el("div", { class = "sp-title", text = "Nothing playing" }))
		table.insert(info, ui.el("div", { class = "sp-artists", text = "Pick something above, or search with /." }))
	end
	local play_label = if s.playing then "⏸" else "▶"
	local view = top(state)
	local saved = t ~= nil and state.saved_uri == (t :: common.Track).uri and state.saved == true
	local song: { Node } = { cover_view(state, s), ui.el("div", { key = "info", class = "sp-info" }, info) }
	if t then
		local known = state.saved_uri == (t :: common.Track).uri and state.saved ~= nil
		table.insert(song, button("save", if saved then "♥" else "♡", "save",
			if not known then "Checking Liked Songs…" elseif saved then "In Liked Songs (f)" else "Save to Liked Songs (f)", saved))
	end
	local middle: { Node } = {
		ui.el("div", { key = "controls", class = "sp-controls" }, {
			button("shuffle", "⇄", "shuffle", if s.shuffle then "Shuffle on (s)" else "Shuffle off (s)", s.shuffle),
			button("prev", "⏮", "prev", "Previous (p)"),
			ui.el("button", {
				key = "toggle",
				class = "sp-btn sp-play",
				title = if s.playing then "Pause (Space)" else "Play (Space)",
				aria = if s.playing then "Pause" else "Play",
				text = play_label,
				actions = { click = "toggle" },
			}),
			button("next", "⏭", "next", "Next (n)"),
			button(
				"repeat",
				if s.repeat_mode == "track" then "↻1" else "↻",
				"repeat",
				(REPEAT_LABEL[s.repeat_mode] or "Repeat") .. " (r)",
				s.repeat_mode ~= "off"
			),
		}),
	}
	if t then
		local dur = t.duration_ms
		local pos = math.min(s.position_ms, if dur > 0 then dur else s.position_ms)
		table.insert(middle, seek_view(pos, dur))
	end
	return ui.el("div", { key = "now", class = "sp-now" }, {
		ui.el("div", { key = "song", class = "sp-bar-song" }, song),
		ui.el("div", { key = "mid", class = "sp-bar-mid" }, middle),
		ui.el("div", { key = "side", class = "sp-bar-side" }, {
			button("lyrics", "♫", "lyrics", "Lyrics (y)", view ~= nil and view.lyrics),
			volume_view(state),
		}),
	})
end

local KIND_LABEL: { [string]: string } = {
	album = "Album ›",
	artist = "Artist ›",
	playlist = "Playlist ›",
	collection = "Songs ›",
}

-- What an image's tile shows while it loads, or when there is none.
local KIND_GLYPH: { [string]: string } = { track = "♪", album = "♪", playlist = "♫", collection = "♥" }

-- A square cover `px` wide (an artist's photo round), or its placeholder.
local function art(state: State, url: string?, kind: string, name: string, px: number, class: string): Node
	local round = if kind == "artist" then " sp-round" else ""
	local blob = if url then state.covers[url :: string] else nil
	if blob then
		return ui.el("span", { key = "art", class = class .. round }, {
			ui.node("image", { blob = blob, alt = name, w = px, h = px }),
		})
	end
	local glyph = KIND_GLYPH[kind] or string.upper(string.match(name, "^[%z\1-\127\194-\244][\128-\191]*") or "♪")
	return ui.el("span", { key = "art", class = class .. round .. " sp-art-empty", text = glyph })
end

-- An opened page's header, as the desktop app draws it: cover, kind, name,
-- who and how many, and ▶ to play the whole page.
local function hero_view(state: State, view: View): Node?
	local head = view.head
	if head == nil then
		return nil
	end
	local info: { Node } = {
		ui.el("div", { class = "sp-hero-label", text = head.label }),
		ui.el("div", { class = "sp-hero-name", text = head.name, title = head.name }),
		ui.el("div", { class = "sp-hero-detail", text = head.detail }),
	}
	if view.context then
		table.insert(info, ui.el("div", { key = "acts", class = "sp-hero-acts" }, {
			ui.el("button", {
				key = "play",
				class = "sp-hero-play",
				title = "Play " .. head.name,
				aria = "Play " .. head.name,
				text = "▶",
				actions = { click = "play_page" },
			}),
		}))
	end
	return ui.el("div", { key = "hero", class = "sp-hero" }, {
		art(state, head.image, head.kind, head.name, 120, "sp-hero-art"),
		ui.el("div", { key = "info", class = "sp-hero-info" }, info),
	})
end

-- One row of a page: its cover (a page without any, such as an album's
-- tracks, numbers its rows instead), name over detail, and the length or
-- what opening it shows. Click chooses it; double-click plays or opens it.
local function row_view(state: State, item: Item, i: number, now: boolean, playing: boolean, thumbs: boolean): Node
	local lead = if thumbs then art(state, item.image, item.kind, item.name, 40, "sp-thumb")
		else ui.el("span", { key = "art", class = "sp-num", text = if now then (if playing then "♪" else "‖") else tostring(i) })
	local length = if item.duration_ms then common.clock(item.duration_ms :: number) else nil
	local value = if now and thumbs then (if playing then "♪ " else "‖ ") .. (length or "")
		else length or KIND_LABEL[item.kind] or item.kind
	return ui.el("div", {
		key = "i" .. i,
		class = "sp-row" .. (if now then " sp-playing" else ""),
		actions = { click = "pick=" .. i, dblclick = "go=" .. i },
	}, {
		lead,
		ui.el("span", { key = "text", class = "sp-row-text" }, {
			ui.el("span", { class = "sp-row-name", text = common.clip(item.name, 80) }),
			ui.el("span", { class = "sp-row-detail", text = common.clip(item.detail, 80) }),
		}),
		ui.el("span", { key = "value", class = "sp-row-value", text = value }),
	})
end

local function list_view(state: State, view: View, s: Status, cx: BlockCx): Node
	local playing_uri = if s.track then (s.track :: common.Track).uri else nil
	local list: Node
	local n: number
	if view.lyrics then
		-- The line being sung is selected (and kept in view by `after_status`);
		-- clicking a line seeks to it.
		local items: { { [string]: any } } = {}
		for i, line in view.lines do
			items[i] = {
				label = if line.text == "" then "♪" else line.text,
				value = if view.synced then { ui.span(common.clock(line.ms), "dim") } else nil,
			}
		end
		n = #items
		list = ui.list(items :: any)
		local kids = list.c or {}
		for i = 1, n do
			local p: { [string]: any } = kids[i].p or {}
			p.key = "i" .. i
			kids[i].p = p
		end
	else
		local thumbs = false
		for _, item in view.items do
			if item.image then
				thumbs = true
				break
			end
		end
		local rows: { Node } = {}
		for i, item in view.items do
			rows[i] = row_view(state, item, i, item.uri == playing_uri, s.playing, thumbs)
		end
		n = #rows
		list = ui.node("list", {}, rows)
	end
	local p: { [string]: any } = list.p or {}
	p.key = LIST_KEY
	p.selected = if view.sel <= n and (not view.lyrics or view.synced) then ROW_ID .. view.sel else nil
	p.role = if view.lyrics then "sp-lyrics" else "sp-rows"
	p.empty = {
		ui.span(if view.busy then "Loading…" elseif view.search then "No results." else "Nothing here.", "muted"),
	}
	local hero = hero_view(state, view)
	p.max = { lines = math.max(4, cx.rows - 16 - (if hero then HERO_LINES else 0)) }
	list.p = p

	-- Library › New Music Friday: where ⌫ goes back to.
	local trail: { string } = {}
	for i, v in state.views do
		trail[i] = common.clip(v.title, if i == #state.views then 60 else 24)
	end
	local head: { Node } = { ui.el("span", { class = "sp-list-title", text = table.concat(trail, "  ›  ") }) }
	if #state.views > 1 then
		table.insert(head, chip("Back ⌫", "back", "neutral"))
	end
	-- A page with a header plays from its ▶.
	if view.context and hero == nil then
		table.insert(head, chip("Play all a", "play_all", "accent"))
	end
	table.insert(head, chip("Search /", "search", "neutral"))
	table.insert(head, chip("Library l", "library", "neutral"))
	local body: { Node } = { ui.el("div", { class = "sp-list-head" }, head) }
	if hero then
		table.insert(body, hero)
	end
	if view.error then
		table.insert(body, ui.text({ ui.span(view.error :: string, "error") }))
	elseif view.note then
		-- A synced page's note is only the credit ("Lyrics by …").
		table.insert(body, ui.text({ ui.span(view.note :: string, if view.lyrics and view.synced then "dim" else "warning") }))
	end
	table.insert(body, list)
	return ui.el("div", { key = "browse", class = "sp-browse" }, body)
end

local KEYS = "Space play/pause · n/p next/prev · ←/→ seek · +/- volume · 1–9/0 volume 10–100% · m mute · f ♡ · y lyrics · s shuffle · r repeat · / search · l library · ⏎ open/play · a play all · ⌫ back · t play here · ? hide keys"
-- One line's worth, for narrow panes; `?` shows KEYS.
local KEYS_SHORT = "Space ⏯ · n/p ⏭⏮ · / search · y lyrics · f ♡ · ? all keys"

local function dock_view(state: State, s: Status): Node
	local where: { Span | string } = {}
	if s.active then
		table.insert(where, ui.span("● Playing on " .. s.device, "ok"))
	else
		table.insert(where, ui.span("○ " .. s.device .. " is not the active device", "muted"))
	end
	local who = s.display_name or s.user
	if who then
		table.insert(where, ui.span("  ·  " .. (who :: string), "dim"))
	end
	local summary = ui.text(where)
	local sp: { [string]: any } = summary.p or {}
	sp.grow = 1
	summary.p = sp
	local row: { Node } = { summary }
	if not s.active then
		table.insert(row, chip("Play here", "transfer", "accent"))
	end
	table.insert(row, chip("Sign out", "logout", "neutral"))
	local lines: { Node } = { ui.row(row) }
	local warn = state.flash or s.error
	if warn then
		table.insert(lines, ui.text({ ui.span(warn :: string, "warning") }))
	end
	table.insert(lines, ui.el("div", {
		key = "keys",
		class = if state.help then "sp-keys sp-keys-all" else "sp-keys",
		text = if state.help then KEYS else KEYS_SHORT,
		title = KEYS,
	}))
	return ui.col(lines)
end

-- ── Input ────────────────────────────────────────────────────────────────

-- Sets the volume (0–100) and shows it at once.
local function set_volume(state: State, v: number)
	v = math.clamp(math.floor(v + 0.5), 0, 100)
	if v > 0 then
		state.unmute_to = nil
	end
	state.volume_pending = v
	state.volume_pending_at = tern.now()
	render(state)
	send(state, "POST", "/volume?percent=" .. v)
end

local function do_action(state: State, act: string, value: string?)
	local s = state.status
	if act == "toggle" then
		send(state, "POST", "/toggle")
	elseif act == "next" then
		send(state, "POST", "/next")
	elseif act == "prev" then
		send(state, "POST", "/prev")
	elseif act == "shuffle" and s then
		send(state, "POST", "/shuffle?on=" .. tostring(not s.shuffle))
	elseif act == "repeat" and s then
		send(state, "POST", "/repeat?mode=" .. (REPEAT_NEXT[s.repeat_mode] or "off"))
	elseif (act == "vup" or act == "vdown") and s then
		set_volume(state, shown_volume(state) + (if act == "vup" then VOLUME_STEP else -VOLUME_STEP))
	elseif act == "volume" and value and tonumber(value) then
		set_volume(state, tonumber(value) :: number)
	elseif act == "mute" and s then
		local v = shown_volume(state)
		if v > 0 then
			state.unmute_to = v
			set_volume(state, 0)
		else
			set_volume(state, state.unmute_to or 50)
		end
	elseif act == "open" and value and string.sub(value, 1, 8) == "spotify:" then
		-- The artist or album clicked in Now Playing (its name comes with the page).
		push(state, new_view("…", "/open?uri=" .. common.encode(value), false))
		reveal(state)
	elseif act == "seek" and value and tonumber(value) then
		send(state, "POST", "/seek?ms=" .. math.floor(tonumber(value) :: number))
	elseif act == "save" and s and s.track then
		local uri = (s.track :: common.Track).uri
		if state.saved_uri == uri and state.saved ~= nil then
			local on = not state.saved
			state.saved = on
			common.call("POST", "/save?uri=" .. common.encode(uri) .. "&on=" .. tostring(on), nil, function(reply: Reply, _)
				if not state.alive then
					return
				end
				if reply.ok then
					state.flash = nil
					state.cx:toast("success", if on then "Added to Liked Songs" else "Removed from Liked Songs", (s.track :: common.Track).name)
				elseif state.saved_uri == uri then
					state.saved = not on
					state.flash = reply.error
				end
				render(state)
			end)
		end
	elseif act == "lyrics" then
		toggle_lyrics(state)
		reveal(state)
	elseif act == "transfer" then
		send(state, "POST", "/transfer")
	elseif act == "login" then
		send(state, "POST", "/login")
	elseif act == "cancel" then
		send(state, "POST", "/login/cancel")
	elseif act == "logout" then
		state.views = {}
		send(state, "POST", "/logout")
	elseif act == "retry" then
		send(state, "POST", "/retry")
	elseif act == "library" then
		load_library(state)
	elseif act == "back" then
		back(state)
	elseif act == "play_all" then
		play_all(state)
	elseif act == "play_page" then
		-- The page header's ▶: the whole page, whichever row is chosen.
		local view = top(state)
		if view and view.context then
			play(state, view.context :: string, nil)
		end
	elseif (act == "pick" or act == "go") and value and tonumber(value) then
		-- A row clicked (chosen) or double-clicked (played or opened).
		local view = top(state)
		local i = tonumber(value) :: number
		if view and not view.lyrics and view.items[i] then
			view.sel = i
			if act == "go" then
				activate(state, view.items[i])
			end
			reveal(state)
		end
	elseif act == "search" then
		if value and value ~= "" then
			search(state, value)
		else
			ask_search(state)
		end
	elseif act == "build" then
		build(state)
	elseif act == "start" then
		state.reach = "probe"
		start_daemon(state)
	end
end

local function seek(state: State, delta: number)
	local s = state.status
	if s and s.track then
		local dur = (s.track :: common.Track).duration_ms
		local to = math.clamp(s.position_ms + delta, 0, math.max(0, dur - 1000))
		send(state, "POST", "/seek?ms=" .. math.floor(to))
	end
end

reveal = function(state: State)
	local view = top(state)
	if view == nil then
		return
	end
	-- Rows near the chosen one load their covers first.
	queue_images(state)
	local id = ROW_ID .. view.sel
	tern.timer(0, function()
		pcall(function()
			-- The sung lyric line sits mid-pane; a chosen row just in view.
			state.cx:frame({ { "reveal", id, if view.lyrics then "center" else "nearest" } })
		end)
	end)
end

local block: BlockDef<State> = {
	init = function(cx, _args, _saved)
		local state: State = {
			cx = cx,
			alive = true,
			status = nil,
			reach = "probe",
			problem = nil,
			started_at = nil,
			build_out = nil,
			views = {},
			library_loaded = false,
			flash = nil,
			volume_pending = nil,
			volume_pending_at = 0,
			unmute_to = nil,
			help = false,
			covers = {},
			cover_count = 0,
			fetching = {},
			queue = {},
			inflight = 0,
			render_queued = false,
			ticks = 0,
		}
		tern.timer(0, function()
			poll(state, true)
		end)
		return state
	end,

	title = function(state)
		local s = state.status
		if s and s.track and s.phase == "ready" then
			return (if s.playing then "▶ " else "⏸ ") .. common.clip(common.describe(s.track :: common.Track), 60)
		end
		return "Spotify"
	end,

	view = function(state, cx)
		local setup = setup_view(state)
		if setup then
			return { main = ui.col({ setup }) }
		end
		local s = state.status
		if s == nil then
			return { main = ui.col({ ui.progress(nil, "Looking for the player…") }) }
		end
		if s.phase == "signed_out" or s.phase == "signing_in" then
			return { main = ui.col({ signin_view(state, s) }) }
		elseif s.phase ~= "ready" then
			return { main = ui.col({ connecting_view(s) }) }
		end
		local view = top(state)
		local parts: { Node } = {}
		if view then
			table.insert(parts, list_view(state, view, s, cx))
		end
		-- The player bar sits at the bottom, above the device line.
		return { main = ui.col(parts), dock = ui.col({ now_view(state, s), dock_view(state, s) }) }
	end,

	key = function(state, key, _cx)
		if key.ctrl or key.meta then
			return false
		end
		local s = state.status
		local k = key.text or key.name
		if s == nil or s.phase ~= "ready" then
			if s and key.name == "enter" and s.phase == "signed_out" then
				do_action(state, "login")
			elseif s and key.name == "escape" and s.phase == "signing_in" then
				do_action(state, "cancel")
			end
			return false
		end
		local view = top(state)
		local n = if view == nil then 0 elseif view.lyrics then #view.lines else #view.items
		if key.name == "space" then
			do_action(state, "toggle")
		elseif k == "n" then
			do_action(state, "next")
		elseif k == "p" then
			do_action(state, "prev")
		elseif key.name == "left" then
			seek(state, -SEEK_MS)
		elseif key.name == "right" then
			seek(state, SEEK_MS)
		elseif k == "+" or k == "=" then
			do_action(state, "vup")
		elseif k == "-" or k == "_" then
			do_action(state, "vdown")
		elseif k == "m" then
			do_action(state, "mute")
		elseif k and string.match(k, "^%d$") then
			-- 1–9: 10–90 %, 0: 100 %.
			local d = tonumber(k) :: number
			do_action(state, "volume", tostring(if d == 0 then 100 else d * 10))
		elseif k == "s" then
			do_action(state, "shuffle")
		elseif k == "r" then
			do_action(state, "repeat")
		elseif k == "f" then
			do_action(state, "save")
		elseif k == "?" then
			state.help = not state.help
			return true
		elseif k == "y" then
			do_action(state, "lyrics")
			return true
		elseif k == "t" then
			do_action(state, "transfer")
		elseif k == "/" then
			ask_search(state)
		elseif k == "l" then
			load_library(state)
			return true
		elseif key.name == "paste" and key.text then
			search(state, key.text :: string)
			return true
		elseif key.name == "backspace" or key.name == "escape" then
			if back(state) then
				reveal(state)
				return true
			end
		elseif view == nil or n == 0 then
			return false
		elseif key.name == "up" or k == "k" then
			view.sel = math.max(1, view.sel - 1)
			reveal(state)
			return true
		elseif key.name == "down" or k == "j" then
			view.sel = math.min(n, view.sel + 1)
			reveal(state)
			return true
		elseif key.name == "home" or k == "g" then
			view.sel = 1
			reveal(state)
			return true
		elseif key.name == "end" or k == "G" then
			view.sel = n
			reveal(state)
			return true
		elseif k == "a" or (key.name == "enter" and key.shift) then
			play_all(state)
		elseif key.name == "enter" and view.lyrics and view.lines[view.sel] then
			-- ⏎ on a lyric line: sing from there.
			do_action(state, "seek", tostring(view.lines[view.sel].ms))
		elseif key.name == "enter" and view.items[view.sel] then
			activate(state, view.items[view.sel])
			reveal(state)
			return true
		end
		return false
	end,

	event = function(state, ev, _cx)
		if ev.ev == "action" and type(ev.act) == "string" then
			local act = ev.act :: string
			local value = if type(ev.value) == "string" then ev.value :: string
				elseif type(ev.value) == "number" then tostring(ev.value)
				else nil
			-- `search=<query>` from the window's prompt, split or not.
			local name, rest = string.match(act, "^([%w_]+)=(.*)$")
			if name then
				act, value = name, rest
			end
			do_action(state, act, value)
		elseif (ev.ev == "select" or ev.ev == "activate") and type(ev.item) == "string" then
			-- Lyrics lines (a page's rows send `pick` and `go` actions).
			local i = tonumber(string.sub(ev.item, #ROW_ID + 1))
			local view = top(state)
			if i and view and view.lyrics and view.lines[i] then
				-- A click selects a line; only a different line than the one
				-- being sung seeks (the follow-along selection must not).
				if ev.ev == "activate" or i ~= view.sel then
					view.sel = i
					do_action(state, "seek", tostring(view.lines[i].ms))
				end
			end
		end
	end,
}

tern.block.define("player", block)
