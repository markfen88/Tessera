//! Reading input reports from a device node, off the async runtime.
//!
//! A `hidraw` node returns one input report per `read()`. Each opened node
//! gets its own plain thread (not a tokio blocking task: a read must never
//! hold up the runtime's shutdown). The node is opened non-blocking and the
//! thread waits for it with `poll()` in slices of [`POLL_SLICE_MS`], so it
//! notices within one slice that it should stop (the [`Reports`] was
//! dropped) even on a device that never sends anything, and then closes the
//! node. It hands each report to the control plane through a bounded
//! channel and also ends when the device goes away (the read fails) or at
//! end of file.
//!
//! A rawmidi node (MIDI surfaces, [`open_duplex`]) is read the same way, as
//! a byte stream (a read returns whatever bytes the driver holds; messages
//! may be split across reads), and is also written to through an
//! [`Output`] (feedback to the desk). Output never blocks the caller: whole
//! messages are queued to a writer thread per port or connection, which
//! writes each one whole within [`WRITE_DEADLINE`]; a write that fails or
//! times out part way ends the session (its [`Reports`] end with the
//! reason), so a desk's parser is never left out of step and a failure is
//! seen, not only logged.
//!
//! A `tcp_midi` connection ([`connect_tcp`]) has TCP keep-alive and a user
//! timeout of a few seconds, so a peer that is gone (power, cable) ends the
//! connection even while nothing is sent; the session can add its own liveness
//! check on top (`Surfaces::set_liveness`, off by default).
//!
//! **Lab scripts.** With `--lab-surface-root` a node may be a regular file
//! of scripted reports (one report per line as hex, `# sleep <ms>` waits,
//! looped; see `tests/fixtures/surfaces/README.md`), at most
//! [`MAX_SCRIPT`] bytes. Never in production: there the node is read as it
//! is.
//!
//! Plain `std::fs` reads and the safe `poll()` wrapper of `nix`: no C
//! library, no `unsafe`, no ioctl (so no `HIDIOCGINPUT` prime: buttons start
//! released).

use std::fs::File;
use std::io::Read;
use std::net::TcpStream;
use std::os::fd::AsFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::poll::{PollFd, PollFlags, poll};
use tokio::sync::mpsc;

/// Largest report read (hidraw reports are at most 4 KiB; USB full speed
/// devices send 64 bytes).
const MAX_REPORT: usize = 4096;
/// Reports queued towards the control plane.
const QUEUE: usize = 256;
/// A script without any `# sleep` waits this long per loop (it must not spin).
const SCRIPT_MIN_LOOP: Duration = Duration::from_millis(100);
/// Longest wait for a report before the reading thread looks at its stop
/// flag again, milliseconds.
pub const POLL_SLICE_MS: u16 = 100;
/// Largest lab script read, bytes.
pub const MAX_SCRIPT: u64 = 1 << 20;

/// One step of a lab script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Deliver this report.
    Report(Vec<u8>),
    /// Wait this many milliseconds.
    Sleep(u64),
}

/// Parse a lab script (see the module documentation).
///
/// # Errors
/// The first line that is neither a comment nor hex bytes, by number.
pub fn parse_script(text: &str) -> Result<Vec<Step>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(c) = t.strip_prefix('#') {
            if let Some(ms) = c.trim().strip_prefix("sleep") {
                let ms = ms
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| format!("line {}: `# sleep` needs milliseconds", n + 1))?;
                out.push(Step::Sleep(ms));
            }
            continue;
        }
        let hex: String = t.chars().filter(|c| !c.is_whitespace()).collect();
        if !hex.len().is_multiple_of(2) {
            return Err(format!("line {}: odd number of hex digits", n + 1));
        }
        let bytes = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(hex.get(i..i + 2).unwrap_or("x"), 16))
            .collect::<Result<Vec<u8>, _>>()
            .map_err(|_| format!("line {}: not hex bytes", n + 1))?;
        out.push(Step::Report(bytes));
    }
    Ok(out)
}

/// Reports from one opened node. Dropping it stops the reading thread
/// (within [`POLL_SLICE_MS`]), which closes the node.
#[derive(Debug)]
pub struct Reports {
    rx: mpsc::Receiver<Result<Vec<u8>, String>>,
    stop: Arc<AtomicBool>,
    /// Set by the reading thread as it ends.
    #[cfg_attr(not(test), allow(dead_code))]
    ended: Arc<AtomicBool>,
}

