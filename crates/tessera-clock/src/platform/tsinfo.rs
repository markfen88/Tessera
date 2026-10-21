//! Runtime detection of NIC timestamping capabilities and PHC access.
//!
//! UNTESTED against real hardware: the build VM has no PHC. The kernel
//! interfaces used are `ETHTOOL_GET_TS_INFO` (via `SIOCETHTOOL`),
//! `SIOCSHWTSTAMP` and `PTP_SYS_OFFSET_EXTENDED` on `/dev/ptpN`.

use std::format;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, RawFd};

const SIOCETHTOOL: u64 = 0x8946;
const SIOCSHWTSTAMP: u64 = 0x89b0;
const SIOCGHWTSTAMP: u64 = 0x89b1;
const ETHTOOL_GET_TS_INFO: u32 = 0x41;
/// `_IOWR('=', 9, struct ptp_sys_offset_extended)` (1216 bytes).
const PTP_SYS_OFFSET_EXTENDED: u64 = 0xC4C0_3D09;
const PTP_MAX_SAMPLES: usize = 25;

const SOF_TX_HARDWARE: u32 = 1 << 0;
const SOF_RX_HARDWARE: u32 = 1 << 2;
const SOF_RAW_HARDWARE: u32 = 1 << 6;
const HWTSTAMP_TX_OFF: i32 = 0;
const HWTSTAMP_TX_ON: i32 = 1;
const HWTSTAMP_FILTER_NONE: i32 = 0;
const HWTSTAMP_FILTER_ALL: i32 = 1;
const HWTSTAMP_FILTER_PTP_V1_L4_EVENT: i32 = 3;

/// `struct ifreq` with the data-pointer view of the union, padded to cover
/// the largest union member on 32- and 64-bit targets.
#[repr(C)]
struct IfReq {
    name: [u8; 16],
    data: *mut libc::c_void,
    pad: [u8; 16],
}

impl IfReq {
    fn new(iface: &str, data: *mut libc::c_void) -> io::Result<Self> {
        let b = iface.as_bytes();
        if b.is_empty() || b.len() >= 16 || b.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "interface name must be 1..=15 bytes without NUL",
            ));
        }
        let mut name = [0u8; 16];
        name[..b.len()].copy_from_slice(b);
        Ok(IfReq {
            name,
            data,
            pad: [0; 16],
        })
    }
}

#[repr(C)]
#[derive(Default)]
struct EthtoolTsInfo {
    cmd: u32,
    so_timestamping: u32,
    phc_index: i32,
    tx_types: u32,
    tx_reserved: [u32; 3],
    rx_filters: u32,
    rx_reserved: [u32; 3],
}

#[repr(C)]
struct HwTstampConfig {
    flags: i32,
    tx_type: i32,
    rx_filter: i32,
}

/// What an interface can timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TsCaps {
    /// Hardware RX and TX timestamps in the PHC's time base are available.
    pub hardware: bool,
    /// Index of the PHC (`/dev/ptpN`), if the NIC has one.
    pub phc_index: Option<u32>,
    /// The NIC can timestamp PTPv1 UDP event messages (filter
    /// `PTP_V1_L4_EVENT` or `ALL`).
    pub hw_filter_ptp_v1: bool,
    /// The NIC can timestamp every received packet (filter `ALL`).
    pub hw_filter_all: bool,
}

impl TsCaps {
    /// Hardware timestamping is usable for PTPv1 on this interface.
    #[must_use]
    pub fn hardware_usable(&self) -> bool {
        self.hardware && self.phc_index.is_some() && self.hw_filter_ptp_v1
    }
}

