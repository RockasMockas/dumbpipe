#![cfg_attr(target_os = "windows", allow(unused_imports, dead_code))]
use std::{
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, UdpSocket},
    str::FromStr,
    sync::{Arc, Barrier},
    time::{Duration, Instant},
};

use streampipe::EndpointTicket;
use rand::RngExt;

// binary path
fn streampipe_bin() -> &'static str {
    env!("CARGO_BIN_EXE_streampipe")
}

/// Read `n` lines from `reader`, returning the bytes read including the newlines.
///
/// This assumes that the header lines are ASCII and can be parsed byte by byte.
fn read_ascii_lines(mut n: usize, reader: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut buf = [0u8; 1];
    let mut res = Vec::new();
    loop {
        if reader.read(&mut buf)? != 1 {
            break;
        }
        let char = buf[0];
        res.push(char);
        if char != b'\n' {
            continue;
        }
        if n > 1 {
            n -= 1;
        } else {
            break;
        }
    }
    Ok(res)
}

fn wait2() -> Arc<Barrier> {
    Arc::new(Barrier::new(2))
}

/// generate a random, non privileged port
fn random_port() -> u16 {
    rand::rng().random_range(10000u16..60000)
}

/// Tests the basic functionality of the connect and listen pair
///
/// Connect and listen both write a limited amount of data and then EOF.
/// The interaction should stop when both sides have EOF'd.
#[test]
#[ignore = "flaky"]
fn connect_listen_happy() {
    // the bytes provided by the listen command
    let listen_to_connect = b"hello from listen";
    let connect_to_listen = b"hello from connect";
    let mut listen = duct::cmd(streampipe_bin(), ["listen"])
        .env_remove("RUST_LOG") // disable tracing
        .stdin_bytes(listen_to_connect)
        .stderr_to_stdout() //
        .reader()
        .unwrap();
    // read the first 3 lines of the header, and parse the last token as a ticket
    let header = read_ascii_lines(3, &mut listen).unwrap();
    let header = String::from_utf8(header).unwrap();
    let ticket = header.split_ascii_whitespace().last().unwrap();
    let ticket = EndpointTicket::from_str(ticket).unwrap();

    let connect = duct::cmd(streampipe_bin(), ["connect", &ticket.to_string()])
        .env_remove("RUST_LOG") // disable tracing
        .stdin_bytes(connect_to_listen)
        .stderr_null()
        .stdout_capture()
        .run()
        .unwrap();

    assert!(connect.status.success());
    assert!(connect.stdout.starts_with(listen_to_connect));

    let mut listen_stdout = Vec::new();
    listen.read_to_end(&mut listen_stdout).unwrap();
    assert!(listen_stdout.starts_with(connect_to_listen));
}

/// Tests the basic functionality of the connect and listen pair
///
/// Connect and listen both write a limited amount of data and then EOF.
/// The interaction should stop when both sides have EOF'd.
#[test]
#[ignore = "flaky"]
fn connect_listen_custom_alpn_happy() {
    // the bytes provided by the listen command
    let listen_to_connect = b"hello from listen";
    let connect_to_listen = b"hello from connect";
    let mut listen = duct::cmd(
        streampipe_bin(),
        ["listen", "--custom-alpn", "utf8:mysuperalpn/0.1.0"],
    )
    .env_remove("RUST_LOG") // disable tracing
    .stdin_bytes(listen_to_connect)
    .stderr_to_stdout() //
    .reader()
    .unwrap();
    // read the first 3 lines of the header, and parse the last token as a ticket
    let header = read_ascii_lines(3, &mut listen).unwrap();
    let header = String::from_utf8(header).unwrap();
    let ticket = header.split_ascii_whitespace().last().unwrap();
    let ticket = EndpointTicket::from_str(ticket).unwrap();

    let connect = duct::cmd(
        streampipe_bin(),
        [
            "connect",
            &ticket.to_string(),
            "--custom-alpn",
            "utf8:mysuperalpn/0.1.0",
        ],
    )
    .env_remove("RUST_LOG") // disable tracing
    .stdin_bytes(connect_to_listen)
    .stderr_null()
    .stdout_capture()
    .run()
    .unwrap();

    assert!(connect.status.success());
    assert!(connect.stdout.starts_with(listen_to_connect));

    let mut listen_stdout = Vec::new();
    listen.read_to_end(&mut listen_stdout).unwrap();
    assert!(listen_stdout.starts_with(connect_to_listen));
}

