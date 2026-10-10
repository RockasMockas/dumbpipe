# StreamPipe

StreamPipe uses [iroh](https://crates.io/crates/iroh) to broadcast video and
audio directly, peer-to-peer, to friends using streaming software like OBS.

Think of it like Discord screen-sharing, but with much higher bitrate/quality
for both audio and video, while hooking into all of the benefits of using a
local video player like mpv/vlc/ffplay (shaders, hotkeys, scripts, etc.) for
the viewer.

Run `streampipe` with no arguments to open the GUI. Everything below happens
in that window.

The window has two tabs — **Watch Stream** and **Broadcast** — a status dot in
the bottom menu bar that shows what the app is doing (idle, starting,
broadcasting, watching), and a **Settings** button for the finer options.

## Broadcasting

Use the **Broadcast** tab to take a stream from OBS and share it with friends.

1. Open the **Broadcast** tab.
2. *(Optional)* Expand **Advanced settings** to set the WHIP host, WHIP port,
   an optional bearer token, and optional fixed bind IPv4/IPv6 sockets. The
   defaults are fine for most cases.
3. Press **●  Start Broadcasting**. The status dot turns green and the tab
   reveals two click-to-copy boxes:
   - **Give OBS this WHIP server URL** — point OBS at this address.
   - **Send friends this stream ticket** — the ticket your viewers paste to
     watch. Copy it and send it to them.
4. In OBS, under `Settings > Stream`:
   - Service: `WHIP`
   - Server: the WHIP URL you copied from the first box
   - Bearer Token: the token you set in Advanced settings (leave empty if you
     did not set one)
5. Start streaming in OBS. The tab shows **Waiting for OBS to start
   streaming…** until the first frames arrive, then a live **N viewer(s) live**
   count and running statistics.
6. Press **■  Stop Broadcasting** when you are done.

When not broadcasting, you can press **Refresh Relay** to update the ticket with a new relay for the next broadcast.

## Watching

Use the **Watch Stream** tab to watch someone else's broadcast in your own
local player.

1. Open the **Watch Stream** tab.
2. Paste the **stream ticket** you received from the broadcaster into the
   ticket box.
3. Pick a **Player** from the dropdown (`mpv` by default, `ffplay`, `vlc`, or
   `none` to get just the stream description and open it yourself).
4. *(Optional)* Set a **Buffer (ms)** jitter buffer. Leave it blank for the
   lowest latency (right on a local network); raise it for a lossy or
   long-distance link.
5. Press **▶  Watch Stream** (or press Enter while the ticket box is focused).
   The app connects and launches your chosen player in its own window; the
   status reads **playing in a player window**.
6. Press **■  Stop** to end the session.

The ticket box remembers the last ticket you used, so replaying a recent stream
is just a focus and Enter away.

## Settings

The **Settings** button in the bottom menu bar opens a panel with two sections:

- **Broadcasting** — the current ticket (with **Reset Ticket** to mint a new
  one) and the log verbosity level.
- **Watching** — the default player (auto-selected on the Watch tab when the
  app opens), a per-player binary path for players not on `PATH`, the default
  buffer, and the local play address.
