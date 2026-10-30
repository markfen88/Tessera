//! Single-operation runner through the existing daemon authority. Authority admission, ownership and deduplication belong to the
//! actor; the exchange below supplies no such permission.
use tessera_control::arc::{Legacy, Source};
use tessera_control::receiver::{Operation, Outcome, Progress, Selection};
use tessera_control_types::{CommandError, ReceiverSelection};

pub(crate) mod accounting;
pub(crate) mod actor;
pub(crate) mod carriers;
pub(crate) mod live;
pub(crate) mod recovery;
mod source_effects;
mod worker;

#[derive(Debug, Clone)]
pub(crate) struct Expected {
    pub(crate) rx: u16,
    pub(crate) source: Option<ReceiverSelection>,
    pub(crate) raw_status: u32,
}

#[derive(Debug)]
pub(crate) struct Plan {
    pub(crate) profile: Legacy,
    pub(crate) rx_count: u16,
    pub(crate) rx: u16,
    pub(crate) expected: Vec<Expected>,
    pub(crate) desired: Option<ReceiverSelection>,
}

fn source(value: &ReceiverSelection) -> Source<'_> {
    Source {
        device: &value.tx_device,
        channel: &value.tx_channel,
    }
}

pub(crate) trait Transport {
    fn sequence(&mut self) -> u16;
    /// Exactly one request; an implementation must never resend.
    async fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, CommandError>;
}

pub(crate) trait Authority {
    /// Actor revalidation immediately before allowing mutation bytes to be
    /// emitted. It must durably consume this boot's expectation and mark the
    /// operation uncertain before returning permission. Cancellation afterward
    /// cannot reopen that permission.
    async fn dispatch(&mut self) -> Result<(), CommandError>;
}

#[derive(Debug, PartialEq)]
pub(crate) enum RunResult {
    /// Supervisor observed worker termination, after its socket was dropped.
    Stopped,
    /// Admission or revalidation failed, before mutation emission.
    Refused(CommandError),
    /// Protocol observations only. NamesMatch does not assert media or audio.
    Observed(Outcome),
}