#[cfg(unix)]
#[test]
fn connect_listen_ctrlc_connect() {
    use nix::{
        sys::signal::{self, Signal},
        unistd::Pid,
    };
    // the bytes provided by the listen command
    let mut listen = duct::cmd(streampipe_bin(), ["listen"])
        .env_remove("RUST_LOG") // disable tracing
        .stdin_bytes(b"hello from listen\n")
        .stderr_to_stdout() //
        .reader()
        .unwrap();
    // read the first 3 lines of the header, and parse the last token as a ticket
    let header = read_ascii_lines(3, &mut listen).unwrap();
    let header = String::from_utf8(header).unwrap();
    let ticket = header.split_ascii_whitespace().last().unwrap();
    let ticket = EndpointTicket::from_str(ticket).unwrap();

    let mut connect = duct::cmd(streampipe_bin(), ["connect", &ticket.to_string()])
        .env_remove("RUST_LOG") // disable tracing
        .stderr_null()
        .stdout_capture()
        .reader()
        .unwrap();
    // wait until we get a line from the listen process
    read_ascii_lines(1, &mut connect).unwrap();
    for pid in connect.pids() {
        signal::kill(Pid::from_raw(pid as i32), Signal::SIGINT).unwrap();
    }

    let mut tmp = Vec::new();
    // we don't care about the results. This test is just to make sure that the
    // listen command stops when the connect command stops.
    listen.read_to_end(&mut tmp).ok();
    connect.read_to_end(&mut tmp).ok();
}

#[cfg(unix)]
#[test]
fn connect_listen_ctrlc_listen() {
    use std::time::Duration;

    use nix::{
        sys::signal::{self, Signal},
        unistd::Pid,
    };
    // the bytes provided by the listen command
    let mut listen = duct::cmd(streampipe_bin(), ["listen"])
        .env_remove("RUST_LOG") // disable tracing
        .stderr_to_stdout()
        .reader()
        .unwrap();
    // read the first 3 lines of the header, and parse the last token as a ticket
    let header = read_ascii_lines(3, &mut listen).unwrap();
    let header = String::from_utf8(header).unwrap();
    let ticket = header.split_ascii_whitespace().last().unwrap();
    let ticket = EndpointTicket::from_str(ticket).unwrap();

    let mut connect = duct::cmd(streampipe_bin(), ["connect", &ticket.to_string()])
        .env_remove("RUST_LOG") // disable tracing
        .stderr_null()
        .stdout_capture()
        .reader()
        .unwrap();
    std::thread::sleep(Duration::from_secs(1));
    for pid in listen.pids() {
        signal::kill(Pid::from_raw(pid as i32), Signal::SIGINT).unwrap();
    }

    let mut tmp = Vec::new();
    // we don't care about the results. This test is just to make sure that the
    // listen command stops when the connect command stops.
    listen.read_to_end(&mut tmp).ok();
    connect.read_to_end(&mut tmp).ok();
}

// TODO: figure out why this is flaky on windows
#[test]
#[cfg(unix)]
#[ignore = "flaky"]
fn listen_tcp_happy() {
    let b1 = wait2();
    let b2 = b1.clone();
    let port = random_port();
    // start a dummy tcp server and wait for a single incoming connection
    let host_port = format!("localhost:{port}");
    let host_port_2 = host_port.clone();
    std::thread::spawn(move || {
        let server = TcpListener::bind(host_port_2).unwrap();
        b1.wait();
        let (mut stream, _addr) = server.accept().unwrap();
        stream.write_all(b"hello from tcp").unwrap();
        stream.flush().unwrap();
        drop(stream);
    });
    // wait for the tcp listener to start
    b2.wait();
    // start a streampipe listen-tcp process
    let mut listen_tcp = duct::cmd(streampipe_bin(), ["listen-tcp", "--host", &host_port])
        .env_remove("RUST_LOG") // disable tracing
        .stderr_to_stdout() //
        .reader()
        .unwrap();
    let header = read_ascii_lines(4, &mut listen_tcp).unwrap();
    let header = String::from_utf8(header).unwrap();
    let ticket = header.split_ascii_whitespace().last().unwrap();
    let ticket = EndpointTicket::from_str(ticket).unwrap();
    // poke the listen-tcp process with a connect command
    let connect = duct::cmd(streampipe_bin(), ["connect", &ticket.to_string()])
        .env_remove("RUST_LOG") // disable tracing
        .stderr_null()
        .stdout_capture()
        .stdin_bytes(b"hello from connect")
        .run()
        .unwrap();
    assert!(connect.status.success());
    assert!(connect.stdout.starts_with(b"hello from tcp"));
}

