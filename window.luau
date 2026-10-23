--!strict
-- The window half: palette commands and keys that drive the daemon without
-- the block, a status-line segment with what is playing, and the search
-- prompt the block asks for (`tern-spotify:search?pane=N`): Tern's prompt
-- field takes IME input, which a plugin block's keys don't.

local common = require("./common")

type Status = common.Status
type Item = common.Item
type Reply = common.Reply

local POLL_MS = 3000
-- While the daemon isn't running: the window never starts it on its own.
local IDLE_POLL_MS = 15000

local last: Status? = nil
local shown = ""

local function segment_text(s: Status?): string
	if s == nil or s.phase ~= "ready" or s.track == nil or not s.active then
		return ""
	end
	return (if s.playing then "▶ " else "⏸ ") .. common.clip(common.describe(s.track :: common.Track), 48)
end

local function tick()
	common.call("GET", "/status", nil, function(reply: Reply, _)
		last = if reply.ok then reply.body :: Status else nil
		local text = segment_text(last)
		if text ~= shown then
			shown = text
			tern.chrome.refresh()
		end
		tern.timer(if reply.down then IDLE_POLL_MS else POLL_MS, function()
			tick()
		end)
	end)
end

tern.on("window_start", function(_cx: WindowCx)
	tick()
end)

tern.chrome.status(function(_pane: StatusPaneInfo): { StatusSegment }?
	if shown == "" then
		return nil
	end
	return { { text = shown, tone = "muted", command = "plugin.spotify.toggle" } }
end)

local function open_player(cx: WindowCx)
	if cx:new_block(common.KIND, {}, "beside") == nil then
		cx:toast("error", "The Spotify player is not available", "The spotify plugin's host half is not ready on this host")
	end
end

-- Runs `fn` with the daemon answering: starts it when nothing listens, and
-- opens the player block when it isn't built or signed in (the block shows
-- how to fix either).
local function with_daemon(cx: WindowCx, fn: (cx: WindowCx, s: Status) -> ())
	local function ready(reply: Reply, cx2: WindowCx)
		local s = reply.body :: Status
		if s.phase ~= "ready" then
			cx2:toast("info", "Spotify is not signed in", "Sign in from the Spotify player")
			open_player(cx2)
			return
		end
		fn(cx2, s)
	end
	common.call("GET", "/status", nil, function(reply: Reply, cx2: WindowCx?)
		local c = cx2 :: WindowCx
		if reply.ok then
			return ready(reply, c)
		elseif not reply.down then
			return c:toast("error", "Spotify player", reply.error)
		elseif not common.built() then
			c:toast("info", "The Spotify player is not built yet", "The player block can build it")
			return open_player(c)
		end
		common.start(function(ok: boolean, err: string?)
			if not ok then
				-- `start` calls back without a cx; the toast waits for the retry.
				tern.log.warn("spotify: could not start the daemon: " .. (err or ""))
			end
			tern.timer(1500, function(cx3: WindowCx?)
				common.call("GET", "/status", nil, function(again: Reply, cx4: WindowCx?)
					local c4 = (cx4 or cx3) :: WindowCx
					if again.ok then
						ready(again, c4)
					else
						c4:toast("error", "The Spotify player did not start", "See " .. common.LOG)
					end
				end)
			end)
		end)
	end)
end

local function command(method: string, path: string)
	return function(cx: WindowCx)
		with_daemon(cx, function(_cx2: WindowCx, _s: Status)
			common.call(method, path, nil, function(reply: Reply, cx3: WindowCx?)
				if not reply.ok and cx3 then
					cx3:toast("error", "Spotify", reply.error)
				end
			end)
		end)
	end
end

local function play(cx: WindowCx, item: Item)
	local ctx = item.context
	common.call("POST", "/load", { uri = ctx or item.uri, track = if ctx then item.uri else nil }, function(reply: Reply, cx2: WindowCx?)
		if cx2 then
			if reply.ok then
				cx2:toast("success", "Playing " .. item.name, item.detail)
			else
				cx2:toast("error", "Could not play " .. item.name, reply.error)
			end
		end
	end)
end

local KIND_GROUP: { [string]: string } = {
	track = "Tracks",
	album = "Albums",
	artist = "Artists",
	playlist = "Playlists",
	collection = "Library",
}