pub(crate) async fn drive(
    plan: &Plan,
    transport: &mut impl Transport,
    authority: &mut impl Authority,
    deadline: std::time::Instant,
) -> RunResult {
    // Bound borrowed row metadata and retained name text before constructing
    // any exchange. This is an internal plan limit, not an RX inventory cap.
    let names = plan
        .expected
        .iter()
        .filter_map(|e| e.source.as_ref())
        .chain(plan.desired.iter())
        .try_fold(0usize, |n, s| {
            n.checked_add(s.tx_channel.len())?
                .checked_add(s.tx_device.len())
        });
    if plan.expected.len() > 255 || names.is_none_or(|n| n > 1024 * 1024) {
        return RunResult::Refused(crate::runtime_host::unavailable(
            "operation plan exceeds budget",
        ));
    }
    let expected: Vec<_> = plan
        .expected
        .iter()
        .map(|e| Selection {
            rx: e.rx,
            source: e.source.as_ref().map(source),
            raw_status: Some(e.raw_status),
        })
        .collect();
    let mut operation = match Operation::new(
        plan.profile,
        plan.rx_count,
        plan.rx,
        &expected,
        plan.desired.as_ref().map(source),
    ) {
        Ok(operation) => operation,
        Err(error) => return RunResult::Observed(Outcome::Refused(error)),
    };
    // Request buffers hold one mutation's two wire names, not receive pages.
    let mut bytes = vec![0; 65535];
    loop {
        if std::time::Instant::now() >= deadline {
            return RunResult::Observed(operation.interrupt());
        }
        match operation.progress() {
            Progress::Finished(outcome) => return RunResult::Observed(outcome),
            Progress::WriteReady => {
                match tokio::time::timeout_at(deadline.into(), authority.dispatch()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => return RunResult::Refused(error),
                    Err(_) => return RunResult::Refused(CommandError::ReceiverReadTimeout),
                }
            }
            Progress::ReadReady => {}
            Progress::AwaitingReply => return RunResult::Observed(operation.interrupt()),
        }
        if std::time::Instant::now() >= deadline {
            return RunResult::Observed(operation.interrupt());
        }
        let request = match operation.request(transport.sequence(), &mut bytes) {
            Ok(request) => request,
            Err(_) => return RunResult::Observed(operation.interrupt()),
        };
        let reply =
            match tokio::time::timeout_at(deadline.into(), transport.exchange(request)).await {
                Ok(Ok(reply)) => reply,
                Ok(Err(_)) | Err(_) => return RunResult::Observed(operation.interrupt()),
            };
        if operation.receive(&reply).is_err() {
            // receive() has already recorded Refused or Unknown.
            if let Progress::Finished(outcome) = operation.progress() {
                return RunResult::Observed(outcome);
            }
            return RunResult::Observed(operation.interrupt());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn plan(old: Option<&str>, desired: Option<&str>) -> Plan {
        Plan {
            profile: Legacy::from_advertised_version("2.7.41").unwrap(),
            rx_count: 2,
            rx: 2,
            expected: vec![
                Expected {
                    rx: 1,
                    source: Some(ReceiverSelection::new("Laptop", "A")),
                    raw_status: 8,
                },
                Expected {
                    rx: 2,
                    source: old.map(|s| ReceiverSelection::new("Laptop", s)),
                    raw_status: 8,
                },
            ],
            desired: desired.map(|s| ReceiverSelection::new("Laptop", s)),
        }
    }

    fn page(sequence: u16, rx1: Option<&str>, rx2: Option<&str>) -> Vec<u8> {
        let mut bytes = vec![0u8; 52];
        bytes[10] = 8;
        bytes[11] = 2;
        for (index, selected) in [rx1, rx2].into_iter().enumerate() {
            let at = 12 + index * 20;
            bytes[at..at + 2].copy_from_slice(&(index as u16 + 1).to_be_bytes());
            bytes[at + 12..at + 16].copy_from_slice(&8u32.to_be_bytes());
            if let Some(selected) = selected {
                for (field, name) in [(6, selected), (8, "Laptop")] {
                    let offset = bytes.len() as u16;
                    bytes[at + field..at + field + 2].copy_from_slice(&offset.to_be_bytes());
                    bytes.extend_from_slice(name.as_bytes());
                    bytes.push(0);
                }
            }
        }
        let length = bytes.len() as u16;
        for (index, word) in [0x2729, length, sequence, 0x3000, 1]
            .into_iter()
            .enumerate()
        {
            bytes[index * 2..index * 2 + 2].copy_from_slice(&word.to_be_bytes());
        }
        bytes
    }

    fn ack() -> Vec<u8> {
        vec![0x27, 0x29, 0, 10, 0, 2, 0x30, 0x10, 0, 1]
    }

    #[derive(Default)]
    struct Fake {
        replies: VecDeque<std::result::Result<Vec<u8>, CommandError>>,
        requests: Vec<Vec<u8>>,
        authorized: bool,
        dispatches: usize,
        stall_at: usize,
    }
    impl Transport for Fake {
        fn sequence(&mut self) -> u16 {
            self.requests.len() as u16 + 1
        }
        async fn exchange(&mut self, request: &[u8]) -> std::result::Result<Vec<u8>, CommandError> {
            self.requests.push(request.to_vec());
            if self.requests.len() == self.stall_at {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
            self.replies
                .pop_front()
                .unwrap_or(Err(CommandError::ReceiverReadTimeout))
        }
    }
    impl Authority for Fake {
        async fn dispatch(&mut self) -> std::result::Result<(), CommandError> {
            self.dispatches += 1;
            if self.authorized {
                Ok(())
            } else {
                Err(CommandError::locked("flows"))
            }
        }
    }

    async fn run(
        plan: &Plan,
        transport: &mut impl Transport,
        authority: &mut impl Authority,
    ) -> RunResult {
        drive(
            plan,
            transport,
            authority,
            std::time::Instant::now() + std::time::Duration::from_secs(3),
        )
        .await
    }

    #[tokio::test]
    async fn connect_occupied_replace_and_disconnect_use_one_mutation_and_check_the_witness() {
        for (old, desired) in [(None, Some("A")), (Some("A"), Some("B")), (Some("B"), None)] {
            let mut transport = Fake {
                replies: [page(1, Some("A"), old), ack(), page(3, Some("A"), desired)]
                    .into_iter()
                    .map(Ok)
                    .collect(),
                ..Fake::default()
            };
            let mut authority = Fake {
                authorized: true,
                ..Fake::default()
            };
            assert_eq!(
                run(&plan(old, desired), &mut transport, &mut authority).await,
                RunResult::Observed(Outcome::NamesMatch)
            );
            assert_eq!(authority.dispatches, 1);
            assert_eq!(
                transport
                    .requests
                    .iter()
                    .map(|r| u16::from_be_bytes([r[6], r[7]]))
                    .collect::<Vec<_>>(),
                [0x3000, 0x3010, 0x3000]
            );
        }
    }

    #[tokio::test]
    async fn stale_selection_and_late_actor_lock_both_stop_before_mutation() {
        for stale in [true, false] {
            let mut transport = Fake {
                replies: [Ok(page(
                    1,
                    Some("A"),
                    if stale { Some("B") } else { Some("A") },
                ))]
                .into(),
                ..Fake::default()
            };
            let mut authority = Fake::default();
            let result = run(&plan(Some("A"), Some("B")), &mut transport, &mut authority).await;
            assert!(matches!(
                result,
                RunResult::Refused(_) | RunResult::Observed(Outcome::Refused(_))
            ));
            assert_eq!(transport.requests.len(), 1);
            assert_eq!(authority.dispatches, usize::from(!stale));
        }
    }

    #[tokio::test]
    async fn send_uncertainty_and_after_read_disagreement_never_retry_or_restore() {
        for replies in [
            vec![
                Ok(page(1, Some("A"), Some("A"))),
                Err(CommandError::ReceiverReadTimeout),
            ],
            vec![
                Ok(page(1, Some("A"), Some("A"))),
                Ok(ack()),
                Ok(page(3, Some("B"), Some("B"))),
            ],
        ] {
            let mut transport = Fake {
                replies: replies.into(),
                ..Fake::default()
            };
            let mut authority = Fake {
                authorized: true,
                ..Fake::default()
            };
            assert!(matches!(
                run(&plan(Some("A"), Some("B")), &mut transport, &mut authority).await,
                RunResult::Observed(Outcome::Unknown(_))
            ));
            assert_eq!(
                transport
                    .requests
                    .iter()
                    .filter(|r| r[6..8] == [0x30, 0x10])
                    .count(),
                1
            );
            assert_eq!(authority.dispatches, 1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_during_before_read_refuses_and_during_mutation_is_unknown() {
        for stall_at in [1, 2] {
            let mut transport = Fake {
                stall_at,
                replies: [Ok(page(1, Some("A"), Some("A")))].into(),
                ..Fake::default()
            };
            let mut authority = Fake {
                authorized: true,
                ..Fake::default()
            };
            let result = run(&plan(Some("A"), Some("B")), &mut transport, &mut authority).await;
            assert!(if stall_at == 1 {
                matches!(result, RunResult::Observed(Outcome::Refused(_)))
            } else {
                matches!(result, RunResult::Observed(Outcome::Unknown(_)))
            });
            assert_eq!(transport.requests.len(), stall_at);
            assert_eq!(authority.dispatches, stall_at - 1);
        }
    }

    #[tokio::test]
    async fn invalid_plan_or_expired_start_opens_no_exchange_and_requests_no_permission() {
        for expired in [false, true] {
            let mut plan = plan(Some("A"), Some("B"));
            if !expired {
                plan.rx = 0;
            }
            let mut transport = Fake::default();
            let mut authority = Fake {
                authorized: true,
                ..Fake::default()
            };
            let result = drive(
                &plan,
                &mut transport,
                &mut authority,
                std::time::Instant::now(),
            )
            .await;
            assert!(matches!(result, RunResult::Observed(Outcome::Refused(_))));
            assert!(transport.requests.is_empty());
            assert_eq!(authority.dispatches, 0);
        }
    }
}
