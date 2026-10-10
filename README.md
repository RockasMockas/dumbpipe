# StreamPipe

StreamPipe uses [iroh](https://crates.io/crates/iroh) to enable broadcasting video and audio directly p2p to friends using streaming software like OBS.

Think of it like discord screen-sharing, but with much higher bitrate/quality for both audio and video, while hooking into all of the benefits of using a local video player like mpv/vlc/ffplay (shaders, hotkeys, scripts, etc) for the viewer.

# Examples

## Use streampipe to stream video using [ffmpeg / ffplay](https://ffmpeg.org/):

This is using standard input and output.

### Sender side

On Mac OS:
```
ffmpeg -f avfoundation -r 30 -i "0" -pix_fmt yuv420p -f mpegts - | streampipe listen
```
On Linux:
```
ffmpeg -f v4l2 -i /dev/video0 -r 30 -preset ultrafast -vcodec libx264 -tune zerolatency -f mpegts - | streampipe listen
```
outputs ticket

### Receiver side
```
streampipe connect endpointealvvv4nwa522qhznqrblv6jxcrgnvpapvakxw5i6mwltmm6ps2r4aicamaakdu5wtjasadei2qdfuqjadakqk3t2ieq | ffplay -f mpegts -fflags nobuffer -framedrop -
```

- Adjust the ffmpeg options according to your local platform and video capture devices.
- Use ticket from sender side

## Stream from OBS Studio using WHIP

[OBS Studio](https://obsproject.com/) 30 and newer can output
[WHIP](https://datatracker.ietf.org/doc/html/draft-murillo-whip) (WebRTC-HTTP
ingestion protocol). `streampipe listen-whip` is the WHIP input: it terminates
WebRTC (ICE, DTLS, SRTP) on the machine next to OBS, because neither mpv nor
VLC can do that, and forwards the media over an iroh connection to
`streampipe connect-whip`, which plays it in a local player.

The media is forwarded unbuffered, one QUIC datagram per RTP packet, so the
latency is that of the network plus the decoder.

### The host, i.e. the machine with OBS

```
streampipe listen-whip --listen 127.0.0.1:8080 --bearer-token secret
```

This serves the WHIP input on `http://127.0.0.1:8080/whip` and prints a ticket which is what the viewers connect with.

In OBS, under `Settings > Stream`:

- Service: `WHIP`
- Server: `http://127.0.0.1:8080/whip` (
- Bearer Token: `secret`, the same as `--bearer-token` (bearer token is optional, can leave empty)

To accept WHIP from another machine on the local network, use
`--listen 0.0.0.0:8080` and point OBS at the address of that machine.

### The viewer

```
streampipe connect-whip <ticket>
```

This writes the incoming RTP to local UDP ports, writes an SDP file describing
the stream, and launches [mpv](https://mpv.io/) on it:

```
mpv --no-cache --profile=low-latency --force-window=immediate /tmp/streampipe-5004.sdp
```

The player is picked with `--player`:

- `mpv` (default), the `low-latency` profile with no cache and
- `ffplay`, with `-fflags nobuffer` and no analysis delay
- `vlc`, with `--network-caching=100`
- `none`, which starts no player: it writes the stream description to
  `streampipe-<port>.sdp` in the current folder and prints where it is, so you
  can open it in a player yourself

Other options:

- `--player-path <path>` runs a player binary that is not on `PATH`. The flags
  are still those of `--player`, which defaults to `mpv`: `--player-path
  /opt/mpv` runs `/opt/mpv` with the mpv flags; `--player ffplay
  --player-path /opt/ffplay` runs `/opt/ffplay` with the ffplay flags.
- `--addr 127.0.0.1:5004` is the base of the local RTP ports. Video takes the
  even port (5004), its RTCP the port above (5005), audio the next pair
  (5006/5007). An odd port is rounded up.
- `--sdp` writes the stream description to the default file
  (`streampipe-<port>.sdp`) in the current folder, so you can open it by hand.
  Give a path with an equals sign, `--sdp=/tmp/mine.sdp`, to name it, or a
  directory to put the default file inside it. Without `--sdp`, a launched
  player keeps it in the system temp dir.
- `--no-launch` does not start a player, so you can open the SDP file yourself,
  or point something else at the ports.
- `--buffer <ms>` gives the player a jitter buffer. The default is the lowest
  latency, which is right on a local network. See the next section for when you
  need it.


## Compatibility

StreamPipe is a fork of [dumbpipe](https://github.com/n0-computer/dumbpipe).
Its classic pipe modes (`listen`/`connect`, `listen-tcp`/`connect-tcp`,
`listen-unix`/`connect-unix`) speak the dumbpipe wire protocol (ALPN
`DUMBPIPEV0` with the `hello` handshake), so `streampipe connect` interoperates
with `dumbpipe listen` and vice versa. The newer features — the UDP tunnel and
the WHIP/WebRTC video pipeline — are streampipe-only and use their own
`STREAMPIPE_*` ALPNs.