-- Lists `path`'s items in a picker and plays the chosen one.
local function pick_from(cx: WindowCx, path: string, placeholder: string)
	common.call("GET", path, nil, function(reply: Reply, cx2: WindowCx?)
		local c = cx2 :: WindowCx
		if not reply.ok then
			return c:toast("error", "Spotify", reply.error)
		end
		local listing = reply.body :: common.Listing
		if #listing.items == 0 then
			return c:toast("info", "Nothing found", listing.note)
		end
		local items: { PickItem } = {}
		for i, item in listing.items do
			items[i] = {
				label = item.name,
				detail = item.detail,
				group = KIND_GROUP[item.kind] or item.kind,
				index = i,
			}
		end
		if listing.note then
			c:toast("info", "Spotify", listing.note)
		end
		c:pick({ items = items, placeholder = placeholder, action = "play" }):next(function(choice: PickItem?, _err: string?, cx3: WindowCx?)
			if choice and cx3 then
				play(cx3, listing.items[choice.index :: number])
			end
		end)
	end)
end

local function search_prompt(cx: WindowCx, done: (q: string, cx: WindowCx) -> ())
	cx:prompt({ placeholder = "Search Spotify: songs, albums, artists, playlists", action = "search" })
		:next(function(text: string?, _err: string?, cx2: WindowCx?)
			local q = string.match(text or "", "^%s*(.-)%s*$") or ""
			if q ~= "" and cx2 then
				done(q, cx2)
			end
		end)
end

-- The block's `/`: the query goes back to that block as a `search` action.
tern.route.link(function(link: LinkRequest, cx: WindowCx): RouteDecision?
	local pane = tonumber(string.match(link.url, "^tern%-spotify:search%?pane=(%d+)$"))
	if pane == nil then
		return nil
	end
	local target = pane :: number
	search_prompt(cx, function(q: string, cx2: WindowCx)
		cx2.session:event(target, { ev = "action", id = "main.browse", act = "search=" .. q })
	end)
	return { handled = true }
end)

tern.command({ id = "open", title = "Spotify: Open Player", icon = "play", run = open_player })
tern.command({ id = "toggle", title = "Spotify: Play/Pause", icon = "play", run = command("POST", "/toggle") })
tern.command({ id = "next", title = "Spotify: Next Track", icon = "play", run = command("POST", "/next") })
tern.command({ id = "prev", title = "Spotify: Previous Track", icon = "play", run = command("POST", "/prev") })
tern.command({ id = "transfer", title = "Spotify: Play Here", icon = "play", run = command("POST", "/transfer") })
tern.command({
	id = "search",
	title = "Spotify: Search…",
	icon = "play",
	run = function(cx: WindowCx)
		with_daemon(cx, function(cx2: WindowCx, _s: Status)
			search_prompt(cx2, function(q: string, cx3: WindowCx)
				pick_from(cx3, "/search?q=" .. common.encode(q), "Play from results for " .. q)
			end)
		end)
	end,
})
tern.command({
	id = "library",
	title = "Spotify: Play from Library…",
	icon = "play",
	run = function(cx: WindowCx)
		with_daemon(cx, function(cx2: WindowCx, _s: Status)
			pick_from(cx2, "/library", "Play from your library")
		end)
	end,
})
tern.command({ id = "logout", title = "Spotify: Sign Out", icon = "play", run = command("POST", "/logout") })
tern.command({
	id = "quality",
	title = "Spotify: Audio Quality…",
	icon = "play",
	run = function(cx: WindowCx)
		with_daemon(cx, function(cx2: WindowCx, s: Status)
			local levels = { "lossless", "very_high" }
			local items: { PickItem } = {
				{ label = "Lossless", detail = "FLAC, when Spotify has it", current = s.player.quality == "lossless", index = 1 },
				{ label = "Very high", detail = "Ogg Vorbis, 320 kbps", current = s.player.quality == "very_high", index = 2 },
			}
			cx2:pick({ items = items, placeholder = "Audio quality (the player restarts to change it)", action = "choose" })
				:next(function(choice: PickItem?, _err: string?, _cx3: WindowCx?)
					if choice == nil then
						return
					end
					local level = levels[choice.index :: number]
					common.call("POST", "/quality?level=" .. level, nil, function(reply: Reply, cx4: WindowCx?)
						if cx4 then
							if reply.ok then
								cx4:toast("success", "Spotify quality: " .. choice.label)
							else
								cx4:toast("error", "Spotify", reply.error)
							end
						end
					end)
				end)
		end)
	end,
})
tern.command({
	id = "quit",
	title = "Spotify: Quit Player",
	icon = "play",
	run = function(cx: WindowCx)
		common.call("POST", "/quit", nil, function(reply: Reply, cx2: WindowCx?)
			if cx2 then
				if reply.ok then
					cx2:toast("success", "Spotify player stopped")
				elseif reply.down then
					cx2:toast("info", "The Spotify player is not running")
				end
			end
		end)
	end,
})

tern.bind("ctrl+alt+p", "plugin.spotify.toggle")
tern.bind("ctrl+alt+n", "plugin.spotify.next")
tern.bind("ctrl+alt+b", "plugin.spotify.prev")