#[test]
fn connect_tcp_happy() {
    let port = random_port();
    let host_port = format!("localhost:{port}");
    // start a streampipe listen process just so the connect-tcp command has something to connect to
    let mut listen = duct::cmd(streampipe_bin(), ["listen"])
        .env_remove("RUST_LOG") // disable tracing
        .stdin_bytes(b"hello from listen\n")
        .stderr_to_stdout() //
        .reader()
        .unwrap();
    let header = read_ascii_lines(3, &mut listen).unwrap();
    let header = String::from_utf8(header).unwrap();
    let ticket = header.split_ascii_whitespace().last().unwrap();
    let ticket = EndpointTicket::from_str(ticket).unwrap();
    let ticket = ticket.to_string();

    // start a streampipe connect-tcp process
    let _connect_tcp = duct::cmd(
        streampipe_bin(),
        ["connect-tcp", "--addr", &host_port, &ticket],
    )
    .env_remove("RUST_LOG") // disable tracing
    .stderr_to_stdout() //
    .reader()
    .unwrap();
    std::thread::sleep(Duration::from_secs(1));

    //
    let mut conn = TcpStream::connect(host_port).unwrap();
    conn.write_all(b"hello from tcp").unwrap();
    conn.flush().unwrap();
    let mut buf = Vec::new();
    conn.read_to_end(&mut buf).unwrap();
    assert_eq!(&buf, b"hello from listen\n");
}

/// Integration test for udp datagram tunneling.
///
/// A dummy udp echo server stands in for an application that does its own
/// retransmission, like an SRT listener.
///
/// - `listen-udp` forwards datagrams to the echo server.
/// - `connect-udp` exposes a local udp port that feeds the tunnel.
/// - Small datagrams must survive the round trip, oversized ones (larger than
///   what fits into a single QUIC datagram) must be dropped without killing the
///   tunnel.
#[test]
fn udp_roundtrip() {
    /// Drain all pending datagrams, so that a late reply to a previous probe
    /// cannot be mistaken for the reply we are waiting for.
    fn drain(sock: &UdpSocket, buf: &mut [u8]) {
        sock.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        while sock.recv_from(buf).is_ok() {}
    }

    /// Send a datagram of `size` bytes and wait for it to come back.
    ///
    /// Keeps probing until `timeout` has passed, since establishing the
    /// connection and learning the peer address takes a moment.
    fn roundtrip(
        sock: &UdpSocket,
        addr: SocketAddr,
        size: usize,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Option<usize> {
        drain(sock, buf);
        let data = vec![b'x'; size];
        let deadline = Instant::now() + timeout;
        loop {
            sock.send_to(&data, addr).unwrap();
            match sock.recv_from(buf) {
                Ok((len, _)) => return Some(len),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) && Instant::now() < deadline =>
                {
                    continue;
                }
                Err(_) => return None,
            }
        }
    }

    let echo_port = random_port();
    let tunnel_port = random_port();
    let echo_addr: SocketAddr = format!("127.0.0.1:{echo_port}").parse().unwrap();
    let tunnel_addr: SocketAddr = format!("127.0.0.1:{tunnel_port}").parse().unwrap();

    // start a dummy udp echo server
    std::thread::spawn(move || {
        let server = UdpSocket::bind(("127.0.0.1", echo_port)).unwrap();
        let mut buf = [0u8; 65535];
        while let Ok((len, from)) = server.recv_from(&mut buf) {
            let _ = server.send_to(&buf[..len], from);
        }
    });

    // forward incoming datagrams to the echo server
    let mut listen_udp = duct::cmd(
        streampipe_bin(),
        ["listen-udp", "--host", &echo_addr.to_string()],
    )
    .env_remove("RUST_LOG") // disable tracing
    .stderr_to_stdout()
    .reader()
    .unwrap();
    let header = read_ascii_lines(4, &mut listen_udp).unwrap();
    let header = String::from_utf8(header).unwrap();
    let ticket = header.split_ascii_whitespace().last().unwrap();
    let ticket = EndpointTicket::from_str(ticket).unwrap().to_string();

    // expose a local udp port that feeds the tunnel
    let _connect_udp = duct::cmd(
        streampipe_bin(),
        ["connect-udp", "--addr", &tunnel_addr.to_string(), &ticket],
    )
    .env_remove("RUST_LOG") // disable tracing
    .stderr_null()
    .reader()
    .unwrap();

    // an unconnected socket, so that icmp errors do not poison the tunnel
    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut buf = [0u8; 65535];

    // datagrams that fit into a quic datagram must come back unchanged
    for size in [10usize, 500, 1000] {
        let reply = roundtrip(
            &client,
            tunnel_addr,
            size,
            &mut buf,
            Duration::from_secs(20),
        );
        assert_eq!(
            reply,
            Some(size),
            "{size} byte datagram did not survive the tunnel"
        );
    }

    // an oversized datagram must be dropped, and must not take the tunnel down
    let reply = roundtrip(&client, tunnel_addr, 4096, &mut buf, Duration::from_secs(2));
    assert_eq!(reply, None, "oversized datagram should have been dropped");
    let reply = roundtrip(&client, tunnel_addr, 100, &mut buf, Duration::from_secs(20));
    assert_eq!(reply, Some(100), "tunnel should still work after a drop");
}

