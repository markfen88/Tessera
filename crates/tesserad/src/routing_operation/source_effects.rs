//! The host's compatibility runtime plans one carrier per source host. It
//! refuses a second input on the same receiver that selects a source channel
//! another input already selects, and a change can briefly restart the other
//! inputs fed from that source device (spec/runtime-host-source-effects.md).
//! Refuse the duplicate before dispatch so the receiver never stores a
//! selection it will not carry. The disclosed restart is accepted settling.
//!
//! A daemon serving the device role itself ([`crate::native`]) requests a
//! repeated channel once per input, so it has no such restriction.
use super::Expected;
use tessera_control_types::{CommandError, ReceiverSelection};

pub(super) fn admit(
    expected: &[Expected],
    rx: u16,
    desired: Option<&ReceiverSelection>,
) -> Result<(), CommandError> {
    if fan_out() {
        return Ok(());
    }
    if let Some(desired) = desired {
        if expected
            .iter()
            .any(|e| e.rx != rx && e.source.as_ref() == Some(desired))
        {
            return Err(crate::runtime_host::unavailable(
                "this receiver cannot feed one source channel to two of its inputs yet",
            ));
        }
    }
    Ok(())
}

/// Whether this host's receiver can feed one source channel to several inputs.
pub(super) fn fan_out() -> bool {
    crate::native::local().is_some()
}
