//! Shared-memory audio rings and clock overlay shared between tesserad and
//! audio clients (ADR 0007).
//!
//! This crate is a **cross-process ABI**. Everything below is the contract; a
//! client written in C or any other language can implement it from this text.
//!
//! # Objects
//!
//! A client connects to `audio.sock` (a Unix stream socket, ADR 0010),
//! exchanges framed messages (see *Handshake*), and receives three file
//! descriptors by `SCM_RIGHTS`, in this order:
//!
//! | # | object | access | purpose |
//! |---|--------|--------|---------|
//! | 0 | ring memfd | read/write mapping | the endpoint's audio ring |
//! | 1 | clock-overlay memfd | **read-only** mapping | media clock map (ADR 0005) |
//! | 2 | wake socket (client end of a per-session `AF_UNIX`/`SOCK_STREAM` socketpair) | read/poll | one byte per daemon engine tick |
//!
//! **Departure from ADR 0007:** the wake object is a socketpair, not an
//! eventfd. A shared eventfd lets a client fill its counter and clear
//! `O_NONBLOCK` on the shared open file description, blocking the daemon's
//! media-engine thread forever. With a socketpair the daemon keeps its own end
//! (a different open file description), signals with
//! `MSG_DONTWAIT | MSG_NOSIGNAL`, and treats a full buffer or a dead client as
//! success; see [`wake_pair`].
//!
//! **Invariant: one ring and one wake socketpair per session.** The only object
//! shared between sessions is the clock overlay, which is read-only for
//! clients.
//!
//! Both memfds are **sealed**: a receiver must verify (`fcntl(F_GET_SEALS)`)
//! that `F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL` are all set before
//! mapping, otherwise the peer could `ftruncate` the file and make any access
//! raise `SIGBUS`. The overlay additionally carries `F_SEAL_FUTURE_WRITE`
//! (applied by the daemon after it made its own writable mapping), so a
//! writable shared mapping of it cannot be created by anyone; clients map it
//! `PROT_READ`. The `fstat` size of each memfd must equal the `total_len` in
//! its header, and must be checked against a local maximum *before* mapping.
//! Check the seals first and read the size (`fstat`) afterwards; before the
//! seals are verified the size proves nothing.
//! Receive descriptors with `MSG_CMSG_CLOEXEC`; send with `MSG_NOSIGNAL`.
//!
//! # Memory model
//!
//! * Byte order is the host's native order. The memfds only ever exist on one
//!   machine; there is no cross-endian support.
//! * **Every field that can be touched by the peer while a mapping is live is
//!   accessed only with atomic operations of its natural width** (8-byte
//!   atomics for 64-bit fields and bitmap words, 4-byte atomics for samples).
//!   The *static* header (below) is written once before the descriptor is
//!   shared and is read-only afterwards.
//! * Orderings are given in C11 terms (`store_release`, `load_acquire`,
//!   `atomic_thread_fence`, `relaxed`). 64-bit atomics must be lock-free.
//!   The overlay is mapped read-only, so its reader uses only *relaxed* loads
//!   (plus fences). On 64-bit targets (x86-64, AArch64) those are 64-bit loads.
//!   On targets with a pointer width below 64 bits (e.g. ARMv7) Rust does not
//!   guarantee 64-bit loads from read-only memory, so the reader reads each
//!   64-bit overlay field as two relaxed 32-bit loads and takes the seqlock
//!   sequence from its low 32 bits only; torn halves are rejected by the
//!   sequence re-check (see the overlay section). The memory layout and the
//!   writer are identical on every target. The crate still requires native
//!   64-bit atomics (`AtomicU64`) for ring positions and counters, which
//!   `armv7-unknown-linux-gnueabihf` has (`ldrexd`/`strexd`); targets without
//!   them fail to compile with a clear message.
//! * Receivers copy the static header once into local memory, validate the
//!   copy with checked arithmetic against the real mapping size, and never
//!   re-read geometry from shared memory. Positions found in shared memory are
//!   untrusted: they are only ever used modulo the capacity or in saturating
//!   comparisons, never as lengths or offsets.
//!
//! # Ring layout (version 1.0)
//!
//! Offsets are bytes from the start of the mapping. `cap` is the capacity in
//! frames per channel (a power of two, >= 64).
//!
//! ```text
//! 0x000  [u8;8]  magic = "TSRARING"
//! 0x008  u32     version_major = 1      (reject any other value)
//! 0x00C  u32     version_minor = 0      (additive changes only)
//! 0x010  u32     header_len = 512
//! 0x014  u32     flags = 0
//! 0x018  u64     total_len              (must equal the memfd size)
//! 0x020  u32     direction              1 = rx (daemon produces), 2 = tx (client produces)
//! 0x024  u32     sample_format          1 = i32, 2 = f32   (both 4 bytes)
//! 0x028  u32     channels               >= 1
//! 0x02C  u32     sample_rate_hz         >= 1 (informational for the ring)
//! 0x030  u64     capacity_frames        cap (<= 2^34, because plane_offset is a u32)
//! 0x038  u32     first_channel          endpoint channel number of plane 0 (informational)
//! 0x03C  u32     reserved = 0
//! 0x040  u64     channel_stride         see the formulas below
//! 0x048  u64     channels_offset        = 512
//! 0x050  u32     bitmap_offset          = 64   (inside a channel block)
//! 0x054  u32     plane_offset           = 64 + bitmap_bytes (inside a channel block)
//! 0x058  ...     reserved, zero until 0x080
//! --- producer cache line ---
//! 0x080  u64     epoch_seq              atomic; written only by the producer; even = stable, odd = reset in progress;
//!                                       logical epoch = (epoch_seq + 1) / 2
//! 0x088  u64     write_pos              atomic hint: highest end position of a completed write
//! 0x090  u64     late_frames            atomic counter (frames dropped as too old)
//! --- consumer cache line ---
//! 0x0C0  u64     read_pos               atomic; consumer's published position (metrics only)
//! 0x0C8  u64     unwritten_frames       atomic counter (see read semantics)
//! 0x0D0  u64     hole_frames            atomic counter
//! 0x0D8  u64     overrun_frames         atomic counter
//! --- owner cache line ---
//! 0x100  u64     effective_latency_frames   atomic; written by the creating (daemon) side
//! 0x108  ...     reserved up to 0x200
//! --- channel blocks, `channels` of them, `channel_stride` bytes apart, from 0x200 ---
//! +0x00  u64     hi                     atomic; high-water mark of this channel (producer only)
//! +0x08  ...     reserved up to +0x40
//! +bitmap_offset  [u64; cap/64]         validity bitmap: bit (p mod cap) says position p holds data
//! +plane_offset   [u32; cap]            samples: slot (p mod cap) holds the sample of position p
//! ```
//!
//! `bitmap_bytes = cap / 8` rounded up to a multiple of 64, `plane_offset =
//! 64 + bitmap_bytes`, `channel_stride = plane_offset + 4 * cap` (always a
//! multiple of 64), `total_len = 512 + channels * channel_stride`. A receiver
//! recomputes all of these from `channels` and `cap` and rejects the mapping if
//! any stored value differs, if reserved bytes are non-zero (for a minor
//! version it knows), if `total_len` differs from the file size, or if the
//! parameters exceed its local [`Limits`].
//!
//! Samples are non-interleaved planes. `i32` samples are left-aligned signed
//! 32-bit; `f32` samples are IEEE binary32; the all-zero bit pattern is
//! silence in both.
//!
//! # Ring semantics (media-time addressing, ADR 0006)
//!
//! A *position* is a `u64` media-sample index. Position `p` of channel `c`
//! lives in slot `p mod cap`. There is exactly one producer and one consumer
//! per ring. The producer never waits for the consumer (the consumer position
//! is metrics only), so a stalled or dead consumer cannot stall the producer.
//!
//! For each channel, `hi` is the exclusive upper end of everything the
//! producer has ever touched since the last reset. The *window* is
//! `[max(hi - cap, 0), hi)`; only positions in the window can hold data.
//! Position `p` is **valid** iff it is in the window and its bitmap bit is
//! set. Anything else reads as silence. Validity is therefore tracked per
//! sample (block size 1), which allows arbitrary packet boundaries,
//! out-of-order arrival and holes.
//!
//! **Producer, `write(c, pos, samples[n])`** (all values `u64`, saturating
//! where written):
//!
//! ```text
//! end    = pos + n                              (reject on overflow)
//! new_hi = max(hi_local, end)
//! floor  = new_hi - cap  (or 0)
//! if new_hi > hi_local:
//!     clear bitmap bits of positions [max(hi_local, floor), new_hi)   // relaxed stores; whole bitmap if >= cap
//!     store_release(channel.hi, new_hi); hi_local = new_hi
//!     atomic_thread_fence(release)
//! for each position p in [max(pos, floor), end):   // positions < floor are "late": dropped, counted
//!     store_relaxed(plane[p mod cap], sample)
//!     after the last sample in each aligned group of 64 slots:
//!         store_release(word, load_relaxed(word) | bits)
//! store_release(write_pos, max(write_pos, end))
//! ```
//!
//! The producer keeps `hi` (and its counters and epoch) in its own memory;
//! the shared copies are write-only outputs, so a peer scribbling on them
//! cannot steer the producer.
//! At takeover no producer-side local state is trusted from shared memory:
//! the producer starts from zeroed local positions, counters and `late`
//! values and performs a `reset` (fresh epoch). The only value read back is
//! the epoch word, used as a lower bound (`max(local, shared)`, wrapping
//! arithmetic) so that a second `Ring` object over the same memfd never
//! republishes a word a consumer may already have seen. A peer that scribbled
//! on the header beforehand (an RX client maps the ring read/write) therefore
//! cannot silence the session or make the producer overflow. The consumer
//! likewise starts its counters from zero; at takeover it reads only the
//! epoch word (to report `epoch_changed`), and every later read treats `hi`,
//! the bitmap, the samples and the epoch word as untrusted input (saturating
//! arithmetic, all indices masked, nothing used as a length).
//!
//! **Consumer, `read(c, pos, out[n])`** (wait-free: no retries, no waiting):
//!
//! ```text
//! s0 = load_acquire(epoch_seq);  if s0 is odd: the whole output is silence, stop
//! for each group of consecutive positions that share one bitmap word:
//!     h1   = load_acquire(channel.hi)
//!     word = load_acquire(bitmap word)
//!     for p in the group with  max(h1 - cap, 0) <= p < h1  and bit set:
//!         out = load_relaxed(plane[p mod cap])
//!     (everything else: out = 0)
//!     atomic_thread_fence(acquire)
//!     h2 = load_relaxed(channel.hi)
//!     every position p < h2 - cap is set to silence     // producer lapped us while we copied
//! atomic_thread_fence(acquire)
//! if load_relaxed(epoch_seq) != s0: the whole output is silence
//! ```
//!
//! Why this is correct: the producer clears stale bits and publishes the new
//! `hi` (release) *before* it overwrites any slot of the previous lap, and a
//! release fence separates the `hi` store from the sample stores. A reader
//! that observes an overwritten sample therefore, after its acquire fence,
//! observes the advanced `hi` and discards the sample; a reader that observes
//! a bit set for a previous lap cannot have loaded the advanced `hi`.
//!
//! Each frame of a read is classified as exactly one of: *valid*; *overrun*
//! (older than the window: overwritten); *unwritten* (at or beyond `hi`);
//! *hole* (inside the window, never written). The consumer adds the last
//! three to the shared counters (`overrun_frames`, `unwritten_frames`,
//! `hole_frames`; per-channel-frame units). Stale audio is never replayed.
//!
//! **Epoch / reset.** On flow loss or a clock epoch change (ADR 0005) the
//! producer calls `reset`. `epoch_seq` is a seqlock-style word so that a reset
//! is atomic for consumers, including the lap check above (which relies on
//! `hi` only ever growing):
//!
//! ```text
//! s = epoch_seq | 1;  store_relaxed(epoch_seq, s);  atomic_thread_fence(release)   // odd: resetting
//! for every channel: clear all bitmap words, hi = 0           // relaxed stores
//! write_pos = 0                                               // relaxed store
//! store_release(epoch_seq, s + 1)                             // even: new epoch
//! ```
//!
//! A consumer whose read overlaps any part of a reset sees `epoch_seq` differ
//! at the end (any observed effect of the clearing is ordered after the odd
//! store by the release/acquire fences) and returns silence for the whole
//! read; a read that starts while the word is odd returns silence at once. A
//! producer that dies mid-reset leaves the word odd: consumers then read
//! silence forever (never stale audio, never a hang) until a new producer
//! attaches and completes the reset. Consumers report `epoch_changed` when the
//! logical epoch differs from the previous read. The ring epoch is owned by the
//! producer; a TX client that sees the clock-overlay epoch change should
//! `reset` and re-anchor its positions.
//!
//! The daemon publishes the effective latency (frames) in the owner line. In
//! an RX ring the client reads at `now_media - effective_latency - client_offset`
//! (ADR 0007); in a TX ring the client writes ahead of `now_media`.
//!
//! # Clock overlay layout (version 1.0)
//!
//! ```text
//! 0x00  [u8;8]  magic = "TSRACLOK"
//! 0x08  u32     version_major = 1
//! 0x0C  u32     version_minor = 0
//! 0x10  u64     total_len              (must equal the memfd size; sizes 128 ..= 1 MiB are accepted so later minors may append fields)
//! 0x18  ...     reserved up to 0x40
//! --- seqlock cache line (all u64, atomic) ---
//! 0x40  seq              even = stable, odd = write in progress
//! 0x48  epoch            incremented on any discontinuity (step, GM change, holdover entry)
//! 0x50  base_mono_ns     CLOCK_MONOTONIC ns of the anchor
//! 0x58  base_media_ns    media-clock ns at the anchor
//! 0x60  rate_ppb         i64 stored as u64; media rate offset, parts per billion
//! 0x68  state            0 free_running, 1 listening, 2 acquiring, 3 locked, 4 holdover, 5 leader
//! 0x70  gm_identity      grandmaster identity, big-endian number (shorter ids zero-padded)
//! 0x78  updated_mono_ns  CLOCK_MONOTONIC ns of the last publish (liveness)
//! ```
//!
//! `media_ns(t) = base_media_ns + (t - base_mono_ns) * (1 + rate_ppb * 1e-9)`
//! evaluated in 128-bit integer arithmetic, truncating the rate term toward
//! zero and saturating to `[0, 2^64 - 1]`. Readers need only
//! `clock_gettime(CLOCK_MONOTONIC)`.
//!
//! **Writer** (single): `store_relaxed(seq, s | 1)`; `atomic_thread_fence(release)`;
//! relaxed stores of the fields; `store_release(seq, (s | 1) + 1)`.
//!
//! **Reader**: `s1 = load_relaxed(seq)`; if odd, retry; `atomic_thread_fence(acquire)`;
//! relaxed loads of the fields; `atomic_thread_fence(acquire)`;
//! `s2 = load_relaxed(seq)`; accept iff `s1 == s2`. Use only relaxed loads on the
//! read-only mapping. The retry count **must be bounded**: a writer that dies
//! mid-publish leaves `seq` odd forever. This crate retries up to
//! [`DEFAULT_RETRIES`] times and then returns [`OverlayError::Unstable`]; the
//! caller keeps using its last good snapshot (the media clock is an affine
//! map and stays usable for extrapolation).
//!
//! **32-bit reader variant** (pointer width < 64, or the
//! `force-split-overlay-reads` cargo feature): identical protocol and layout,
//! but every 64-bit field is fetched as two relaxed 32-bit loads (low and high
//! half, at the byte offsets of the least/most significant halves for the
//! host endianness) and `seq` is compared by its **low 32 bits only**. Torn
//! field halves can only arise around a concurrent publish, which changes
//! `seq`'s low word, so the re-check rejects them. The 32-bit sequence can
//! alias only if exactly a multiple of 2^32 phases (2^31 publishes) elapse
//! inside one read, i.e. a reader stalled for weeks at 1 kHz publishing; that
//! is not a practical ABA. The writer is unchanged (64-bit stores).
//!
//! # Handshake
//!
//! Frames on `audio.sock`: `u32 payload_len` (little-endian, counts the bytes
//! after itself, `4 <= payload_len <= max_frame_len`, default cap 64 KiB)
//! followed by the payload `u16 version = 1 | u16 kind | body`, all integers
//! little-endian. A receiver must read exactly one frame at a time (no
//! read-ahead), so descriptors attach to the right frame. Unknown versions or
//! kinds, short or over-long bodies, non-zero reserved/flags fields, invalid
//! UTF-8 or control characters in strings, and a descriptor count other than
//! the one the kind requires are all protocol errors; the connection should
//! then be closed.
//!
//! ```text
//! kind 1 Hello  (client -> daemon, 0 fds)
//!   u8 direction (1 rx, 2 tx) | u8 sample_format (1 i32, 2 f32) | u16 0
//!   u32 first_channel | u32 channel_count (>= 1) | u32 sample_rate_hz (0 = default)
//!   u32 max_period_frames (0 = default) | u32 flags = 0
//!   u8 n, n bytes endpoint name | u8 n, n bytes client name        (strings <= 255 bytes)
//! kind 2 Attach (daemon -> client, exactly 3 fds: ring, overlay, wake socket client end)
//!   u64 session_id
//!   u8 direction | u8 sample_format | u16 0
//!   u32 channels | u32 first_channel | u32 sample_rate_hz | u64 capacity_frames
//!   u64 effective_latency_frames | u32 flags = 0
//! kind 3 Reject (daemon -> client, 0 fds)
//!   u32 code (1 unsupported_version, 2 unknown_endpoint, 3 bad_request, 4 busy,
//!             5 permission_denied, 6 internal; others tolerated) | u8 n, n bytes message
//! kind 4 Detach (either direction, 0 fds)
//!   u32 reason (1 client_closing, 2 daemon_shutdown, 3 endpoint_removed,
//!               4 protocol_error; others tolerated)
//! ```
//!
//! After `Attach` the client validates the three descriptors as described
//! under *Objects* and additionally checks that the ring header's parameters
//! equal the `Attach` fields. Either side ends the session with `Detach` or by
//! closing the socket; neither side's death affects the other's mappings.
//!
//! # Real-time rules
//!
//! [`RingProducer::write`], [`RingProducer::reset`], [`RingConsumer::read`],
//! [`RingConsumer::publish_read_pos`], [`ClockOverlayWriter::publish`],
//! [`ClockOverlayReader::try_read`]/[`read`](ClockOverlayReader::read),
//! [`ClockSnapshot::media_ns_at`] and the stat accessors do not allocate, take
//! locks, make syscalls or log; their errors are `Copy` enums. The only syscall
//! in a steady-state audio loop is the wake socket (`WakeSignal::signal`/`WakeWait::drain`) and
//! `clock_gettime` (vDSO). The media-thread signal path never blocks
//! (`MSG_DONTWAIT`); control-plane I/O (`send_message`, `send_attach`,
//! `MessageReceiver`) is time-bounded by its timeout or the socket's own
//! timeouts, and may wait up to that long. Everything that creates, maps,
//! validates or passes descriptors is setup-path code and may allocate.
//!
//! Daemons should raise their soft `RLIMIT_NOFILE` to the hard limit at
//! start-up. Near the limit the kernel may install only some of the descriptors
//! sent with a frame and report `MSG_CTRUNC`; this crate then closes every
//! descriptor that was installed and returns [`Error::ControlTruncated`], but
//! it does not change process limits itself.
//!
//! # `unsafe`
//!
//! The crate relies on the workspace `unsafe_code = "deny"` and opts in only in
//! `map.rs` (the `mmap` wrapper and the atomic slice views, all bounds-checked)
//! and for descriptor adoption after `recvmsg`. Everything else is safe code.
#![warn(missing_docs)]

#[cfg(not(target_has_atomic = "64"))]
compile_error!(
    "tessera-audio-ipc needs native 64-bit atomics (AtomicU64): ring positions and counters are 64-bit \
     words shared between processes"
);

mod endpoint;
mod error;
mod layout;
mod map;
mod memfd;
mod overlay;
pub mod proto;
mod ring;
mod transport;
mod wake;

pub use endpoint::*;
pub use error::*;
pub use layout::*;
pub use overlay::*;
pub use ring::*;
pub use transport::*;
pub use wake::*;