/// Integration test for Unix-domain socket tunneling.
///
/// Validates end-to-end operation between `listen-unix` and `connect-unix`:
/// - A dummy backend server echoes a reply.
/// - `listen-unix` connects to the backend and exposes a ticket.
/// - `connect-unix` consumes the ticket and exposes a new Unix socket.
/// - The test exchanges messages to assert correct data flow.
#[cfg(all(test, unix))]
mod unix_socket_tests {
    use std::{
        io::{BufRead, Read, Write},
        net::Shutdown,
        os::unix::net::{UnixListener, UnixStream},
        path::{Path, PathBuf},
        sync::{Arc, Barrier},
        time::{Duration, Instant},
    };

    use tempfile::TempDir;

    use super::*;

    /// Polls until the condition returns true or timeout is reached.
    fn wait_until<F>(timeout: Duration, mut condition: F)
    where
        F: FnMut() -> bool,
    {
        let deadline = Instant::now() + timeout;
        while !condition() {
            if Instant::now() >= deadline {
                panic!("timeout waiting for condition");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Waits until a filesystem path exists.
    fn wait_for_path<P: AsRef<Path>>(path: P, timeout: Duration) {
        let p = path.as_ref().to_path_buf();
        wait_until(timeout, move || p.exists());
    }

    /// Generate a temp directory with a Unix socket path
    fn temp_socket_path() -> (TempDir, PathBuf) {
        let temp_dir = tempfile::tempdir().unwrap();
        let socket_path = temp_dir.path().join("test.sock");
        (temp_dir, socket_path)
    }

    /// Helper to drain stderr from a process in a background thread
    fn drain_stderr(
        stderr: std::process::ChildStderr,
        prefix: &'static str,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let reader = std::io::BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                eprintln!("[{prefix}] {line}");
            }
        })
    }

