//! Authority-binding partition of a complete global observation. A missing pin
//! never proves independence, and observations cannot select their own domain.
use super::guards::Binding;
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Partition {
    InDomain,
    OtherDomain,
    Unresolved,
}

pub(super) fn partition(binding: &Binding, flow: &FlowSpec) -> Partition {
    if flow.device.0 != binding.device {
        return Partition::OtherDomain;
    }
    let (Some(interface), Some(local)) = (
        flow.interface.as_deref().filter(|s| !s.is_empty()),
        address(flow.local_address.as_deref()),
    ) else {
        return Partition::Unresolved;
    };
    if local.is_unspecified() || local.is_multicast() || local.is_broadcast() {
        return Partition::Unresolved;
    }
    if interface == binding.interface && local == binding.address {
        Partition::InDomain
    } else {
        Partition::OtherDomain
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_control_types::DeviceKey;

    #[test]
    fn exact_pins_other_devices_and_unresolved_pins_are_distinct() {
        let binding = Binding {
            device: "main".into(),
            interface: "audio0".into(),
            address: Ipv4Addr::new(192, 0, 2, 1),
        };
        let mut flow = FlowSpec::new("flow", DeviceKey("main".into()), Direction::Rx, 5001);
        assert_eq!(partition(&binding, &flow), Partition::Unresolved);
        flow.interface = Some("audio0".into());
        flow.local_address = Some("192.0.2.1".into());
        assert_eq!(partition(&binding, &flow), Partition::InDomain);
        flow.interface = Some("audio1".into());
        assert_eq!(partition(&binding, &flow), Partition::OtherDomain);
        flow.local_address = Some("invalid".into());
        assert_eq!(partition(&binding, &flow), Partition::Unresolved);
        flow.device = DeviceKey("other".into());
        assert_eq!(partition(&binding, &flow), Partition::OtherDomain);
    }
}
