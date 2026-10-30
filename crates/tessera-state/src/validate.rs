//! Semantic validation of a [`Config`]. Reports every problem at once, each
//! with the key path (`device[1].latency_us`), never just the first.

use std::collections::BTreeSet;

use tessera_control_types::Direction;

use crate::config::{Config, DeviceConfig, TickUs};
use crate::env::{Environment, validate_channel_label, validate_device_name, validate_name};
use crate::error::{Issue, Severity};

const LOG_LEVELS: [&str; 5] = ["error", "warn", "info", "debug", "trace"];

/// Validate a configuration as it is loaded. Host-dependent findings
/// (interface missing, wireless interface without permission) are warnings
/// here; [`validate_for_start`] turns them into errors.
///
/// Private removal-only report admission relies on every flow-dependent error
/// across this complete validator being unary or pairwise under fixed context.
/// An aggregate or existence-dependent error, including one in tunable checks,
/// must revisit recovery's subset argument and the whole-validator properties.
#[must_use]
pub fn validate(config: &Config, env: &Environment) -> Vec<Issue> {
    let mut issues = Vec::new();
    check(config, env, &mut issues);
    check_interfaces(config, env, Severity::Warning, &mut issues);
    issues
}

/// Validate for starting the daemon on this host: like [`validate`], but a
/// missing interface or a wireless one without `allow_wireless` is an error.
#[must_use]
pub fn validate_for_start(config: &Config, env: &Environment) -> Vec<Issue> {
    let mut issues = Vec::new();
    check(config, env, &mut issues);
    check_interfaces(config, env, Severity::Error, &mut issues);
    issues
}

fn push(issues: &mut Vec<Issue>, sev: Severity, path: String, msg: String) {
    issues.push(match sev {
        Severity::Error => Issue::error(path, msg),
        Severity::Warning => Issue::warning(path, msg),
    });
}