    /// A dummy unix server that accepts multiple connections and handles them properly.
    fn dummy_unix_server(
        socket_path: PathBuf,
        barrier: Arc<Barrier>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let _ = std::fs::remove_file(&socket_path);
            let listener = UnixListener::bind(&socket_path).unwrap();
            barrier.wait();
            // Accept connections in a loop
            for stream in listener.incoming() {
                if let Ok(mut stream) = stream {
                    // Handle each connection in a new thread
                    std::thread::spawn(move || {
                        let mut buf = vec![0; 1024];
                        // Block here waiting for data from the client via the proxy
                        if let Ok(n) = stream.read(&mut buf) {
                            if n > 0 {
                                // once we get data, write a response
                                if stream.write_all(b"hello from unix").is_ok() {
                                    // cleanly shutdown the write side
                                    stream.shutdown(Shutdown::Write).ok();
                                }
                            }
                        }
                        // now drain the read side to allow the client to close gracefully
                        while stream.read(&mut buf).unwrap_or(0) > 0 {}
                    });
                } else {
                    break;
                }
            }
        })
    }

    #[test]
    fn unix_socket_roundtrip() {
        // Create temp socket paths for the backend and the client-facing side.
        let (_tmp_dir, backend_sock) = temp_socket_path();
        let client_sock = backend_sock.with_extension("client");

        // Barrier to sync backend server readiness.
        let barrier = Arc::new(Barrier::new(2));

        // Spawn a dummy backend server.
        let _backend_thread = dummy_unix_server(backend_sock.clone(), barrier.clone());

        // Wait for the backend to be ready.
        barrier.wait();

        // Actively probe the backend server to ensure it's accepting connections.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if UnixStream::connect(&backend_sock).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if UnixStream::connect(&backend_sock).is_err() {
            panic!("backend server not connectable after 5s");
        }

        // Launch listen-unix targeting the backend.
        let mut listen_proc = std::process::Command::new(streampipe_bin())
            .args([
                "listen-unix",
                "--socket-path",
                backend_sock.to_str().unwrap(),
            ])
            .env_remove("RUST_LOG")
            .stdout(std::process::Stdio::null()) // We don't need stdout
            .stderr(std::process::Stdio::piped()) // We must read stderr
            .spawn()
            .expect("spawn listen-unix");

        // Extract the ticket from the stderr output.
        let listen_stderr = listen_proc.stderr.take().unwrap();
        let mut ticket = String::new();
        let mut stderr_reader = std::io::BufReader::new(listen_stderr);
        for line in stderr_reader.by_ref().lines() {
            let line = line.unwrap();
            eprintln!("[listen-unix-stderr] {line}");
            if line.contains("connect-unix") {
                ticket = line.split_whitespace().last().unwrap().to_owned();
                break;
            }
        }
        assert!(!ticket.is_empty(), "Failed to get ticket");

        // Continue draining listen-unix stderr using helper
        let listen_stderr_thread = std::thread::spawn(move || {
            for line in stderr_reader.lines().map_while(Result::ok) {
                eprintln!("[listen-unix-stderr] {line}");
            }
        });

        // Launch connect-unix, exposing the client socket.
        let mut connect_proc = std::process::Command::new(streampipe_bin())
            .args([
                "connect-unix",
                "--socket-path",
                client_sock.to_str().unwrap(),
                &ticket,
            ])
            .env_remove("RUST_LOG")
            .stdout(std::process::Stdio::null()) // We don't need stdout
            .stderr(std::process::Stdio::piped()) // We must read stderr
            .spawn()
            .expect("spawn connect-unix");

        // Drain the stderr of the connect process using helper
        let connect_stderr = connect_proc.stderr.take().unwrap();
        let connect_stderr_thread = drain_stderr(connect_stderr, "connect-unix-stderr");

        // Wait for connect-unix to create its socket.
        wait_for_path(&client_sock, Duration::from_secs(5));

        // Perform the end-to-end exchange.
        let mut client = UnixStream::connect(&client_sock).expect("connect to client socket");
        client
            .write_all(b"hello from client")
            .expect("client write");

        // Don't shutdown write immediately - let the backend respond first
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).expect("client read");
        assert_eq!(&reply, b"hello from unix");

        // Clean up child processes.
        listen_proc.kill().ok();
        listen_proc.wait().ok();
        connect_proc.kill().ok();
        connect_proc.wait().ok();
        listen_stderr_thread.join().ok();
        connect_stderr_thread.join().ok();
    }
}