/// Queries `iface` with `ETHTOOL_GET_TS_INFO`. Software timestamping is
/// always available (kernel feature), so only the hardware side is reported.
///
/// # Errors
/// The ioctl failed (interface missing or driver without the query); callers
/// treat that as "software only".
pub fn query_ts_caps(iface: &str) -> io::Result<TsCaps> {
    let sock = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    let mut info = EthtoolTsInfo {
        cmd: ETHTOOL_GET_TS_INFO,
        ..EthtoolTsInfo::default()
    };
    let mut req = IfReq::new(iface, (&raw mut info).cast())?;
    // SAFETY: `req` and `info` are live, correctly laid out `repr(C)` values
    // for the whole call; the kernel reads `ifreq` and writes at most
    // `sizeof(ethtool_ts_info)` bytes to `info` as selected by `cmd`.
    let rc = unsafe { libc::ioctl(sock.as_raw_fd(), SIOCETHTOOL as _, &raw mut req) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    let need = SOF_TX_HARDWARE | SOF_RX_HARDWARE | SOF_RAW_HARDWARE;
    let filters = info.rx_filters;
    Ok(TsCaps {
        hardware: info.so_timestamping & need == need,
        phc_index: u32::try_from(info.phc_index).ok(),
        hw_filter_ptp_v1: filters & (1 << HWTSTAMP_FILTER_PTP_V1_L4_EVENT) != 0
            || filters & (1 << HWTSTAMP_FILTER_ALL) != 0,
        hw_filter_all: filters & (1 << HWTSTAMP_FILTER_ALL) != 0,
    })
}

/// Decides the NIC configuration to set, given the current one
/// `(tx_type, rx_filter)`. Returns `Ok(None)` if it already timestamps PTPv1
/// events, `Ok(Some(new))` with a configuration that is a superset of the
/// current one, or `Err(())` if that is impossible (the current filter is
/// something else and the NIC cannot stamp everything): a filter in use by
/// another PTP daemon is never narrowed.
pub(crate) fn plan_hw_filter(
    current: (i32, i32),
    all_supported: bool,
) -> Result<Option<(i32, i32)>, ()> {
    let (tx, rx) = current;
    let tx_ok = tx != HWTSTAMP_TX_OFF;
    let rx_ok = rx == HWTSTAMP_FILTER_ALL || rx == HWTSTAMP_FILTER_PTP_V1_L4_EVENT;
    if tx_ok && rx_ok {
        return Ok(None);
    }
    let new_tx = if tx_ok { tx } else { HWTSTAMP_TX_ON };
    let new_rx = if rx_ok {
        rx
    } else if rx == HWTSTAMP_FILTER_NONE {
        HWTSTAMP_FILTER_PTP_V1_L4_EVENT
    } else if all_supported {
        HWTSTAMP_FILTER_ALL
    } else {
        return Err(());
    };
    Ok(Some((new_tx, new_rx)))
}

/// Makes sure the NIC timestamps PTPv1 event messages in hardware. It reads
/// the current configuration (`SIOCGHWTSTAMP`) first and only ever widens it
/// ([`plan_hw_filter`]); if the current configuration cannot be read it
/// refuses to write one blind. Needs privilege (`CAP_NET_ADMIN`).
///
/// # Errors
/// The current configuration is unreadable, cannot be widened safely, or the
/// ioctl failed.
pub fn enable_hw_timestamping(sock_fd: RawFd, iface: &str, caps: &TsCaps) -> io::Result<()> {
    let mut cfg = HwTstampConfig {
        flags: 0,
        tx_type: HWTSTAMP_TX_OFF,
        rx_filter: HWTSTAMP_FILTER_NONE,
    };
    let mut req = IfReq::new(iface, (&raw mut cfg).cast())?;
    // SAFETY: `req` and `cfg` are live `repr(C)` values for the whole call; the
    // kernel writes a `hwtstamp_config` through `ifr_data`. `sock_fd` is a
    // valid open socket owned by the caller.
    let rc = unsafe { libc::ioctl(sock_fd, SIOCGHWTSTAMP as _, &raw mut req) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    let plan = plan_hw_filter((cfg.tx_type, cfg.rx_filter), caps.hw_filter_all)
        .map_err(|()| io::Error::from(io::ErrorKind::Unsupported))?;
    let Some((tx, rx)) = plan else {
        return Ok(());
    };
    let mut new = HwTstampConfig {
        flags: cfg.flags,
        tx_type: tx,
        rx_filter: rx,
    };
    let mut req = IfReq::new(iface, (&raw mut new).cast())?;
    // SAFETY: as above; the kernel reads (and echoes into) `new`.
    let rc = unsafe { libc::ioctl(sock_fd, SIOCSHWTSTAMP as _, &raw mut req) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_is_only_widened() {
        // Already fine: untouched.
        assert_eq!(plan_hw_filter((1, 3), false), Ok(None));
        assert_eq!(plan_hw_filter((1, 1), false), Ok(None));
        // Off: switch on PTPv1 events.
        assert_eq!(plan_hw_filter((0, 0), false), Ok(Some((1, 3))));
        // Somebody uses a PTPv2-only filter: widen to ALL if possible,
        // never replace it with a narrower one.
        assert_eq!(plan_hw_filter((1, 6), true), Ok(Some((1, 1))));
        assert_eq!(plan_hw_filter((1, 6), false), Err(()));
        // A one-step TX mode in use is kept.
        assert_eq!(plan_hw_filter((2, 0), false), Ok(Some((2, 3))));
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PtpClockTime {
    sec: i64,
    nsec: u32,
    reserved: u32,
}

#[repr(C)]
struct PtpSysOffsetExtended {
    n_samples: u32,
    rsv: [u32; 3],
    ts: [[PtpClockTime; 3]; PTP_MAX_SAMPLES],
}

/// Read access to a PHC for converting its time to `CLOCK_REALTIME`.
#[derive(Debug)]
pub struct Phc {
    file: File,
}

impl Phc {
    /// Opens `/dev/ptp<index>` read-only.
    ///
    /// # Errors
    /// The device cannot be opened.
    pub fn open(index: u32) -> io::Result<Self> {
        Ok(Phc {
            file: File::open(format!("/dev/ptp{index}"))?,
        })
    }

    /// One bracketed reading: `(realtime_ns_mid, phc_ns)` from the sample
    /// with the narrowest bracket of `PTP_SYS_OFFSET_EXTENDED`.
    ///
    /// # Errors
    /// The ioctl failed.
    pub fn read_pair(&self) -> io::Result<(u64, u64)> {
        let mut req = PtpSysOffsetExtended {
            n_samples: 3,
            rsv: [0; 3],
            ts: [[PtpClockTime::default(); 3]; PTP_MAX_SAMPLES],
        };
        // SAFETY: `req` is a live `repr(C)` value matching the kernel's
        // `struct ptp_sys_offset_extended`; the kernel fills `ts[..n_samples]`.
        let rc = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                PTP_SYS_OFFSET_EXTENDED as _,
                &raw mut req,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        let ns = |t: &PtpClockTime| -> u64 {
            u64::try_from(t.sec)
                .unwrap_or(0)
                .saturating_mul(1_000_000_000)
                .saturating_add(u64::from(t.nsec))
        };
        let mut best: Option<(u64, u64, u64)> = None;
        for s in req.ts.iter().take(3) {
            let [before, phc, after] = s;
            let (b, a) = (ns(before), ns(after));
            let width = a.saturating_sub(b);
            if best.is_none_or(|x| width < x.0) {
                best = Some((width, b + width / 2, ns(phc)));
            }
        }
        best.map(|(_, real, phc)| (real, phc))
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))
    }
}