impl Drop for Reports {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Reports {
    /// The next report; `Err` with the reason when the device went away
    /// (`"end of file"` for a node that ended).
    pub async fn next(&mut self) -> Result<Vec<u8>, String> {
        match self.rx.recv().await {
            Some(r) => r,
            None => Err("the reader stopped".into()),
        }
    }

    /// The flag the reading thread sets as it ends (tests).
    #[cfg(test)]
    pub(crate) fn ended(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.ended)
    }
}

/// Sets the flag when the reading thread ends, however it ends.
struct EndGuard(Arc<AtomicBool>);

impl Drop for EndGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Open `node` read-only and start reading it. `lab`: a regular file is a
/// script.
///
/// # Errors
/// Opening or reading the node failed (not found, permission denied, a
/// script that is too large or does not parse).
pub fn open(node: &Path, lab: bool) -> std::io::Result<Reports> {
    open_with(node, lab, false).map(|(r, _)| r)
}

/// Messages queued towards a device's writer thread.
const WRITE_QUEUE: usize = 512;
/// Longest a whole message may take to reach a device: a rawmidi port
/// that does not drain, or a `tcp_midi` peer that does not take the bytes,
/// within this time is lost.
pub const WRITE_DEADLINE: Duration = Duration::from_secs(2);

/// Where a duplex node's output goes.
#[derive(Debug, Clone)]
pub enum Output {
    /// A device node or a `tcp_midi` connection, written by its own writer
    /// thread (see [`Writer`]).
    Queue(Writer),
    /// A lab script: what would have been written is recorded here.
    Lab(Arc<std::sync::Mutex<Vec<u8>>>),
}

/// The queue to a device's writer thread. A message is queued whole and
/// written whole by the thread (never on an async worker); when a write
/// fails, or times out part way (which would leave the device's parser out
/// of step), the thread hands the failure to the session's [`Reports`] (the
/// session ends as lost) and stops writing.
#[derive(Debug, Clone)]
pub struct Writer {
    tx: std::sync::mpsc::SyncSender<Vec<u8>>,
    fail: Failure,
}

/// How a writer reports its failure (shared with its thread).
#[derive(Debug, Clone)]
struct Failure {
    /// The reports channel: a write failure ends the session.
    tx: Tx,
    /// Set once a write failed: nothing more is queued.
    failed: Arc<AtomicBool>,
}

impl Failure {
    /// From the session (an async worker): never waits; when the reports
    /// queue is full the session still sees the error of
    /// every later write.
    fn fail(&self, why: String) {
        if !self.failed.swap(true, Ordering::Relaxed) {
            let _ = self.tx.try_send(Err(why));
        }
    }

    /// From the writer thread: waits for room in the reports queue.
    fn fail_blocking(&self, why: String) {
        if !self.failed.swap(true, Ordering::Relaxed) {
            let _ = self.tx.blocking_send(Err(why));
        }
    }
}

impl Output {
    /// Queue `bytes` (one whole MIDI message) for the device. Never blocks.
    ///
    /// # Errors
    /// The output failed before, its queue is full (the device does not
    /// take what it is sent), or its writer stopped; the session's reports
    /// end with the reason too.
    pub fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            Output::Queue(w) => w.send(bytes),
            Output::Lab(v) => {
                v.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend_from_slice(bytes);
                Ok(())
            }
        }
    }
}

impl Writer {
    fn send(&self, bytes: &[u8]) -> std::io::Result<()> {
        if self.fail.failed.load(Ordering::Relaxed) {
            return Err(std::io::Error::other("the output failed"));
        }
        match self.tx.try_send(bytes.to_vec()) {
            Ok(()) => Ok(()),
            Err(e) => {
                let why = match e {
                    std::sync::mpsc::TrySendError::Full(_) => {
                        "the device does not take its output (queue full)"
                    }
                    std::sync::mpsc::TrySendError::Disconnected(_) => "the output stopped",
                };
                self.fail.fail(why.to_owned());
                Err(std::io::Error::other(why))
            }
        }
    }
}

/// An output that has failed already (tests).
#[cfg(test)]
pub(crate) fn failed_output() -> Output {
    let (tx, _) = std::sync::mpsc::sync_channel(1);
    let (ftx, _) = mpsc::channel(1);
    Output::Queue(Writer {
        tx,
        fail: Failure {
            tx: ftx,
            failed: Arc::new(AtomicBool::new(true)),
        },
    })
}