/// Integration tests for the WHIP input and the webrtc viewer.
///
/// These do not need OBS: they speak the WHIP HTTP protocol directly, so they
/// prove that the ingest behaves like a WHIP server, and that the viewer dials
/// the host and waits for media.
///
/// Real media does not flow here, because that needs a publisher that completes
/// ICE, DTLS and SRTP. The media path itself, including the keyframe gate, is
/// covered by the unit tests in `streampipe::webrtc`.
#[cfg(test)]
mod whip_tests {
    use std::{
        process::{Child, Command, Stdio},
        sync::Mutex,
    };

    use super::*;

    /// How long to wait for a process to get somewhere.
    ///
    /// Generous, because an iroh endpoint has to find its relays before the
    /// WHIP server is even bound, and that depends on the network.
    const READY: Duration = Duration::from_secs(30);

    /// A WHIP offer in the shape OBS Studio 30 posts: bundled, sendonly,
    /// H264 with rtx and opus, ICE credentials but no candidates yet.
    fn offer() -> String {
        let fingerprint = "00:01:02:03:04:05:06:07:08:09:0A:0B:0C:0D:0E:0F:10:11:12:13:14:15:16:17:18:19:1A:1B:1C:1D:1E:1F";
        format!(
            "v=0\r\n\
             o=- 4611731400498476682 2 IN IP4 127.0.0.1\r\n\
             s=-\r\n\
             t=0 0\r\n\
             a=group:BUNDLE 0 1\r\n\
             m=video 9 UDP/TLS/RTP/SAVPF 96 97\r\n\
             c=IN IP4 0.0.0.0\r\n\
             a=rtcp-mux\r\n\
             a=ice-ufrag:aBcD\r\n\
             a=ice-pwd:012345678901234567890123456789\r\n\
             a=fingerprint:sha-256 {fingerprint}\r\n\
             a=setup:actpass\r\n\
             a=mid:0\r\n\
             a=sendonly\r\n\
             a=rtpmap:96 H264/90000\r\n\
             a=fmtp:96 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n\
             a=rtpmap:97 rtx/90000\r\n\
             a=fmtp:97 apt=96\r\n\
             m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
             c=IN IP4 0.0.0.0\r\n\
             a=rtcp-mux\r\n\
             a=ice-ufrag:aBcD\r\n\
             a=ice-pwd:012345678901234567890123456789\r\n\
             a=fingerprint:sha-256 {fingerprint}\r\n\
             a=setup:actpass\r\n\
             a=mid:1\r\n\
             a=sendonly\r\n\
             a=rtpmap:111 opus/48000/2\r\n\
             a=fmtp:111 minptime=10;useinbandfec=1\r\n"
        )
    }

