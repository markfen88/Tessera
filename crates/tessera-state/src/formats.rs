//! What a device's sample rate and encoding can be set to now: one
//! definition for the values `SetSampleRate` / `SetEncoding` accept and the
//! lists the status reports and a network controller is offered
//! (`spec/protocol/device-settings.md` §3.3 items 2, 3 and 10).
//!
//! A value is refused, in this order, when the key is `locked` in the
//! device's configuration, when the daemon does not support it, or (sample
//! rate only) when an enabled bridge of the device pins its card to another
//! rate. The current value is always listed: writing it changes nothing
//! (under a lock it is refused as `Locked`, with the same readback).
//! A bridge that follows the device (no `sample_rate`) does not narrow the
//! list: whether its card can run another rate is not known before the
//! bridge reopens it.

use tessera_control_types::{FormatOptions, SettingPin};

use crate::config::{Config, DeviceLock};
use crate::env::Environment;
use crate::model::DeviceModel;

/// Why a value cannot be set now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The key is in the device's `locked` list.
    Locked,
    /// The daemon does not support the value.
    Unsupported,
    /// An enabled bridge runs its card at another rate.
    Bridge { name: String, sample_rate: u32 },
}

impl Refusal {
    fn pin(self) -> SettingPin {
        match self {
            Self::Locked => SettingPin::Configuration,
            Self::Unsupported => SettingPin::Unsupported,
            Self::Bridge { name, sample_rate } => SettingPin::Bridge { name, sample_rate },
        }
    }
}

/// Why `rate` (Hz) cannot be set on `d` now; `None` when `SetSampleRate`
/// accepts it.
#[must_use]
pub fn sample_rate_refusal(
    config: &Config,
    env: &Environment,
    d: &DeviceModel,
    rate: u32,
) -> Option<Refusal> {
    if d.locked.contains(&DeviceLock::SampleRate) {
        return Some(Refusal::Locked);
    }
    if !env.supported_sample_rates.contains(&rate) {
        return Some(Refusal::Unsupported);
    }
    if rate == d.sample_rate {
        return None;
    }
    config
        .bridges
        .iter()
        .find(|b| b.enabled && b.device == d.key.0 && b.sample_rate.is_some_and(|r| r != rate))
        .map(|b| Refusal::Bridge {
            name: b.name.clone(),
            sample_rate: b.sample_rate.unwrap_or(0),
        })
}

/// Why `bits` cannot be set on `d` now; `None` when `SetEncoding` accepts
/// it.
#[must_use]
pub fn encoding_refusal(env: &Environment, d: &DeviceModel, bits: u8) -> Option<Refusal> {
    if d.locked.contains(&DeviceLock::Encoding) {
        return Some(Refusal::Locked);
    }
    if !env.supported_encodings.contains(&bits) {
        return Some(Refusal::Unsupported);
    }
    None
}

/// The sample rates and encodings `d` can be set to now, with the reason
/// when nothing but the current value is accepted.
#[must_use]
pub fn options(config: &Config, env: &Environment, d: &DeviceModel) -> FormatOptions {
    let (sample_rates, rate_pin) = choice(
        d.sample_rate,
        env.supported_sample_rates.iter().copied(),
        |r| sample_rate_refusal(config, env, d, r),
    );
    let (encodings, encoding_pin) =
        choice(d.encoding, env.supported_encodings.iter().copied(), |b| {
            encoding_refusal(env, d, b)
        });
    FormatOptions::new(sample_rates, encodings)
        .with_sample_rate_pin(rate_pin)
        .with_encoding_pin(encoding_pin)
}

/// The current value and every candidate the command accepts, ascending;
/// when no candidate but the current value is accepted, why (the refusal of
/// the first other candidate, or `Unsupported` when there is none).
fn choice<T: Copy + Ord>(
    current: T,
    candidates: impl Iterator<Item = T>,
    refusal: impl Fn(T) -> Option<Refusal>,
) -> (Vec<T>, Option<SettingPin>) {
    let mut values = vec![current];
    let mut first_refusal = None;
    for v in candidates.filter(|v| *v != current) {
        match refusal(v) {
            None => values.push(v),
            Some(r) => {
                first_refusal.get_or_insert(r);
            }
        }
    }
    values.sort_unstable();
    values.dedup();
    let pin = (values.len() == 1).then(|| first_refusal.unwrap_or(Refusal::Unsupported).pin());
    (values, pin)
}