/// An output whose device takes nothing: `capacity` messages are queued,
/// then the queue is full (tests). Keep the receiver: dropped, the output
/// stops instead.
#[cfg(test)]
pub(crate) fn stalled_output(capacity: usize) -> (Output, std::sync::mpsc::Receiver<Vec<u8>>) {
    let (tx, rx) = std::sync::mpsc::sync_channel(capacity);
    let (ftx, _) = mpsc::channel(1);
    (
        Output::Queue(Writer {
            tx,
            fail: Failure {
                tx: ftx,
                failed: Arc::new(AtomicBool::new(false)),
            },
        }),
        rx,
    )
}

/// What a writer thread writes to.
enum Sink {
    Node(File),
    Tcp(TcpStream),
}

impl Sink {
    /// Write one whole message within [`WRITE_DEADLINE`].
    fn write_message(&mut self, mut bytes: &[u8]) -> Result<(), String> {
        use std::io::Write;
        let deadline = std::time::Instant::now() + WRITE_DEADLINE;
        while !bytes.is_empty() {
            let r = match self {
                Sink::Node(f) => f.write(bytes),
                Sink::Tcp(s) => s.write(bytes),
            };
            match r {
                Ok(0) => return Err("the device takes no more bytes".into()),
                Ok(n) => bytes = bytes.get(n..).unwrap_or_default(),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    let left = deadline.saturating_duration_since(std::time::Instant::now());
                    if left.is_zero() {
                        return Err(format!(
                            "a write did not complete within {} ms",
                            WRITE_DEADLINE.as_millis()
                        ));
                    }
                    if let Sink::Node(f) = self {
                        // Non-blocking node: wait for room (a TCP socket
                        // waits in the write itself, up to its timeout).
                        let ms = u16::try_from(left.as_millis()).unwrap_or(u16::MAX).max(1);
                        let mut fds = [PollFd::new(f.as_fd(), PollFlags::POLLOUT)];
                        match poll(&mut fds, ms) {
                            Ok(_) | Err(Errno::EINTR) => {}
                            Err(e) => return Err(format!("poll: {e}")),
                        }
                    }
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(())
    }

    fn close(&self) {
        if let Sink::Tcp(s) = self {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// Start the writer thread of `sink`; failures go to `fail` (the reports).
fn start_writer(sink: Sink, fail: &Tx, name: &str) -> std::io::Result<Output> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(WRITE_QUEUE);
    let w = Writer {
        tx,
        fail: Failure {
            tx: fail.clone(),
            failed: Arc::new(AtomicBool::new(false)),
        },
    };
    // The thread holds no sender: it ends when the session drops its
    // [`Output`].
    let t = w.fail.clone();
    std::thread::Builder::new()
        .name(format!("{name}-out"))
        .spawn(move || {
            let mut sink = sink;
            // Ends when every sender is gone (the session ended).
            while let Ok(msg) = rx.recv() {
                if let Err(e) = sink.write_message(&msg) {
                    sink.close();
                    t.fail_blocking(format!("writing to the device failed: {e}"));
                    return;
                }
            }
        })?;
    Ok(Output::Queue(w))
}

/// Open `node` read-write (a rawmidi port): it is read like [`open`] (as a
/// byte stream: each read hands over what the driver has), and the
/// [`Output`] writes to it. A lab script is read as with [`open`] and its
/// output recorded.
///
/// # Errors
/// As [`open`].
pub fn open_duplex(node: &Path, lab: bool) -> std::io::Result<(Reports, Output)> {
    open_with(node, lab, true).map(|(r, o)| (r, o.unwrap_or_else(|| Output::Lab(Arc::default()))))
}

fn open_with(node: &Path, lab: bool, write: bool) -> std::io::Result<(Reports, Option<Output>)> {
    // A device node that is written too is opened ONCE, read-write: ALSA
    // rawmidi refuses a second open of the same substream (EBUSY), so the
    // writer is a clone of the reader's descriptor, never a second open.
    let lab_file = lab && std::fs::metadata(node)?.is_file();
    let duplex = write && !lab_file;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(duplex)
        .custom_flags(OFlag::O_NONBLOCK.bits())
        .open(node)?;
    let script = if lab && file.metadata()?.is_file() {
        if file.metadata()?.len() > MAX_SCRIPT {
            return Err(std::io::Error::other(format!(
                "a lab script is at most {MAX_SCRIPT} bytes"
            )));
        }
        let mut text = String::new();
        (&file).take(MAX_SCRIPT).read_to_string(&mut text)?;
        Some(parse_script(&text).map_err(std::io::Error::other)?)
    } else {
        None
    };
    let (tx, rx) = mpsc::channel(QUEUE);
    let name = format!(
        "surface-{}",
        node.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    );
    let output = if !write {
        None
    } else if script.is_some() {
        Some(Output::Lab(Arc::default()))
    } else {
        Some(start_writer(Sink::Node(file.try_clone()?), &tx, &name)?)
    };
    let stop = Arc::new(AtomicBool::new(false));
    let ended = Arc::new(AtomicBool::new(false));
    let stop_t = Arc::clone(&stop);
    let guard = EndGuard(Arc::clone(&ended));
    std::thread::Builder::new().name(name).spawn(move || {
        let _guard = guard;
        match script {
            Some(steps) => play(&steps, &tx, &stop_t),
            None => read_node(file, &tx, &stop_t),
        }
    })?;
    Ok((Reports { rx, stop, ended }, output))
}

/// Longest wait for a `tcp_midi` connection to be accepted.
pub const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Longest wait of one write call to a `tcp_midi` connection (on the
/// writer thread; a whole message has [`WRITE_DEADLINE`]).
pub const TCP_WRITE_TIMEOUT: Duration = Duration::from_millis(500);
/// TCP keep-alive on a `tcp_midi` connection: probes after this much
/// silence, seconds ...
pub const TCP_KEEPALIVE_IDLE_S: u32 = 3;
/// ... one every this many seconds ...
pub const TCP_KEEPALIVE_INTERVAL_S: u32 = 1;
/// ... and the connection is dropped after this many unanswered (a peer
/// that is gone is noticed within about 6 s even when nothing is sent).
pub const TCP_KEEPALIVE_COUNT: u32 = 3;
/// Longest sent data may stay unacknowledged before the kernel drops the
/// connection, milliseconds (`TCP_USER_TIMEOUT`).
pub const TCP_USER_TIMEOUT_MS: u32 = 6_000;

/// Keep-alive and user timeout on a `tcp_midi` socket (see the constants).
fn keepalive(s: &TcpStream) -> std::io::Result<()> {
    use nix::sys::socket::{setsockopt, sockopt};
    let io = |e: Errno| std::io::Error::from_raw_os_error(e as i32);
    setsockopt(s, sockopt::KeepAlive, &true).map_err(io)?;
    setsockopt(s, sockopt::TcpKeepIdle, &TCP_KEEPALIVE_IDLE_S).map_err(io)?;
    setsockopt(s, sockopt::TcpKeepInterval, &TCP_KEEPALIVE_INTERVAL_S).map_err(io)?;
    setsockopt(s, sockopt::TcpKeepCount, &TCP_KEEPALIVE_COUNT).map_err(io)?;
    setsockopt(s, sockopt::TcpUserTimeout, &TCP_USER_TIMEOUT_MS).map_err(io)?;
    Ok(())
}

/// Connect out to `host:port` and read the connection like a rawmidi node
/// (a byte stream, on a plain thread); the [`Output`] writes to it. Tessera
/// only connects out (ADR 0017). Blocking (name lookup, connect): call off
/// the async workers.
///
/// # Errors
/// The name does not resolve, or no address accepts the connection.
pub fn connect_tcp(host: &str, port: u16) -> std::io::Result<(Reports, Output)> {
    use std::net::ToSocketAddrs;
    let mut last = std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("`{host}` has no address"),
    );
    let mut stream = None;
    for addr in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, TCP_CONNECT_TIMEOUT) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last = e,
        }
    }
    let stream = stream.ok_or(last)?;
    let _ = stream.set_nodelay(true);
    keepalive(&stream)?;
    stream.set_write_timeout(Some(TCP_WRITE_TIMEOUT))?;
    stream.set_read_timeout(Some(Duration::from_millis(u64::from(POLL_SLICE_MS))))?;
    let (tx, rx) = mpsc::channel(QUEUE);
    let output = start_writer(Sink::Tcp(stream.try_clone()?), &tx, "surface-tcp")?;
    let stop = Arc::new(AtomicBool::new(false));
    let ended = Arc::new(AtomicBool::new(false));
    let stop_t = Arc::clone(&stop);
    let guard = EndGuard(Arc::clone(&ended));
    std::thread::Builder::new()
        .name("surface-tcp".into())
        .spawn(move || {
            let _guard = guard;
            read_tcp(stream, &tx, &stop_t);
        })?;
    Ok((Reports { rx, stop, ended }, output))
}

fn read_tcp(mut s: TcpStream, tx: &Tx, stop: &AtomicBool) {
    let mut buf = vec![0u8; MAX_REPORT];
    loop {
        if stop.load(Ordering::Relaxed) || tx.is_closed() {
            let _ = s.shutdown(std::net::Shutdown::Both);
            return;
        }
        let r = match s.read(&mut buf) {
            Ok(0) => Err("the connection was closed".to_owned()),
            Ok(n) => Ok(buf.get(..n).unwrap_or_default().to_vec()),
            // The read timeout (`SO_RCVTIMEO`) is `WouldBlock`; `TimedOut`
            // (ETIMEDOUT) is the keep-alive or user timeout giving up: lost.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(e) => Err(e.to_string()),
        };
        let last = r.is_err();
        if stop.load(Ordering::Relaxed) || tx.blocking_send(r).is_err() || last {
            let _ = s.shutdown(std::net::Shutdown::Both);
            return;
        }
    }
}

type Tx = mpsc::Sender<Result<Vec<u8>, String>>;

/// Wait up to `ms` for `file` to become readable. A hang-up or error counts
/// as readable: the read that follows reports it.
fn readable(file: &File, ms: u16) -> Result<bool, Errno> {
    let mut fds = [PollFd::new(file.as_fd(), PollFlags::POLLIN)];
    Ok(poll(&mut fds, ms)? > 0)
}

fn read_node(mut file: File, tx: &Tx, stop: &AtomicBool) {
    let mut buf = vec![0u8; MAX_REPORT];
    loop {
        if stop.load(Ordering::Relaxed) || tx.is_closed() {
            return;
        }
        match readable(&file, POLL_SLICE_MS) {
            Ok(true) => {}
            Ok(false) | Err(Errno::EINTR) => continue,
            Err(e) => {
                let _ = tx.blocking_send(Err(format!("poll: {e}")));
                return;
            }
        }
        let r = match file.read(&mut buf) {
            Ok(0) => Err("end of file".to_owned()),
            Ok(n) => Ok(buf.get(..n).unwrap_or_default().to_vec()),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(e) => Err(e.to_string()),
        };
        let last = r.is_err();
        if stop.load(Ordering::Relaxed) || tx.blocking_send(r).is_err() || last {
            return;
        }
    }
}

/// Sleep `d` in short slices, returning early (`false`) once `stop` is set.
fn nap(d: Duration, stop: &AtomicBool) -> bool {
    let slice = Duration::from_millis(50);
    let mut left = d;
    while !left.is_zero() {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let s = left.min(slice);
        std::thread::sleep(s);
        left -= s;
    }
    !stop.load(Ordering::Relaxed)
}

fn play(steps: &[Step], tx: &Tx, stop: &AtomicBool) {
    let sleeps = steps.iter().any(|s| matches!(s, Step::Sleep(_)));
    loop {
        for s in steps {
            match s {
                Step::Report(r) => {
                    if stop.load(Ordering::Relaxed) || tx.blocking_send(Ok(r.clone())).is_err() {
                        return;
                    }
                }
                Step::Sleep(ms) => {
                    if !nap(Duration::from_millis(*ms), stop) {
                        return;
                    }
                }
            }
        }
        if !sleeps && !nap(SCRIPT_MIN_LOOP, stop) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_parse_reports_sleeps_and_comments() {
        let s = parse_script("# a comment\n\n01 00 ff\n# sleep 20\n0a0b\n#sleep 5\n").unwrap();
        assert_eq!(
            s,
            vec![
                Step::Report(vec![1, 0, 0xff]),
                Step::Sleep(20),
                Step::Report(vec![10, 11]),
                Step::Sleep(5),
            ]
        );
        assert!(parse_script("01 0").unwrap_err().contains("line 1"));
        assert!(parse_script("\nzz").unwrap_err().contains("line 2"));
        assert!(parse_script("# sleep x").is_err());
    }

    #[test]
    fn the_shipped_fixture_script_parses() {
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/surfaces/sc3/dev/hidraw3");
        let s = parse_script(&std::fs::read_to_string(p).unwrap()).unwrap();
        assert!(
            s.iter()
                .filter(|s| matches!(s, Step::Report(r) if r.len() == 8))
                .count()
                > 100
        );
    }

    #[tokio::test]
    async fn a_script_loops_and_a_plain_file_ends() {
        let dir = std::env::temp_dir().join(format!("tessera-reader-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("node");
        std::fs::write(&p, "01 02\n# sleep 1\n03 04\n").unwrap();
        let mut r = open(&p, true).unwrap();
        let got: Vec<Vec<u8>> = vec![
            r.next().await.unwrap(),
            r.next().await.unwrap(),
            r.next().await.unwrap(),
        ];
        assert_eq!(got, vec![vec![1, 2], vec![3, 4], vec![1, 2]], "looped");
        drop(r);
        // Not lab: the bytes as they are, then the end.
        let mut r = open(&p, false).unwrap();
        assert_eq!(r.next().await.unwrap().len(), 22, "the whole file");
        assert_eq!(r.next().await.unwrap_err(), "end of file");
        assert!(open(&dir.join("missing"), true).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_quiet_node_still_stops_and_closes() {
        use std::os::fd::AsRawFd;
        // A pipe nobody writes to: a read would block for ever.
        let (quiet, _writer) = std::io::pipe().unwrap();
        let path = format!("/proc/self/fd/{}", quiet.as_raw_fd());
        let r = open(Path::new(&path), false).unwrap();
        let ended = r.ended();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!ended.load(Ordering::Relaxed), "still waiting for a report");
        drop(r);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !ended.load(Ordering::Relaxed) {
            assert!(
                std::time::Instant::now() < deadline,
                "the reading thread did not stop"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A writer on a pipe whose reading end is closed (every write fails),
    /// and the reports channel its failure goes to.
    fn writer_on_a_closed_pipe() -> (Output, mpsc::Receiver<Result<Vec<u8>, String>>) {
        let (r, w) = std::io::pipe().unwrap();
        drop(r);
        let file = File::from(std::os::fd::OwnedFd::from(w));
        let (tx, rx) = mpsc::channel(QUEUE);
        (start_writer(Sink::Node(file), &tx, "test").unwrap(), rx)
    }

    #[tokio::test]
    async fn a_failed_write_ends_the_reports_and_refuses_more() {
        let (out, mut rx) = writer_on_a_closed_pipe();
        // Queued at once (the thread writes it).
        out.write(&[0xb0, 0x63, 0x40]).unwrap();
        let why = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(why.contains("writing to the device failed"), "{why}");
        // Nothing more is taken.
        assert!(out.write(&[0xb0]).is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn writes_to_a_peer_that_does_not_read_never_block_and_end_the_session() {
        // A desk that accepts and then never reads: its buffers fill up.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (done, wait) = std::sync::mpsc::channel::<()>();
        let peer = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let _ = wait.recv_timeout(Duration::from_secs(10));
            drop(s);
        });
        let (mut reports, out) =
            tokio::task::spawn_blocking(move || connect_tcp("127.0.0.1", port))
                .await
                .unwrap()
                .unwrap();
        // Far more than the socket buffers hold, queued from the async
        // worker: it must not wait for the peer.
        let big = vec![0xf8u8; 64 * 1024];
        let start = std::time::Instant::now();
        for _ in 0..400 {
            if out.write(&big).is_err() {
                break;
            }
        }
        assert!(
            // Generous: a loaded test run; blocking would take the writer's
            // 2 s deadline per message, far longer.
            start.elapsed() < Duration::from_millis(2_000),
            "queueing blocked for {:?}",
            start.elapsed()
        );
        // The writer gives up within its deadline: the reports end. Under
        // load the read side (TCP user timeout) can report the dead peer
        // first; either way the session ends with a reason.
        let why = tokio::time::timeout(Duration::from_secs(9), reports.next())
            .await
            .expect("the session was not ended")
            .unwrap_err();
        assert!(!why.is_empty(), "the session ends with a reason");
        drop((reports, out));
        let _ = done.send(());
        peer.join().unwrap();
    }

    #[test]
    fn an_oversized_lab_script_is_refused() {
        let dir = std::env::temp_dir().join(format!("tessera-reader-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("node");
        let line = "01 02 03 04 05 06 07 08\n";
        let n = usize::try_from(MAX_SCRIPT).unwrap() / line.len() + 1;
        std::fs::write(&p, line.repeat(n)).unwrap();
        let e = open(&p, true).unwrap_err();
        assert!(e.to_string().contains("at most"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