fn valid_log_filter(s: &str) -> Result<(), String> {
    if s.trim().is_empty() {
        return Err("must not be empty".into());
    }
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err("contains an empty directive".into());
        }
        match part.split_once('=') {
            None => {
                if !LOG_LEVELS.contains(&part.to_ascii_lowercase().as_str()) {
                    return Err(format!(
                        "`{part}` is not a level (error, warn, info, debug, trace); write targets as `target=level`"
                    ));
                }
            }
            Some((target, level)) => {
                let target = target.trim();
                let level = level.trim();
                if target.is_empty()
                    || !target
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '-'))
                {
                    return Err(format!("`{target}` is not a valid log target"));
                }
                if !LOG_LEVELS.contains(&level.to_ascii_lowercase().as_str()) {
                    return Err(format!(
                        "`{level}` is not a level (error, warn, info, debug, trace)"
                    ));
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn valid_interface_name(n: &str) -> Result<(), String> {
    // IFNAMSIZ is 16 including the NUL.
    if n.is_empty() {
        Err("interface name must not be empty".into())
    } else if n.len() > 15 {
        Err(format!("`{n}` is longer than 15 bytes (IFNAMSIZ)"))
    } else if n
        .chars()
        .any(|c| c.is_whitespace() || c == '/' || c.is_control())
        || n == "."
        || n == ".."
    {
        Err(format!("`{n}` is not a valid interface name"))
    } else {
        Ok(())
    }
}

fn check(config: &Config, env: &Environment, out: &mut Vec<Issue>) {
    let err = |out: &mut Vec<Issue>, path: &str, msg: String| {
        out.push(Issue::error(path, msg));
    };

    // [daemon]
    if let Err(e) = valid_log_filter(&config.daemon.log_level) {
        err(out, "daemon.log_level", e);
    }
    for (k, p) in [
        ("daemon.state_dir", &config.daemon.state_dir),
        ("daemon.runtime_dir", &config.daemon.runtime_dir),
    ] {
        if !p.is_absolute() {
            err(
                out,
                k,
                format!("`{}` must be an absolute path", p.display()),
            );
        }
    }
    let g = &config.daemon.admin_group;
    if g.is_empty()
        || g.chars()
            .any(|c| c.is_whitespace() || c == ':' || c.is_control())
    {
        err(
            out,
            "daemon.admin_group",
            format!("`{g}` is not a valid group name"),
        );
    }

    // [network]
    let mut seen = BTreeSet::new();
    for (i, n) in config.network.interfaces.iter().enumerate() {
        if let Err(e) = valid_interface_name(n) {
            err(out, &format!("network.interfaces[{i}]"), e);
        } else if !seen.insert(n.as_str()) {
            err(
                out,
                &format!("network.interfaces[{i}]"),
                format!("`{n}` is listed twice"),
            );
        }
    }
    if let Some(s) = &config.network.secondary {
        if let Err(e) = valid_interface_name(s) {
            err(out, "network.secondary", e);
        } else if seen.contains(s.as_str()) {
            err(
                out,
                "network.secondary",
                format!("`{s}` is already a primary interface"),
            );
        }
    }
    if !(1..=65_535).contains(&config.network.mtu_payload) {
        err(
            out,
            "network.mtu_payload",
            format!(
                "{} is outside 1..=65535 (the UDP payload range)",
                config.network.mtu_payload
            ),
        );
    }

    // [engine]
    if config.engine.workers == 0 {
        err(out, "engine.workers", "must be at least 1".into());
    }
    if let TickUs::Micros(0) = config.engine.tick_us {
        err(
            out,
            "engine.tick_us",
            "must be at least 1 (or \"auto\")".into(),
        );
    }
    for (k, v) in [
        ("engine.rt_priority", config.engine.rt_priority),
        ("engine.ptp_rt_priority", config.engine.ptp_rt_priority),
    ] {
        if !(1..=99).contains(&v) {
            err(
                out,
                k,
                format!("{v} is outside 1..=99 (the Linux SCHED_FIFO priority range)"),
            );
        }
    }
    let mut cpus = BTreeSet::new();
    for (i, c) in config.engine.cpu_affinity.iter().enumerate() {
        if !cpus.insert(*c) {
            err(
                out,
                &format!("engine.cpu_affinity[{i}]"),
                format!("CPU {c} is listed twice"),
            );
        }
    }

    // [backends]
    let mut backends = BTreeSet::new();
    for (i, b) in config.backends.enabled.iter().enumerate() {
        if !backends.insert(*b) {
            err(
                out,
                &format!("backends.enabled[{i}]"),
                format!("`{b}` is listed twice"),
            );
        }
    }

    // [[device]]
    let mut ids = BTreeSet::new();
    let mut names = BTreeSet::new();
    for (i, d) in config.devices.iter().enumerate() {
        let p = format!("device[{i}]");
        let valid_id = !d.id.is_empty()
            && d.id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !valid_id {
            err(
                out,
                &format!("{p}.id"),
                format!(
                    "`{}` must be non-empty and use only letters, digits, `_` and `-`",
                    d.id
                ),
            );
        } else if !ids.insert(d.id.as_str()) {
            err(
                out,
                &format!("{p}.id"),
                format!("duplicate device id `{}`", d.id),
            );
        }
        if let Err(e) = validate_name(&d.name, env.max_name_bytes) {
            err(out, &format!("{p}.name"), format!("device name {e}"));
        } else if !names.insert(d.name.to_ascii_lowercase()) {
            err(
                out,
                &format!("{p}.name"),
                format!(
                    "duplicate device name `{}` (names are compared ignoring case)",
                    d.name
                ),
            );
        }
        if validate_name(&d.name, env.max_name_bytes).is_ok() {
            if let Err(e) = validate_device_name(&d.name, env.max_name_bytes) {
                out.push(Issue::warning(
                    format!("{p}.name"),
                    format!("device name {e}; retained unchanged for existing subscriptions; external controllers may refuse routing"),
                ));
            }
        }
        check_device(&p, d, env, out);
    }

    check_tunables(config, env, out);

    // [[flow]]
    check_flows(config, out);

    // [[bridge]]
    check_bridges(config, env, out);
}

/// `[[bridge]]`: names, device references, channel ranges (in range, not
/// overlapping another bridge of the same device and direction), the rate
/// (equal to the device's: the resampler's nominal ratio is 1), the card
/// sizes the bridge needs, the loop tuning, and whether this program can run
/// bridges at all.
fn check_bridges(config: &Config, env: &Environment, out: &mut Vec<Issue>) {
    use crate::config::{
        BRIDGE_ERROR_FILTER_TAU_S, MAX_BRIDGE_CORRECTION_PPM, MAX_BRIDGE_TRANSIENT_S,
    };
    let err = |out: &mut Vec<Issue>, key: String, msg: String| out.push(Issue::error(key, msg));
    if !env.bridges_supported && !config.bridges.is_empty() {
        err(
            out,
            "bridge".into(),
            format!(
                "{} [[bridge]] entr{} configured, but this tesserad was built without the ALSA hardware bridge (cargo feature `alsa-bridge`); remove the entries or install a build with the bridge",
                config.bridges.len(),
                if config.bridges.len() == 1 {
                    "y is"
                } else {
                    "ies are"
                }
            ),
        );
    }
    let mut names = BTreeSet::new();
    // Enabled bridges seen so far: (index, device, direction, first, last, pcm).
    let mut ranges: Vec<(usize, &str, Direction, u32, u32, &str)> = Vec::new();
    for (i, b) in config.bridges.iter().enumerate() {
        let p = format!("bridge[{i}]");
        let valid_name = !b.name.is_empty()
            && b.name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !valid_name {
            err(
                out,
                format!("{p}.name"),
                format!(
                    "`{}` must be non-empty and use only letters, digits, `_` and `-`",
                    b.name
                ),
            );
        } else if !names.insert(b.name.as_str()) {
            err(
                out,
                format!("{p}.name"),
                format!(
                    "duplicate bridge name `{}` (two bridges of one device and direction need explicit, distinct names)",
                    b.name
                ),
            );
        }
        if b.alsa_pcm.is_empty() {
            err(out, format!("{p}.alsa_pcm"), "must not be empty".into());
        } else if b.alsa_pcm.chars().any(char::is_control) {
            err(
                out,
                format!("{p}.alsa_pcm"),
                "must not contain control characters".into(),
            );
        }
        let mut list_ok = true;
        if let Some(list) = &b.channel_list {
            list_ok = false;
            let mut seen = BTreeSet::new();
            let mut element_error = false;
            for (j, c) in list.iter().enumerate() {
                let cp = format!("{p}.channels[{j}]");
                if *c == 0 {
                    err(out, cp, "channel numbers start at 1".into());
                    element_error = true;
                } else if !seen.insert(*c) {
                    err(out, cp, format!("channel {c} is listed twice"));
                    element_error = true;
                }
            }
            if list.is_empty() {
                err(
                    out,
                    format!("{p}.channels"),
                    "must list at least one channel".into(),
                );
            } else if !element_error {
                err(
                    out,
                    format!("{p}.channels"),
                    format!(
                        "{list:?} is not one ascending run of channels: a bridge carries a contiguous range (`first_channel` and a count)"
                    ),
                );
            }
        }
        let dir = b.direction.device_direction();
        let Some(dev) = config.device(&b.device) else {
            err(
                out,
                format!("{p}.device"),
                format!("no device with id `{}`", b.device),
            );
            continue;
        };
        let available = match dir {
            Direction::Rx => dev.rx_channels,
            Direction::Tx => dev.tx_channels,
        };
        let count = b.channel_count(available);
        if !list_ok {
            // Reported above.
        } else if b.first_channel == 0 {
            err(
                out,
                format!("{p}.first_channel"),
                "channel numbers start at 1".into(),
            );
        } else if count == 0 {
            err(
                out,
                format!("{p}.channels"),
                if b.channels.is_some() {
                    "must be at least 1".into()
                } else {
                    format!(
                        "device `{}` has no {} channel at or after channel {}",
                        b.device,
                        if dir == Direction::Rx { "rx" } else { "tx" },
                        b.first_channel
                    )
                },
            );
        } else {
            let first = u32::from(b.first_channel);
            let last = first + u32::from(count) - 1;
            if last > u32::from(available) {
                err(
                    out,
                    format!("{p}.channels"),
                    format!(
                        "channels {first}..={last} are beyond device `{}`'s {available} {} channel(s)",
                        b.device,
                        if dir == Direction::Rx { "rx" } else { "tx" }
                    ),
                );
            } else if b.enabled {
                for (j, d, od, f, l, pcm) in &ranges {
                    if *d == b.device && *od == dir && first <= *l && *f <= last {
                        err(
                            out,
                            format!("{p}.channels"),
                            format!(
                                "channels {first}..={last} overlap channels {f}..={l} of bridge[{j}] on the same device and direction"
                            ),
                        );
                    }
                    if *pcm == b.alsa_pcm && *od == dir {
                        err(
                            out,
                            format!("{p}.alsa_pcm"),
                            format!(
                                "`{}` is already used in this direction by bridge[{j}]",
                                b.alsa_pcm
                            ),
                        );
                    }
                }
                ranges.push((i, b.device.as_str(), dir, first, last, b.alsa_pcm.as_str()));
            }
        }
        if let Some(r) = b.sample_rate.filter(|r| *r != dev.sample_rate) {
            err(
                out,
                format!("{p}.sample_rate"),
                format!(
                    "{r} Hz differs from device `{}`'s {} Hz: the bridge compensates clock drift, not rate differences (its resampler's nominal ratio is 1); leave `sample_rate` out to follow the device",
                    b.device, dev.sample_rate
                ),
            );
        }
        let rate = dev.sample_rate;
        match b.period_frames {
            Some(0) => err(
                out,
                format!("{p}.period_frames"),
                "must be at least 1".into(),
            ),
            _ => {
                let (period, buffer) = (b.period(rate), b.buffer(rate));
                if buffer == 0 {
                    err(
                        out,
                        format!("{p}.buffer_frames"),
                        "must be at least 1".into(),
                    );
                } else if u64::from(buffer) < 2 * u64::from(period) {
                    err(
                        out,
                        format!("{p}.buffer_frames"),
                        format!(
                            "{buffer} frames cannot hold two periods of {period} frames (the bridge keeps one period free and at least one queued)"
                        ),
                    );
                }
            }
        }
        let tau = b.time_constant_s;
        if !(tau.is_finite() && tau > 0.0) {
            err(
                out,
                format!("{p}.time_constant_s"),
                format!("{tau} must be a positive number of seconds"),
            );
        } else if !(1.0 / tau).powi(2).is_finite() {
            // The loop's integral gain is (1 / tau)^2 per frame and second:
            // the PI controller refuses a gain that is not finite.
            err(
                out,
                format!("{p}.time_constant_s"),
                format!("{tau} s is too short for the loop's gains to be finite numbers"),
            );
        } else if 0.4 * b.max_correction_ppm.max(0.0) * 1e-6 * tau > MAX_BRIDGE_TRANSIENT_S {
            err(
                out,
                format!("{p}.time_constant_s"),
                format!(
                    "{tau} s with a {} ppm clamp lets {:.0} s of position error build up, more than {MAX_BRIDGE_TRANSIENT_S} s (the guard and the ring would grow by that much); shorten the time constant or lower the clamp",
                    b.max_correction_ppm,
                    0.4 * b.max_correction_ppm * 1e-6 * tau
                ),
            );
        } else if tau < 10.0 * BRIDGE_ERROR_FILTER_TAU_S {
            out.push(Issue::warning(
                format!("{p}.time_constant_s"),
                format!(
                    "{tau} s is less than ten times the bridge's {BRIDGE_ERROR_FILTER_TAU_S} s filter on its measured error: the loop may ring. If a second servo follows (CamillaDSP rate adjust), keep it at least ten times that loop's adjustment period"
                ),
            ));
        }
        let ppm = b.max_correction_ppm;
        if !(ppm.is_finite() && ppm > 0.0 && ppm <= MAX_BRIDGE_CORRECTION_PPM) {
            err(
                out,
                format!("{p}.max_correction_ppm"),
                format!(
                    "{ppm} is outside (0, {MAX_BRIDGE_CORRECTION_PPM}] ppm (the range the bridge's resampler is built for)"
                ),
            );
        }
        if b.rt_priority > 99 {
            err(
                out,
                format!("{p}.rt_priority"),
                format!(
                    "{} is outside 0..=99 (0: normal scheduling; 1..=99: the Linux SCHED_FIFO range)",
                    b.rt_priority
                ),
            );
        }
    }
}

/// The multicast group an rx flow listens on: its `local_address`, when
/// that is a group (`flows-media.md` §5: the receiver binds the group).
fn listened_group(f: &crate::config::FlowConfig) -> Option<std::net::Ipv4Addr> {
    f.local_address
        .filter(|a| f.direction == Direction::Rx && a.is_multicast())
}

/// `[[flow]]` (PROVISIONAL static flows): ids, device references, channel
/// ranges, ports. Packet-size limits depend on the packet format and are
/// checked by the daemon when it builds the flow.
///
/// An rx flow whose `local_address` is a multicast group listens on that
/// group. Every socket bound to a group receives every datagram, so several
/// such flows may share the group's port and sender (one per input, each
/// independent of the others), and in such a flow a channel of 0 marks a
/// wire position that no device channel takes.
// Private report-admission removal-only recovery relies on flow errors being
// unary or pairwise under fixed configuration. A new dependency/existence or
// aggregate rule must revisit that subset-safety argument and its properties.
fn check_flows(config: &Config, out: &mut Vec<Issue>) {
    let err = |out: &mut Vec<Issue>, key: String, msg: String| out.push(Issue::error(key, msg));
    let mut ids = BTreeSet::new();
    let mut endpoints = std::collections::HashMap::new();
    // rx flows seen so far: (index, device, channels (explicit), flow).
    let mut rx_seen: Vec<(usize, &crate::config::FlowConfig)> = Vec::new();
    for (i, f) in config.flows.iter().enumerate() {
        let p = format!("flow[{i}]");
        let group = listened_group(f);
        if group.is_none() {
            if let Some(previous) = endpoints.insert((f.peer, f.port, f.direction), i) {
                err(
                    out,
                    format!("{p}.peer"),
                    format!("one flow per peer, port and direction: duplicates flow[{previous}]"),
                );
            }
        }
        if f.direction == Direction::Tx && f.local_address.is_some_and(|a| a.is_multicast()) {
            err(
                out,
                format!("{p}.local_address"),
                "only an rx flow can listen on a multicast group; a tx flow sends to a group named in `peer`".into(),
            );
        }
        let valid_id = !f.id.is_empty()
            && f.id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !valid_id {
            err(
                out,
                format!("{p}.id"),
                format!(
                    "`{}` must be non-empty and use only letters, digits, `_` and `-`",
                    f.id
                ),
            );
        } else if !ids.insert(f.id.as_str()) {
            err(
                out,
                format!("{p}.id"),
                format!("duplicate flow id `{}`", f.id),
            );
        }
        let dev = config.device(&f.device);
        if dev.is_none() {
            err(
                out,
                format!("{p}.device"),
                format!("no device with id `{}`", f.device),
            );
        }
        if f.port == 0 {
            err(out, format!("{p}.port"), "must not be 0".into());
        }
        if f.direction == Direction::Tx && f.peer.is_none() {
            err(
                out,
                format!("{p}.peer"),
                "a tx flow needs the destination address in `peer`".into(),
            );
        }
        if let Some(peer) = f.peer {
            if peer.is_unspecified() || peer.is_broadcast() {
                err(
                    out,
                    format!("{p}.peer"),
                    format!("`{peer}` is not a usable peer address"),
                );
            } else if f.direction == Direction::Rx && peer.is_multicast() {
                err(
                    out,
                    format!("{p}.peer"),
                    format!(
                        "`{peer}` is a multicast address; an rx flow's `peer` is the sender's unicast address (to receive a multicast group, set the group as `local_address`)"
                    ),
                );
            }
        }
        if f.direction == Direction::Rx {
            if f.peer.is_none() && !f.any_source {
                err(
                    out,
                    format!("{p}.peer"),
                    "an rx flow accepts only the sender it names: set `peer`, or `any_source = true` to accept datagrams from any address".into(),
                );
            }
            if f.peer.is_some() && f.any_source {
                err(
                    out,
                    format!("{p}.any_source"),
                    "contradicts `peer`: name the sender or accept any source, not both".into(),
                );
            }
            // Two rx flows must not listen on one port (a missing interface
            // or address may resolve to the other flow's, so it counts as a
            // possible clash; a missing address is the interface's, never a
            // group), nor carry the same device channel. Listeners on one
            // multicast group share it.
            for (j, g) in &rx_seen {
                let same_if =
                    f.interface == g.interface || f.interface.is_none() || g.interface.is_none();
                let same_addr = match (f.local_address, g.local_address) {
                    (Some(a), Some(b)) => a == b,
                    (Some(a), None) | (None, Some(a)) => !a.is_multicast(),
                    (None, None) => true,
                };
                let shared_group = group.is_some() && group == listened_group(g);
                if f.port != 0 && f.port == g.port && same_if && same_addr && !shared_group {
                    err(
                        out,
                        format!("{p}.port"),
                        format!(
                            "rx flow `{}` (flow[{j}]) may already listen on port {}",
                            g.id, f.port
                        ),
                    );
                }
                if f.device == g.device {
                    let all = |x: &crate::config::FlowConfig| {
                        config.device(&x.device).map_or(0, |d| d.rx_channels)
                    };
                    let chans = |x: &crate::config::FlowConfig| -> BTreeSet<u16> {
                        if x.channels.is_empty() {
                            (1..=all(x)).collect()
                        } else {
                            x.channels.iter().copied().filter(|c| *c != 0).collect()
                        }
                    };
                    let (a, b) = (chans(f), chans(g));
                    if let Some(c) = a.intersection(&b).next() {
                        err(
                            out,
                            format!("{p}.channels"),
                            format!(
                                "device channel {c} is already received by rx flow `{}` (flow[{j}])",
                                g.id
                            ),
                        );
                    }
                }
            }
            rx_seen.push((i, f));
        } else if f.any_source {
            err(
                out,
                format!("{p}.any_source"),
                "only applies to rx flows".into(),
            );
        }
        if f.max_lag_us == Some(0) {
            err(out, format!("{p}.max_lag_us"), "must be at least 1".into());
        }
        if f.frames_per_packet == Some(0) {
            err(
                out,
                format!("{p}.frames_per_packet"),
                "must be at least 1".into(),
            );
        }
        let count = dev.map(|d| match f.direction {
            Direction::Rx => d.rx_channels,
            Direction::Tx => d.tx_channels,
        });
        let mut seen = BTreeSet::new();
        for (j, c) in f.channels.iter().enumerate() {
            let cp = format!("{p}.channels[{j}]");
            if *c == 0 {
                // In a group listener: a wire position no device channel
                // takes; in a tx flow: a position sent as silence
                // (`control-flows.md` §1).
                if group.is_none() && f.direction == Direction::Rx {
                    err(out, cp, "channel numbers start at 1".into());
                }
            } else if let Some(n) = count.filter(|n| c > n) {
                err(
                    out,
                    cp,
                    format!("channel {c} is beyond the device's {n} channel(s) in that direction"),
                );
            } else if f.direction == Direction::Rx && !seen.insert(*c) {
                err(out, cp, format!("channel {c} is listed twice"));
            }
        }
        if f.direction == Direction::Tx
            && !f.channels.is_empty()
            && f.channels.iter().all(|c| *c == 0)
        {
            err(
                out,
                format!("{p}.channels"),
                "a tx flow must carry at least one channel; 0 only marks a silent position".into(),
            );
        }
        if f.channels.is_empty() && count == Some(0) {
            err(
                out,
                format!("{p}.channels"),
                "the device has no channels in that direction".into(),
            );
        }
    }
}

fn check_device(p: &str, d: &DeviceConfig, env: &Environment, out: &mut Vec<Issue>) {
    let err = |out: &mut Vec<Issue>, key: &str, msg: String| {
        out.push(Issue::error(format!("{p}.{key}"), msg));
    };
    // One direction may be 0 (rx-only, tx-only), not both.
    if d.rx_channels == 0 && d.tx_channels == 0 {
        err(
            out,
            "tx_channels",
            format!(
                "device `{}` has no channels: rx_channels and tx_channels are both 0; give at least one direction some channels",
                d.id
            ),
        );
    }
    if !env.supported_sample_rates.contains(&d.sample_rate) {
        err(
            out,
            "sample_rate",
            format!(
                "{} is not a supported rate (supported: {})",
                d.sample_rate,
                join(&env.supported_sample_rates)
            ),
        );
    }
    if !env.supported_encodings.contains(&d.encoding) {
        err(
            out,
            "encoding",
            format!(
                "{} is not a supported encoding (supported: {})",
                d.encoding,
                join(&env.supported_encodings)
            ),
        );
    }
    let [lo, hi] = d.latency_bounds_us;
    if lo == 0 {
        err(
            out,
            "latency_bounds_us",
            "the minimum must be at least 1".into(),
        );
    }
    if lo > hi {
        err(
            out,
            "latency_bounds_us",
            format!("minimum {lo} is greater than maximum {hi}"),
        );
    }
    if d.latency_us < lo || d.latency_us > hi {
        err(
            out,
            "latency_us",
            format!("{} is outside latency_bounds_us [{lo}, {hi}]", d.latency_us),
        );
    }
    if d.latency_us < env.latency_floor_us {
        let msg = format!(
            "{} is below this platform's timing floor of {} (ADR 0011)",
            d.latency_us, env.latency_floor_us
        );
        if d.allow_unsafe_latency {
            out.push(Issue::warning(format!("{p}.latency_us"), msg));
        } else {
            err(
                out,
                "latency_us",
                format!("{msg}; set allow_unsafe_latency = true to accept it"),
            );
        }
    }
    for (key, labels, count) in [
        ("rx_channel_labels", &d.rx_channel_labels, d.rx_channels),
        ("tx_channel_labels", &d.tx_channel_labels, d.tx_channels),
    ] {
        for (ch, label) in labels {
            if *ch > count {
                err(
                    out,
                    &format!("{key}.{ch}"),
                    format!("channel {ch} is beyond the device's {count} channel(s)"),
                );
            }
            if let Err(e) = validate_name(label, env.max_name_bytes) {
                err(out, &format!("{key}.{ch}"), format!("label {e}"));
            } else if let Err(e) = validate_channel_label(label, env.max_name_bytes) {
                out.push(Issue::warning(
                    format!("{p}.{key}.{ch}"),
                    format!("label {e}; retained unchanged for existing subscriptions; external controllers may refuse routing"),
                ));
            }
        }
    }
}

fn join<T: ToString>(v: &[T]) -> String {
    v.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn check_interfaces(config: &Config, env: &Environment, sev: Severity, out: &mut Vec<Issue>) {
    let named = config
        .network
        .interfaces
        .iter()
        .enumerate()
        .map(|(i, n)| (format!("network.interfaces[{i}]"), n))
        .chain(
            config
                .network
                .secondary
                .iter()
                .map(|n| ("network.secondary".to_owned(), n)),
        );
    for (path, name) in named {
        if valid_interface_name(name).is_err() {
            continue; // already reported as an error
        }
        match env.interfaces.lookup(name) {
            None => push(
                out,
                sev,
                path,
                format!("interface `{name}` does not exist on this host"),
            ),
            Some(info) if info.wireless && !config.network.allow_wireless => push(
                out,
                sev,
                path,
                format!(
                    "`{name}` is a wireless interface; set allow_wireless = true to use it for media"
                ),
            ),
            Some(info) if info.wireless => out.push(Issue::warning(
                path,
                format!("`{name}` is wireless; media quality will suffer"),
            )),
            Some(_) => {}
        }
    }
}

/// Largest timeout in seconds (about 136 years): adding it to the current
/// monotonic time can never overflow (`Instant + Duration` panics on
/// overflow).
pub const MAX_TIMEOUT_SECS: u64 = u32::MAX as u64;

/// Largest worst-case memory of the event queues (every subscriber stalled
/// with a full queue): 2 GiB, the address space a 32-bit target (armv7)
/// can give one process.
pub const MAX_EVENT_QUEUE_BYTES: u64 = 2 << 30;

/// Largest `rx_batch` / `tx_batch`: Linux `UIO_MAXIOV`, the most messages
/// one `recvmmsg`/`sendmmsg` call moves.
pub const UIO_MAXIOV: u32 = 1024;

/// Smallest `[control] max_frame_bytes`: the daemon's own normal replies
/// (a status of a few devices, an error frame) must always fit.
pub const MIN_MAX_FRAME_BYTES: u64 = 64 << 10;

/// The process tunables (`[daemon]`, `[control]`, `[audio]`, `[engine]`
/// additions, `[defaults]` and the per-entry overrides). Floors protect the
/// system; ceilings come only from integer width or memory.
fn check_tunables(config: &Config, env: &Environment, out: &mut Vec<Issue>) {
    let err = |out: &mut Vec<Issue>, path: &str, msg: String| {
        out.push(Issue::error(path, msg));
    };
    let at_least_one = |out: &mut Vec<Issue>, path: &str, v: u64, why: &str| {
        if v == 0 {
            out.push(Issue::error(path, format!("must be at least 1 ({why})")));
        }
    };
    let d = &config.daemon;
    at_least_one(
        out,
        "daemon.shutdown_timeout_secs",
        d.shutdown_timeout_secs,
        "the final state flush needs time",
    );

    let c = &config.control;
    let mut routing_sources = BTreeSet::new();
    for (index, source) in c.routing_sources.iter().enumerate() {
        if source.len() != 16
            || !source.bytes().all(|b| b.is_ascii_hexdigit())
            || source.bytes().all(|b| b == b'0')
            || !routing_sources.insert(source.to_ascii_lowercase())
        {
            err(
                out,
                &format!("control.routing_sources[{index}]"),
                "expected a unique nonzero native wire identity of 16 hexadecimal digits".into(),
            );
        }
    }
    if c.max_frame_bytes < MIN_MAX_FRAME_BYTES {
        err(
            out,
            "control.max_frame_bytes",
            format!(
                "{} is below the floor of {MIN_MAX_FRAME_BYTES} bytes (64 KiB): the daemon could no longer send its own normal replies",
                c.max_frame_bytes
            ),
        );
    } else if c.max_frame_bytes > u64::from(u32::MAX) {
        err(
            out,
            "control.max_frame_bytes",
            format!(
                "{} is above {} (the frame length prefix is 32 bits)",
                c.max_frame_bytes,
                u32::MAX
            ),
        );
    }
    for (k, v, why) in [
        (
            "control.max_connections",
            c.max_connections,
            "nobody could connect",
        ),
        (
            "control.max_connections_per_uid",
            c.max_connections_per_uid,
            "nobody could connect",
        ),
        (
            "control.event_queue",
            c.event_queue,
            "subscribers would get no events",
        ),
        (
            "control.rate_burst",
            c.rate_burst,
            "every request would be refused",
        ),
        (
            "control.rate_per_second",
            c.rate_per_second,
            "every request would be refused",
        ),
        (
            "control.threads",
            c.threads,
            "the control plane needs a thread",
        ),
    ] {
        at_least_one(out, k, u64::from(v), why);
    }
    for (k, v) in [
        ("control.idle_timeout_secs", c.idle_timeout_secs),
        ("control.frame_timeout_secs", c.frame_timeout_secs),
        ("control.write_timeout_secs", c.write_timeout_secs),
        (
            "control.stream_write_timeout_secs",
            c.stream_write_timeout_secs,
        ),
    ] {
        at_least_one(
            out,
            k,
            v,
            "a zero timeout would close every connection at once",
        );
    }
    for (k, v) in [
        ("daemon.shutdown_timeout_secs", d.shutdown_timeout_secs),
        ("control.idle_timeout_secs", c.idle_timeout_secs),
        ("control.frame_timeout_secs", c.frame_timeout_secs),
        ("control.write_timeout_secs", c.write_timeout_secs),
        (
            "control.stream_write_timeout_secs",
            c.stream_write_timeout_secs,
        ),
    ] {
        if v > MAX_TIMEOUT_SECS {
            err(
                out,
                k,
                format!(
                    "{v} s is above {MAX_TIMEOUT_SECS} s: now plus the timeout must stay representable on the monotonic clock"
                ),
            );
        }
    }
    if d.persist_delay_ms > MAX_TIMEOUT_SECS * 1000 {
        err(
            out,
            "daemon.persist_delay_ms",
            format!(
                "{} ms is above {} ms: now plus the delay must stay representable on the monotonic clock",
                d.persist_delay_ms,
                MAX_TIMEOUT_SECS * 1000
            ),
        );
    }
    // Every stalled subscriber can hold a full queue: the worst case must
    // fit the 2 GiB address space of a 32-bit target (armv7).
    let event_bytes =
        u64::try_from(std::mem::size_of::<tessera_control_types::Event>()).unwrap_or(u64::MAX);
    let worst = u64::from(c.event_queue)
        .saturating_mul(u64::from(c.max_connections))
        .saturating_mul(event_bytes);
    if worst > MAX_EVENT_QUEUE_BYTES {
        err(
            out,
            "control.event_queue",
            format!(
                "{} events x {} connections x {event_bytes} bytes = {worst} bytes if every subscriber stalls, above {MAX_EVENT_QUEUE_BYTES} bytes (the address space of a 32-bit target); lower event_queue or max_connections",
                c.event_queue, c.max_connections
            ),
        );
    }
    if c.socket_mode & 0o600 != 0o600 {
        out.push(Issue::warning(
            "control.socket_mode",
            format!(
                "0o{:o} does not give the owner read and write: the daemon's own tools cannot connect",
                c.socket_mode
            ),
        ));
    }
    if c.admin_reserved_connections >= c.max_connections && c.max_connections > 0 {
        err(
            out,
            "control.admin_reserved_connections",
            format!(
                "{} reserved slots leave none of the {} connections for other users; reserve fewer",
                c.admin_reserved_connections, c.max_connections
            ),
        );
    }
    if c.socket_mode > 0o777 {
        err(
            out,
            "control.socket_mode",
            format!("0o{:o} has bits outside 0o777", c.socket_mode),
        );
    }
    if let Some(g) = &c.socket_group {
        if g.is_empty()
            || g.chars()
                .any(|ch| ch.is_whitespace() || ch == ':' || ch.is_control())
        {
            err(
                out,
                "control.socket_group",
                format!("`{g}` is not a valid group name or number"),
            );
        }
    }

    let a = &config.audio;
    for (k, v, why) in [
        (
            "audio.max_clients",
            a.max_clients,
            "use `socket = \"off\"` to refuse every client",
        ),
        (
            "audio.max_clients_per_uid",
            a.max_clients_per_uid,
            "use `socket = \"off\"` to refuse every client",
        ),
        (
            "audio.default_period_frames",
            a.default_period_frames,
            "a period has frames",
        ),
        (
            "audio.max_period_frames",
            a.max_period_frames,
            "a period has frames",
        ),
    ] {
        at_least_one(out, k, u64::from(v), why);
    }
    for (k, v) in [
        ("audio.ring_budget_mib", a.ring_budget_mib),
        ("audio.max_ring_mib", a.max_ring_mib),
    ] {
        at_least_one(out, k, v, "no client could attach");
        if v > u64::MAX >> 20 {
            err(out, k, format!("{v} MiB does not fit in 64 bits of bytes"));
        }
    }
    if a.default_period_frames > a.max_period_frames {
        err(
            out,
            "audio.default_period_frames",
            format!(
                "{} is above max_period_frames {}",
                a.default_period_frames, a.max_period_frames
            ),
        );
    }
    if a.max_ring_mib > a.ring_budget_mib {
        err(
            out,
            "audio.max_ring_mib",
            format!(
                "{} MiB is above ring_budget_mib {} MiB: no single ring can be that large",
                a.max_ring_mib, a.ring_budget_mib
            ),
        );
    }
    let budget = a.ring_budget_mib.saturating_mul(1 << 20);
    // Packet buffers are preallocated per media socket and locked with the
    // rest: `rx_batch` (rx flows) or `tx_batch` (tx flows) packets of at
    // most the MTU payload plus a header.
    let packet = u64::from(config.network.mtu_payload).saturating_add(64);
    let sockets: u64 = config
        .flows
        .iter()
        .map(|f| {
            u64::from(match f.direction {
                Direction::Rx => config.engine.rx_batch,
                Direction::Tx => config.engine.tx_batch,
            })
            .saturating_mul(packet)
        })
        .fold(0, u64::saturating_add);
    let locked_bytes = budget.saturating_add(sockets);
    if let (Some(limit), true) = (env.memlock_limit_bytes, config.engine.mlock) {
        if locked_bytes > limit && config.daemon.realtime {
            out.push(Issue::warning(
                "audio.ring_budget_mib",
                format!(
                    "the ring budget of {budget} bytes plus {sockets} bytes of packet buffers is above RLIMIT_MEMLOCK ({limit} bytes): memory beyond the limit cannot be locked; raise the limit or lower the budget"
                ),
            ));
        }
    }
    for (i, f) in config.flows.iter().enumerate() {
        if f.direction == Direction::Tx {
            for (k, set) in [
                ("rx_keepalive_ms", f.rx_keepalive_ms.is_some()),
                (
                    "rx_keepalive_timeout_ms",
                    f.rx_keepalive_timeout_ms.is_some(),
                ),
                ("rx_keepalive_payload", f.rx_keepalive_payload.is_some()),
            ] {
                if set {
                    out.push(Issue::warning(
                        format!("flow[{i}].{k}"),
                        "receiver keepalives are sent by rx flows only; ignored on a tx flow",
                    ));
                }
            }
        }
    }

    let e = &config.engine;
    for (k, v) in [
        ("engine.rx_batch", e.rx_batch),
        ("engine.rx_max_batches", e.rx_max_batches),
        ("engine.tx_batch", e.tx_batch),
        ("engine.tx_max_per_tick", e.tx_max_per_tick),
        ("engine.socket_buffer_kib", e.socket_buffer_kib),
    ] {
        at_least_one(out, k, u64::from(v), "the engine would move no packets");
    }
    for (k, v) in [
        ("engine.rx_batch", e.rx_batch),
        ("engine.tx_batch", e.tx_batch),
    ] {
        if v > UIO_MAXIOV {
            err(
                out,
                k,
                format!(
                    "{v} is above {UIO_MAXIOV} (UIO_MAXIOV): the kernel moves at most that many datagrams per call, the rest would be locked memory for nothing"
                ),
            );
        }
    }
    // SO_RCVBUF/SO_SNDBUF take a C int of bytes.
    let max_kib = u32::try_from(i32::MAX / 1024).unwrap_or(u32::MAX);
    if e.socket_buffer_kib > max_kib {
        err(
            out,
            "engine.socket_buffer_kib",
            format!(
                "{} KiB is above {max_kib} KiB (the socket option is a 32-bit byte count)",
                e.socket_buffer_kib
            ),
        );
    }

    // In [defaults.bridge] a maximum still at its built-in default means
    // the user set the minimum: report an order problem there.
    let built_in = crate::config::BridgeDefaults::default();
    let d = &config.defaults.bridge;
    check_backoff(
        "defaults.bridge",
        d.error_backoff_min_ms,
        d.error_backoff_max_ms,
        d.error_backoff_max_ms == built_in.error_backoff_max_ms,
        out,
    );
    for (i, b) in config.bridges.iter().enumerate() {
        if b.error_backoff_min_ms.is_some() || b.error_backoff_max_ms.is_some() {
            let r = b.backoff(&config.defaults.bridge);
            // Only the minimum set in this entry: an order problem is its
            // fault, report it there.
            let blame_min = b.error_backoff_max_ms.is_none();
            check_backoff(&format!("bridge[{i}]"), r.min_ms, r.max_ms, blame_min, out);
        }
    }
    check_error_sleep(
        "defaults.bridge",
        d.error_sleep_min_ms,
        d.error_sleep_max_ms,
        d.error_sleep_max_ms == built_in.error_sleep_max_ms,
        out,
    );
    for (i, b) in config.bridges.iter().enumerate() {
        if b.error_sleep_min_ms.is_some() || b.error_sleep_max_ms.is_some() {
            let r = b.error_sleep(&config.defaults.bridge);
            let blame_min = b.error_sleep_max_ms.is_none();
            check_error_sleep(&format!("bridge[{i}]"), r.min_ms, r.max_ms, blame_min, out);
        }
    }
}

/// `min > max`, reported under the key the user set: `min` when only the
/// minimum comes from this table (`blame_min`), else `max`.
fn order_issue(p: &str, what: &str, min: u32, max: u32, blame_min: bool) -> Issue {
    if blame_min {
        Issue::error(
            format!("{p}.{what}_min_ms"),
            format!("{min} ms is above {what}_max_ms ({max} ms)"),
        )
    } else {
        Issue::error(
            format!("{p}.{what}_max_ms"),
            format!("{max} ms is below {what}_min_ms ({min} ms)"),
        )
    }
}

/// `MIN_BRIDGE_ERROR_SLEEP_MS <= min <= max <= MAX_BRIDGE_ERROR_SLEEP_MS`.
fn check_error_sleep(p: &str, min: u32, max: u32, blame_min: bool, out: &mut Vec<Issue>) {
    use crate::config::{MAX_BRIDGE_ERROR_SLEEP_MS, MIN_BRIDGE_ERROR_SLEEP_MS};
    if min < MIN_BRIDGE_ERROR_SLEEP_MS {
        out.push(Issue::error(
            format!("{p}.error_sleep_min_ms"),
            format!(
                "{min} ms is below the floor of {MIN_BRIDGE_ERROR_SLEEP_MS} ms: a card that keeps failing would make the real-time bridge thread spin"
            ),
        ));
    }
    if max > MAX_BRIDGE_ERROR_SLEEP_MS {
        out.push(Issue::error(
            format!("{p}.error_sleep_max_ms"),
            format!(
                "{max} ms is above the ceiling of {MAX_BRIDGE_ERROR_SLEEP_MS} ms: the bridge would miss whole card buffers while it recovers"
            ),
        ));
    }
    if min > max {
        out.push(order_issue(p, "error_sleep", min, max, blame_min));
    }
}

/// `MIN_BRIDGE_BACKOFF_MS <= min <= max <= MAX_BRIDGE_BACKOFF_MS`.
fn check_backoff(p: &str, min: u32, max: u32, blame_min: bool, out: &mut Vec<Issue>) {
    use crate::config::{MAX_BRIDGE_BACKOFF_MS, MIN_BRIDGE_BACKOFF_MS};
    if min < MIN_BRIDGE_BACKOFF_MS {
        out.push(Issue::error(
            format!("{p}.error_backoff_min_ms"),
            format!(
                "{min} ms is below the floor of {MIN_BRIDGE_BACKOFF_MS} ms: a card that keeps failing would be reopened, and its real-time bridge thread restarted, in a tight loop"
            ),
        ));
    }
    if max > MAX_BRIDGE_BACKOFF_MS {
        out.push(Issue::error(
            format!("{p}.error_backoff_max_ms"),
            format!("{max} ms is above the ceiling of {MAX_BRIDGE_BACKOFF_MS} ms (10 minutes)"),
        ));
    }
    if min > max {
        out.push(order_issue(p, "error_backoff", min, max, blame_min));
    }
}