    /// A trickle ICE fragment with two host candidates.
    fn trickle() -> &'static str {
        "a=candidate:aBcD 1 udp 2130706431 127.0.0.1 50000 typ host\r\n\
         a=candidate:aBcD 2 udp 2130706431 127.0.0.1 50001 typ host\r\n"
    }

    /// An answer, split into what a test wants to look at.
    #[derive(Debug)]
    struct Reply {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl Reply {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        }
    }

    /// Build a raw HTTP/1.1 request, so the tests need no HTTP client.
    fn request(method: &str, path: &str, headers: &[(&str, String)], body: &str) -> String {
        let mut req =
            format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
        for (name, value) in headers {
            req.push_str(&format!("{name}: {value}\r\n"));
        }
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
        req.push_str("\r\n");
        req.push_str(body);
        req
    }

    /// A `Content-Type` header, the one most requests need.
    fn content_type(ty: &str) -> Vec<(&'static str, String)> {
        vec![("Content-Type", ty.to_string())]
    }

    /// A `Content-Type` plus the `Authorization` and `If-Match` headers a
    /// WHIP client sends once it has a resource.
    fn headers(ty: &str, auth: Option<&str>, etag: Option<&str>) -> Vec<(&'static str, String)> {
        let mut headers = content_type(ty);
        if let Some(auth) = auth {
            headers.push(("Authorization", auth.to_string()));
        }
        if let Some(etag) = etag {
            headers.push(("If-Match", etag.to_string()));
        }
        headers
    }

    /// Send a raw request, retrying until the WHIP server has bound its port.
    fn send(port: u16, request: &str) -> Reply {
        let deadline = Instant::now() + READY;
        loop {
            match TcpStream::connect(("127.0.0.1", port)) {
                Ok(mut stream) => {
                    stream.write_all(request.as_bytes()).unwrap();
                    stream.flush().unwrap();
                    let mut buf = Vec::new();
                    stream.read_to_end(&mut buf).unwrap();
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
                    let mut lines = head.lines();
                    let status_line = lines.next().expect("a status line");
                    let status = status_line
                        .split_ascii_whitespace()
                        .nth(1)
                        .expect("a status code")
                        .parse()
                        .expect("a numeric status code");
                    let headers = lines
                        .filter_map(|line| line.split_once(':'))
                        .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
                        .collect();
                    return Reply {
                        status,
                        headers,
                        body: body.to_string(),
                    };
                }
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                    assert!(Instant::now() < deadline, "no whip server on port {port}");
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(e) => panic!("error talking to the whip server on port {port}: {e}"),
            }
        }
    }

    /// Collect the pipes of a child process into a string a test can poll.
    ///
    /// The pipes are taken from the child, so the child stays usable to kill.
    fn collect(child: &mut Child) -> Arc<Mutex<String>> {
        let out = Arc::new(Mutex::new(String::new()));
        let mut pipes: Vec<Box<dyn std::io::Read + Send>> = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            pipes.push(Box::new(stdout));
        }
        if let Some(stderr) = child.stderr.take() {
            pipes.push(Box::new(stderr));
        }
        for pipe in pipes {
            let out = out.clone();
            std::thread::spawn(move || {
                let mut pipe = std::io::BufReader::new(pipe);
                let mut buf = [0u8; 4096];
                loop {
                    match pipe.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            out.lock()
                                .unwrap()
                                .push_str(&String::from_utf8_lossy(&buf[..n]));
                        }
                    }
                }
            });
        }
        out
    }

    /// Wait until the collected output contains `pattern`.
    fn wait_for(out: &Arc<Mutex<String>>, pattern: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let text = out.lock().unwrap().clone();
            if text.contains(pattern) {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "never saw {pattern:?} in:\n{text}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Parse the ticket a `listen-whip` process prints.
    ///
    /// Scans every token, because the ticket shares the output with logging.
    fn ticket(out: &Arc<Mutex<String>>) -> EndpointTicket {
        let text = wait_for(out, "connect-whip", READY);
        text.split_ascii_whitespace()
            .filter_map(|token| EndpointTicket::from_str(token).ok())
            .next()
            .expect("a ticket")
    }

    /// Start `streampipe listen-whip` on a random loopback port.
    fn listen(extra: &[&str]) -> (u16, Child, Arc<Mutex<String>>) {
        let port = random_port();
        let addr = format!("127.0.0.1:{port}");
        let mut cmd = Command::new(streampipe_bin());
        cmd.arg("listen-whip")
            .arg("--listen")
            .arg(&addr)
            .args(extra)
            .env_remove("RUST_LOG")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn listen-whip");
        let out = collect(&mut child);
        (port, child, out)
    }

    /// The bearer token gate: nothing but the right token may offer media.
    #[test]
    fn whip_bearer_token() {
        let (port, mut child, _out) = listen(&["--bearer-token", "s3cret"]);
        let offer = offer();

        let reply = send(port, &request("POST", "/whip", &[], &offer));
        assert_eq!(reply.status, 401, "no token at all: {reply:?}");

        let reply = send(
            port,
            &request(
                "POST",
                "/whip",
                &headers("application/sdp", Some("Bearer nope"), None),
                &offer,
            ),
        );
        assert_eq!(reply.status, 401, "wrong token: {reply:?}");

        let reply = send(port, &request("DELETE", "/whip/whatever", &[], ""));
        assert_eq!(reply.status, 401, "unauthorized delete: {reply:?}");

        let reply = send(
            port,
            &request(
                "POST",
                "/whip",
                &headers("application/sdp", Some("Bearer s3cret"), None),
                &offer,
            ),
        );
        assert_eq!(reply.status, 201, "{reply:?}");
        assert_eq!(reply.header("content-type"), Some("application/sdp"));
        assert!(reply.body.contains("m=video"), "{}", reply.body);

        child.kill().ok();
        child.wait().ok();
    }

    /// The negotiation itself: an answer with the right headers, trickle, and
    /// the methods a WHIP client is not allowed to use.
    #[test]
    fn whip_answer_and_methods() {
        let (port, mut child, _out) = listen(&[]);
        let offer = offer();
        let sdp = content_type("application/sdp");

        let reply = send(port, &request("OPTIONS", "/whip", &[], ""));
        assert_eq!(reply.status, 204, "{reply:?}");
        assert_eq!(reply.header("accept-post"), Some("application/sdp"));

        let reply = send(
            port,
            &request("POST", "/whip", &content_type("text/plain"), "hi"),
        );
        assert_eq!(reply.status, 415, "wrong content type: {reply:?}");

        let reply = send(port, &request("GET", "/whip", &[], ""));
        assert_eq!(reply.status, 405, "get: {reply:?}");

        let reply = send(port, &request("POST", "/whip", &sdp, &offer));
        assert_eq!(reply.status, 201, "{reply:?}");
        assert_eq!(reply.header("content-type"), Some("application/sdp"));
        let location = reply.header("location").expect("a location").to_string();
        assert!(location.starts_with("/whip/"), "{location}");
        let etag = reply.header("etag").expect("an etag").to_string();
        assert!(!etag.is_empty());
        assert!(reply.body.starts_with("v=0"), "{}", reply.body);
        assert!(reply.body.contains("m=video"), "{}", reply.body);
        // we receive, we never send
        assert!(reply.body.contains("a=recvonly"), "{}", reply.body);
        assert!(!reply.body.contains("a=sendonly"), "{}", reply.body);

        let reply = send(
            port,
            &request(
                "PATCH",
                &location,
                &content_type("application/trickle-ice-sdpfrag"),
                trickle(),
            ),
        );
        assert_eq!(reply.status, 204, "trickle: {reply:?}");

        let reply = send(port, &request("DELETE", &location, &[], ""));
        assert_eq!(reply.status, 200, "delete: {reply:?}");

        child.kill().ok();
        child.wait().ok();
    }

    /// A renegotiation must match the entity tag of the current session.
    #[test]
    fn whip_renegotiation_etag() {
        let (port, mut child, _out) = listen(&[]);
        let offer = offer();
        let sdp = content_type("application/sdp");

        let reply = send(
            port,
            &request(
                "PUT",
                "/whip",
                &headers("application/sdp", None, Some("\"streampipe-0\"")),
                &offer,
            ),
        );
        assert_eq!(reply.status, 412, "stale etag: {reply:?}");

        let reply = send(port, &request("POST", "/whip", &sdp, &offer));
        assert_eq!(reply.status, 201, "{reply:?}");
        let location = reply.header("location").expect("a location").to_string();
        let etag = reply.header("etag").expect("an etag").to_string();

        let reply = send(
            port,
            &request(
                "PUT",
                &location,
                &headers("application/sdp", None, Some(&etag)),
                &offer,
            ),
        );
        assert_eq!(reply.status, 201, "renegotiation with the etag: {reply:?}");

        child.kill().ok();
        child.wait().ok();
    }

    /// The viewer dials the host and waits for media without a player.
    #[test]
    fn connect_whip_no_launch() {
        let (_port, mut host, host_out) = listen(&[]);
        let ticket = ticket(&host_out).to_string();

        // an even port, because rtp uses the even port of a pair
        let play = random_port() & !1;
        let play_addr = format!("127.0.0.1:{play}");
        let mut viewer = Command::new(streampipe_bin())
            .args(["connect-whip", "-v", "--addr", &play_addr, "--no-launch"])
            .arg(&ticket)
            .env_remove("RUST_LOG")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn connect-whip");
        let viewer_out = collect(&mut viewer);

        wait_for(&viewer_out, "writing the player description", READY);
        wait_for(&viewer_out, "connected to", READY);
        wait_for(&viewer_out, "waiting for media", READY);

        assert!(
            viewer.try_wait().ok().flatten().is_none(),
            "the viewer quit while waiting for media:\n{}",
            viewer_out.lock().unwrap()
        );

        viewer.kill().ok();
        viewer.wait().ok();
        host.kill().ok();
        host.wait().ok();
    }
}
