# Dumb pipe

This is an example to use [iroh](https://crates.io/crates/iroh) to create a dumb pipe to connect two machines with a QUIC connection.

Iroh will take care of hole punching and NAT traversal whenever possible, and fall back to a
relay if hole punching does not succeed.

It is also useful as a standalone tool for quick copy jobs.

This is inspired by the unix tool [netcat](https://en.wikipedia.org/wiki/Netcat). While netcat
works with IP addresses, dumbpipe works with 256 bit endpoint ids and therefore is somewhat location transparent. In addition, connections are encrypted using TLS.

# Installation

With [Cargo](https://doc.rust-lang.org/cargo/getting-started/installation.html):

```
cargo install dumbpipe
```

If you've installed [Homebrew](https://brew.sh), you can install it using the following command:

```
brew install dumbpipe
```

# Examples

## Use dumbpipe to stream video using [ffmpeg / ffplay](https://ffmpeg.org/):

This is using standard input and output.

### Sender side

On Mac OS:
```
ffmpeg -f avfoundation -r 30 -i "0" -pix_fmt yuv420p -f mpegts - | dumbpipe listen
```
On Linux:
```
ffmpeg -f v4l2 -i /dev/video0 -r 30 -preset ultrafast -vcodec libx264 -tune zerolatency -f mpegts - | dumbpipe listen
```
outputs ticket

### Receiver side
```
dumbpipe connect endpointealvvv4nwa522qhznqrblv6jxcrgnvpapvakxw5i6mwltmm6ps2r4aicamaakdu5wtjasadei2qdfuqjadakqk3t2ieq | ffplay -f mpegts -fflags nobuffer -framedrop -
```

- Adjust the ffmpeg options according to your local platform and video capture devices.
- Use ticket from sender side

## Stream from OBS Studio using WHIP

[OBS Studio](https://obsproject.com/) 30 and newer can output
[WHIP](https://datatracker.ietf.org/doc/html/draft-murillo-whip) (WebRTC-HTTP
ingestion protocol). `dumbpipe listen-whip` is the WHIP input: it terminates
WebRTC (ICE, DTLS, SRTP) on the machine next to OBS, because neither mpv nor
VLC can do that, and forwards the media over an iroh connection to
`dumbpipe connect-whip`, which plays it in a local player.

The media is forwarded unbuffered, one QUIC datagram per RTP packet, so the
latency is that of the network plus the decoder.

### The host, i.e. the machine with OBS

```
dumbpipe listen-whip --listen 127.0.0.1:8080 --bearer-token secret
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
dumbpipe connect-whip <ticket>
```

This writes the incoming RTP to local UDP ports, writes an SDP file describing
the stream, and launches [mpv](https://mpv.io/) on it:

```
mpv --no-cache --profile=low-latency --force-window=immediate /tmp/dumbpipe-5004.sdp
```

The player is picked with `--player`:

- `mpv` (default), the `low-latency` profile with no cache and
- `ffplay`, with `-fflags nobuffer` and no analysis delay
- `vlc`, with `--network-caching=100`
- `none`, which starts no player: it writes the stream description to
  `dumbpipe-<port>.sdp` in the current folder and prints where it is, so you
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
  (`dumbpipe-<port>.sdp`) in the current folder, so you can open it by hand.
  Give a path with an equals sign, `--sdp=/tmp/mine.sdp`, to name it, or a
  directory to put the default file inside it. Without `--sdp`, a launched
  player keeps it in the system temp dir.
- `--no-launch` does not start a player, so you can open the SDP file yourself,
  or point something else at the ports.
- `--buffer <ms>` gives the player a jitter buffer. The default is the lowest
  latency, which is right on a local network. See the next section for when you
  need it.

### Streaming across a long distance

On a LAN the zero-buffer settings are ideal. Across a country or an ocean the
path adds latency, jitter and packet loss, and a receiver with no buffer cannot
absorb it: the RTP receiver gives up waiting for reordered packets
(`max delay reached. need to consume packet`), decodes incomplete pictures
(`corrupted macroblock`, `error while decoding MB`) and audio drifts out of
sync (`Invalid audio PTS`, `Audio/Video desynchronisation`). That is not a
dumbpipe bug, it is the buffer being too small for the path.

Trade latency for stability:

```
dumbpipe connect-whip --buffer 300 <ticket>
```

- Start at `--buffer 300` (or `500` if it still breaks). This sets the player's
  read-ahead and the RTP reorder window, so late and reordered packets are
  caught instead of dropped.
- On the OBS side, shorten the keyframe interval to 1 second and keep the
  packet size at ~1200 bytes or lower. A short keyframe interval means a lost
  burst recovers quickly; small packets avoid fragmentation on a smaller path
  MTU, which is what turns into the huge `missed N packets` counts.
- Lower the bitrate if loss persists: less data means fewer datagrams on the
  wire and a lower chance of loss and of oversized datagrams being dropped.

You cannot have LAN-grade latency and clean playback on a lossy intercontinental
link at the same time. `--buffer` is the dial between them: `0`/unset for a
local stream, a few hundred milliseconds for a long-haul one.

### Loss and joining

The tunnel is deliberately dumb: RTP datagrams are not retransmitted, since
retransmitting late media is worse than losing it.

- A viewer that connects in the middle of a GOP receives nothing until the next
  keyframe, then everything.
- A gap in the RTP sequence numbers, and two seconds of silence, both make the
  viewer ask the host for a keyframe, which the host requests from OBS with a
  PLI.
- The host repeats the stream description every second, so a viewer that missed
  it still knows how to play the stream.
- Run with `-v` to see the packet counters, and `-vv` to see them every five
  seconds.

### Caveats

- By default the WHIP input only listens on loopback, so OBS has to run on the
  same machine as `dumbpipe listen-whip`.
- On a machine with several interfaces or a VPN, the automatically chosen ICE
  address may not be the one OBS can reach. Set it with `--ice-addr`.
- mpv and ffplay were verified end to end against the SDP the viewer writes:
  both open it, receive the RTP on the local ports and decode it. VLC also
  parsed the SDP and started its decoder, but failed to create a video output
  in the headless session this was tested on, so its playback is unverified.
  If `--player vlc` misbehaves, use `--player none` and play the SDP file
  yourself.

## Share a shell for pair- or ensemble programming with [tty-share](https://github.com/elisescu/tty-share):

Sharing a terminal session over the internet is useful for collaboration between programmers, but the public [tty-share](https://github.com/elisescu/tty-share) server isn't very reliable and, more importantly, [it is not end-to-end encrypted](https://tty-share.com/how-it-works/#end-to-end-encryption).

On the server:

```
$ dumbpipe listen-tcp --host localhost:8000 &
$ tty-share
```

On the client(s):

```
$ dumbpipe connect-tcp --addr localhost:8000 <ticket> &
$ tty-share http://localhost:8000/s/local/
```

## Forward development web server

You have a development webserver running on port 3000, and want to share it with
a colleague in another office or on the other side of the world.

### The web server
```
npm run dev
>    - Local:        http://localhost:3000
```

### The dumbpipe listener

*Listens* on an endpoint and forwards all incoming requests to the dev web
server that is listening on localhost on port 3000. Any number of connections can
flow through a single dumb pipe, but they will be separate local tcp connections.

```
dumbpipe listen-tcp --host localhost:3000
```
This command will output a ticket that can be used to connect.

### The dumbpipe connector

*Listens* on a tcp interface and port on the local machine. In this case on port 3001.
Forwards all incoming connections to the endpoint given in the ticket.

```
dumbpipe connect-tcp --addr 0.0.0.0:3001 <ticket>
```

### Testing it

You can now browse the website on port 3001.

## Forward a Unix Socket Application (e.g., Zellij)

You can forward applications that communicate over Unix sockets, like the terminal multiplexer [Zellij](https://zellij.dev/).

Note: Zellij keeps its session sockets under `$ZELLIJ_SOCKET_DIR/<VERSION>/session-name`

![image](https://github.com/user-attachments/assets/b8fbb988-57db-40cd-95e2-208e01fbaad6)

1. On the remote host (with Zellij running):

```bash
zellij --version
# zellij 0.42.2
# Forward the remote Zellij socket
# Socket path follows pattern: /tmp/zellij-0/<VERSION>/<session-name>
dumbpipe listen-unix --socket-path /tmp/zellij-0/0.42.2/remote-task-1234
```

This will give you a `<ticket>`.

2. On your local machine:

```bash
zellij --version
# zellij 0.42.1

# Create the local socket directory structure
mkdir -p /tmp/zj-remote/0.42.1

# Create a local socket connected to the remote one
dumbpipe connect-unix --socket-path /tmp/zj-remote/0.42.1/remote-task-1234 <ticket>
```

3. Attach your local Zellij client:

```bash
# In a new terminal window/tab, set the socket directory and attach
ZELLIJ_SOCKET_DIR=/tmp/zj-remote zellij attach remote-task-1234
```

# Advanced features

## Combining Listeners

You can mix and match listeners. For example, forward from a remote Unix socket to a local TCP port:

```bash
# Machine A: Listen on a Unix socket
dumbpipe listen-unix --socket-path /var/run/my-app.sock

# Machine B: Connect to it via a local TCP port
dumbpipe connect-tcp --addr 127.0.0.1:8080 <ticket>
```

## Custom ALPNs

Dumbpipe has an expert feature to specify a custom [ALPN](https://en.wikipedia.org/wiki/Application-Layer_Protocol_Negotiation) string. You can use it to interact with
existing iroh services.

E.g. here is how to interact with the iroh-blobs
protocol:

```
echo request1.bin | dumbpipe connect <ticket> --custom-alpn utf8:/iroh-bytes/2 > response1.bin
```

(`/iroh-bytes/2` is the ALPN string for the iroh-blobs protocol, which used to be called iroh-bytes.)

if request1.bin contained a valid request for the `/iroh-bytes/2` protocol, response1.bin will
now contain the response.
