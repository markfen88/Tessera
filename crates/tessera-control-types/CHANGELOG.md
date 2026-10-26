# Changelog

## 0.3.0

The 0.2.2 release (published from the public branch) had no wire changes; everything
below is new in 0.3.0.

Additive wire changes (envelope version remains 1): `FlowSpec`,
`Command::SetProtocolFlows`, the admin-only `RequestBody::ProtocolCommand`,
`Event::ProtocolFlowsChanged`, `LockedValueRefused`, `RuntimeValueChanged`,
`SubscriptionChangedWithOrigin` and `ConfigReloadedWithOrigin`. Existing variants retain their shapes.
The origin-bearing companions accompany the legacy subscription and reload events.
Runtime change events precede the debounced state write, as specified by ADR 0008.
See [the control interface](../../docs/control-interface.md).
