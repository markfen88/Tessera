//! Private logical launch reservations. Instance uniqueness, prior runtime
//! absence and the pre-exec process witness are supplied trusted premises.
//! No labels, PID lookup, wire token or production attestation is implemented.
use super::guards::{Binding, Refusal};
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DaemonInstance(pub(super) u128);
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LaunchAttempt(pub(super) u128);
#[derive(Debug, Clone, Copy)]
pub(super) struct PriorRuntimeAbsent;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PreExecWitness {
    pub(super) lifetime: Lifetime,
    pub(super) attempt: LaunchAttempt,
    /// An opaque trusted process incarnation, not a caller-supplied PID.
    pub(super) process: u128,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LaunchEnd {
    Consumed,
    Expired,
    Superseded,
    Abandoned,
    ForeignWrite,
    LockChanged,
    Reload,
    LifetimeEnded,
    Invalidated,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Reservation {
    pub(super) lifetime: Lifetime,
    pub(super) binding: Binding,
    pub(super) attempt: LaunchAttempt,
    pub(super) generation: u64,
    pub(super) epoch: u64,
    pub(super) expires: u64,
}
/// Owned by the trusted daemon launcher outside any resettable Authority.
/// No Clone, Default or deserialization: reconstruction retains this issuer.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct LaunchIssuer {
    instance: DaemonInstance,
    serial: u64,
}
impl LaunchIssuer {
    pub(super) fn new(instance: DaemonInstance) -> Self {
        Self {
            instance,
            serial: 0,
        }
    }
    pub(super) fn instance(&self) -> DaemonInstance {
        self.instance
    }
    pub(super) fn reserve(
        &mut self,
        binding: Binding,
        attempt: LaunchAttempt,
        revisions: (u64, u64),
        clock: (u64, u64),
        _absent: PriorRuntimeAbsent,
    ) -> Result<Reservation, Refusal> {
        let (now, valid_for) = clock;
        if !binding.valid() || attempt.0 == 0 || valid_for == 0 || valid_for > 120_000 {
            return Err(Refusal::BindingChanged);
        }
        let expires = now.checked_add(valid_for).ok_or(Refusal::Exhausted)?;
        let serial = self.serial.checked_add(1).ok_or(Refusal::Exhausted)?;
        self.serial = serial;
        Ok(Reservation {
            lifetime: Lifetime {
                instance: self.instance,
                serial,
            },
            binding,
            attempt,
            generation: revisions.0,
            epoch: revisions.1,
            expires,
        })
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct LaunchState {
    pub(super) pending: Option<Reservation>,
    pub(super) last_end: Option<(Lifetime, LaunchEnd)>,
    pub(super) active_process: Option<PreExecWitness>,
}
impl LaunchState {
    pub(super) fn revoke(&mut self, cause: LaunchEnd) {
        if let Some(reservation) = self.pending.take() {
            self.last_end = Some((reservation.lifetime, cause));
        }
    }
    pub(super) fn replace(&mut self, next: Reservation) {
        self.revoke(LaunchEnd::Superseded);
        self.pending = Some(next);
    }
    pub(super) fn association(
        &mut self,
        identity: guards::Identity,
        binding: &Binding,
        witness: PreExecWitness,
        revisions: (u64, u64),
        now: u64,
    ) -> Result<Reservation, Refusal> {
        let guards::Identity::Verified(lifetime) = identity else {
            return Err(Refusal::WriterUnverified);
        };
        let pending = self.pending.as_ref().ok_or(Refusal::LifetimeUnknown)?;
        // Wrong holders, bindings and witnesses cannot consume a different
        // launch's permission, including when that permission is now expired.
        if lifetime != pending.lifetime || witness.lifetime != lifetime {
            return Err(Refusal::LifetimeUnknown);
        }
        if binding != &pending.binding {
            return Err(Refusal::BindingChanged);
        }
        if witness.attempt != pending.attempt || witness.process == 0 {
            return Err(Refusal::LifetimeUnknown);
        }
        if now >= pending.expires {
            self.revoke(LaunchEnd::Expired);
            return Err(Refusal::LifetimeUnknown);
        }
        if revisions != (pending.generation, pending.epoch) {
            self.revoke(LaunchEnd::Invalidated);
            return Err(Refusal::LifetimeUnknown);
        }
        Ok(pending.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn binding() -> Binding {
        Binding {
            device: "main".into(),
            interface: "audio0".into(),
            address: Ipv4Addr::new(192, 0, 2, 1),
        }
    }
    #[test]
    fn issuer_exhaustion_and_invalid_deadlines_do_not_wrap_or_mint_handles() {
        let mut issuer = LaunchIssuer {
            instance: TEST_INSTANCE,
            serial: u64::MAX,
        };
        assert_eq!(
            issuer.reserve(
                binding(),
                LaunchAttempt(1),
                (0, 0),
                (0, 100),
                PriorRuntimeAbsent
            ),
            Err(Refusal::Exhausted)
        );
        assert_eq!(issuer.serial, u64::MAX);
        let mut issuer = LaunchIssuer::new(TEST_INSTANCE);
        for clock in [(0, 0), (0, 120_001), (u64::MAX, 1)] {
            assert!(
                issuer
                    .reserve(
                        binding(),
                        LaunchAttempt(1),
                        (0, 0),
                        clock,
                        PriorRuntimeAbsent
                    )
                    .is_err()
            );
            assert_eq!(issuer.serial, 0);
        }
        assert_eq!(
            issuer
                .reserve(
                    binding(),
                    LaunchAttempt(1),
                    (0, 0),
                    (0, 100),
                    PriorRuntimeAbsent
                )
                .unwrap()
                .lifetime
                .serial,
            1
        );
    }
}
