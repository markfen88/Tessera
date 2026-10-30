//! Plain-HTML selection of names for the existing Subscribe command.
//! No route store, carrier mutation or protocol control lives in the portal.
use super::*;
use axum::extract::Query as Params;
use tessera_control_types::{
    EndpointRef, RoutingChannel, RoutingChannelPage, RoutingChannelsQuery, RoutingCursor,
    RoutingDevice, RoutingDevicePage, RoutingDevicesQuery, RoutingDiscoveryState,
    RoutingUnavailableReason as Reason,
};

#[derive(Debug, Default, Deserialize)]
pub struct Selection {
    rx_channel: u16,
    device_cursor: Option<String>,
    device_snapshot: Option<String>,
    source: Option<String>,
    channel_cursor: Option<String>,
    tx_id: Option<u32>,
    csrf: Option<String>,
    previous: Option<String>,
}

struct PickError(StatusCode, String);
impl From<ClientError> for PickError {
    fn from(error: ClientError) -> Self {
        let code = match &error {
            ClientError::Command(CommandError::RoutingSnapshotExpired) => StatusCode::CONFLICT,
            ClientError::Command(CommandError::PermissionDenied) => StatusCode::FORBIDDEN,
            ClientError::Command(CommandError::RateLimited) => StatusCode::TOO_MANY_REQUESTS,
            _ => StatusCode::BAD_GATEWAY,
        };
        Self(code, explain(&error))
    }
}
fn stale() -> PickError {
    PickError(
        StatusCode::CONFLICT,
        "The selection changed or expired. Choose the source again.".into(),
    )
}
fn blocked(message: &str) -> PickError {
    PickError(StatusCode::CONFLICT, message.into())
}

async fn receiver(st: &Shared, key: &str, rx: u16) -> Result<DeviceStatus, PickError> {
    let status = status_of(st)
        .await
        .map_err(|e| PickError(StatusCode::BAD_GATEWAY, e))?;
    let device = status
        .devices
        .into_iter()
        .find(|d| d.key.0 == key)
        .ok_or_else(|| {
            PickError(
                StatusCode::NOT_FOUND,
                "Receiving device is unavailable.".into(),
            )
        })?;
    if device
        .routing_locks
        .as_ref()
        .is_some_and(|locks| locks.iter().any(|l| l == "subscriptions"))
    {
        return Err(PickError(
            StatusCode::FORBIDDEN,
            "Saved subscription intent is locked by this host.".into(),
        ));
    }
    if !device.rx_channels.iter().any(|c| c.channel.get() == rx) {
        return Err(stale());
    }
    Ok(device)
}

async fn device_page(st: &Shared, f: &Selection) -> Result<RoutingDevicePage, PickError> {
    match call(
        st,
        RequestBody::Query(Query::RoutingDevices(RoutingDevicesQuery::new(
            f.device_cursor
                .clone()
                .filter(|v| !v.is_empty())
                .map(RoutingCursor),
        ))),
    )
    .await?
    {
        Reply::RoutingDevices(page) => Ok(page),
        _ => Err(PickError(
            StatusCode::BAD_GATEWAY,
            "Device inventory is unavailable.".into(),
        )),
    }
}

fn selected_device(page: &RoutingDevicePage, f: &Selection) -> Result<RoutingDevice, PickError> {
    if f.device_snapshot.as_deref() != Some(&page.snapshot) {
        return Err(stale());
    }
    let reference: EndpointRef =
        serde_json::from_str(f.source.as_deref().unwrap_or("")).map_err(|_| stale())?;
    let device = page
        .devices
        .iter()
        .find(|d| d.endpoint == reference)
        .cloned()
        .ok_or_else(stale)?;
    if device.unavailable.contains(&Reason::ConflictingMetadata) {
        return Err(blocked(
            "This device name or its metadata is ambiguous. Names alone cannot identify this source.",
        ));
    }
    if device.unavailable.contains(&Reason::InterfaceNotPermitted) {
        return Err(blocked(
            "This source is outside the permitted discovery interfaces.",
        ));
    }
    Ok(device)
}

async fn channel_page(
    st: &Shared,
    device: &RoutingDevice,
    f: &Selection,
) -> Result<RoutingChannelPage, PickError> {
    match call(
        st,
        RequestBody::Query(Query::RoutingChannels(RoutingChannelsQuery::new(
            device.endpoint.clone(),
            Direction::Tx,
            f.channel_cursor
                .clone()
                .filter(|v| !v.is_empty())
                .map(RoutingCursor),
        ))),
    )
    .await?
    {
        Reply::RoutingChannels(page) => Ok(page),
        _ => Err(PickError(
            StatusCode::BAD_GATEWAY,
            "Output inventory is unavailable.".into(),
        )),
    }
}

fn selected_channel(
    page: &RoutingChannelPage,
    device: &RoutingDevice,
    id: u32,
) -> Result<RoutingChannel, PickError> {
    let channel = page
        .channels
        .iter()
        .find(|c| c.id == id && c.direction == Direction::Tx && c.endpoint == device.endpoint)
        .cloned()
        .ok_or_else(stale)?;
    if channel.unavailable.contains(&Reason::ConflictingMetadata) {
        return Err(blocked(
            "This output has conflicting names or metadata. Choose an unambiguous source.",
        ));
    }
    if channel.label.trim().is_empty()
        || channel.label != channel.label.trim()
        || device.label.trim().is_empty()
        || device.label != device.label.trim()
    {
        return Err(blocked(
            "These advertised names cannot be represented by the existing subscription command.",
        ));
    }
    Ok(channel)
}

fn previous(device: &DeviceStatus, rx: u16) -> String {
    let names = device
        .subscriptions
        .iter()
        .find(|s| s.rx_channel.get() == rx)
        .map(|s| (&s.tx_device, &s.tx_channel));
    serde_json::to_string(&names).expect("names serialize")
}

fn fields(f: &Selection, include_source: bool) -> Markup {
    html! {
        input type="hidden" name="rx_channel" value=(f.rx_channel);
        @if let Some(cursor) = &f.device_cursor { input type="hidden" name="device_cursor" value=(cursor); }
        @if let Some(snapshot) = &f.device_snapshot { input type="hidden" name="device_snapshot" value=(snapshot); }
        @if include_source {
            @if let Some(source) = &f.source { input type="hidden" name="source" value=(source); }
        }
    }
}

fn discovery_note(page: &RoutingDevicePage) -> Markup {
    html! {
        p.muted { @match page.discovery.state {
            RoutingDiscoveryState::Disabled => { "Network browsing is off. Local devices are listed; enable browse_devices to discover network sources, or use advanced name entry." },
            RoutingDiscoveryState::Failed => { "Network discovery is unavailable. This list may be incomplete." },
            RoutingDiscoveryState::Starting => { "Network discovery is reconciling. This list may be incomplete." },
            _ => { "Showing available observations, not a guarantee that every network device has been found." },
        } }
        @if !page.complete { p.muted { "The available discovery cache is incomplete." } }
    }
}

fn failure(
    st: &Shared,
    token: &str,
    s: &Session,
    headers: &HeaderMap,
    key: &str,
    rx: u16,
    error: PickError,
) -> Response {
    let body = html! { section.card {
        h2 { "Choose source again" }
        (error_box(&error.1))
        form method="get" action=(dev_path(key, "mapping")) {
            input type="hidden" name="rx_channel" value=(rx);
            button.btn type="submit" { "Refresh source choices" }
        }
        a href="/devices" { "Back to devices and advanced name entry" }
    } };
    let mut response = page(
        st,
        token,
        s,
        headers,
        view("Subscription intent", "/devices"),
        &body,
    );
    *response.status_mut() = error.0;
    response
}

pub async fn picker(
    State(st): State<Shared>,
    headers: HeaderMap,
    Path(key): Path<String>,
    Params(mut f): Params<Selection>,
) -> Response {
    let (token, s) = match need(&st, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let result = picker_body(&st, &key, &s, &mut f).await;
    match result {
        Ok(body) => page(
            &st,
            &token,
            &s,
            &headers,
            view("Subscription intent", "/devices"),
            &body,
        ),
        Err(error) => failure(&st, &token, &s, &headers, &key, f.rx_channel, error),
    }
}

async fn picker_body(
    st: &Shared,
    key: &str,
    session: &Session,
    f: &mut Selection,
) -> Result<Markup, PickError> {
    let target = receiver(st, key, f.rx_channel).await?;
    let devices = device_page(st, f).await?;
    let path = dev_path(key, "mapping");
    let heading = html! {
        h2 { "Saved subscription intent" }
        p { "Receiving input: " bdi { (target.name) } " · RX " (f.rx_channel) }
        p.muted { "This saves source names. With the native device role, the daemon connects and disconnects the audio for them. With the compatibility runtime, audio arrives through an existing configured flow or a flow made from a network controller, and saving or removing intent does not connect or disconnect audio." }
    };
    if f.source.is_none() {
        f.device_snapshot = Some(devices.snapshot.clone());
        return Ok(html! { section.card {
            (heading)
            h3 { "1. Choose transmitting device (TX)" }
            (discovery_note(&devices))
            form.fields method="get" action=(path) {
                (fields(f, false))
                label { "Source device"
                    select name="source" required {
                        option value="" disabled selected { "Choose a device…" }
                        @for device in &devices.devices {
                            option dir="auto" value=(serde_json::to_string(&device.endpoint).expect("reference serializes"))
                                disabled[device.unavailable.contains(&Reason::ConflictingMetadata) || device.unavailable.contains(&Reason::InterfaceNotPermitted) || device.tx_channels == Some(0)] {
                                (device.label) " · TX " (device.tx_channels.map_or("advertised outputs".into(), |n| n.to_string()))
                                @if let Some(interface) = &device.interface { " · " (interface) }
                                @if device.unavailable.contains(&Reason::ConflictingMetadata) { " · ambiguous name or metadata" }
                                @if device.unavailable.contains(&Reason::InterfaceNotPermitted) { " · interface not permitted" }
                            }
                        }
                    }
                }
                button.btn.primary type="submit" { "Choose output channel" }
            }
            @if let Some(cursor) = &devices.next_cursor {
                form method="get" action=(path) {
                    input type="hidden" name="rx_channel" value=(f.rx_channel);
                    input type="hidden" name="device_cursor" value=(&cursor.0);
                    button.btn type="submit" { "More devices" }
                }
            }
            a href="/devices" { "Back to devices and advanced name entry" }
        } });
    }
    let source = selected_device(&devices, f)?;
    let channels = channel_page(st, &source, f).await?;
    if let Some(id) = f.tx_id {
        let channel = selected_channel(&channels, &source, id)?;
        return Ok(html! { section.card {
            (heading)
            h3 { "3. Review saved intent" }
            p { "Source: " bdi { (source.label) } " · TX " (channel.id) " · " bdi { (channel.label) }
                " → " bdi { (target.name) } " · RX " (f.rx_channel) }
            @if let Some(old) = target.subscriptions.iter().find(|s| s.rx_channel.get() == f.rx_channel) {
                p { "Previous saved intent: " bdi { (old.tx_channel) } " @ " bdi { (old.tx_device) } }
            } @else { p { "No previous saved intent." } }
            p.muted { "The saved mapping contains names, not an interface-specific device identity. Existing carrier ownership and packet observations are shown on the device page; packet activity does not verify this source." }
            form method="post" action=(path) {
                (fields(f, true))
                @if let Some(cursor) = &f.channel_cursor { input type="hidden" name="channel_cursor" value=(cursor); }
                input type="hidden" name="tx_id" value=(id);
                input type="hidden" name="csrf" value=(session.csrf);
                input type="hidden" name="previous" value=(previous(&target, f.rx_channel));
                button.btn.primary type="submit" {
                    @if target.subscriptions.iter().any(|s| s.rx_channel.get() == f.rx_channel) { "Replace saved intent" }
                    @else { "Save subscription intent" }
                }
            }
            a href="/devices" { "Cancel" }
        } });
    }
    Ok(html! { section.card {
        (heading)
        h3 { "2. Choose output channel (TX)" }
        p { "Transmitting device: " bdi { (source.label) } }
        @if source.unavailable.contains(&Reason::InterfaceUnknown) {
            p.muted { "The discovery backend cannot identify the interface. Only source names will be saved." }
        }
        @if channels.channels.is_empty() { p { "No output channels are available in this observation. Try refreshing or use advanced name entry." } }
        form.fields method="get" action=(path) {
            (fields(f, true))
            @if let Some(cursor) = &f.channel_cursor { input type="hidden" name="channel_cursor" value=(cursor); }
            label { "Source output"
                select name="tx_id" required {
                    option value="" disabled selected { "Choose an output…" }
                    @for channel in &channels.channels {
                        option dir="auto" value=(channel.id) disabled[channel.unavailable.contains(&Reason::ConflictingMetadata)] {
                            "TX " (channel.id) " · " (channel.label)
                            @if channel.unavailable.contains(&Reason::ConflictingMetadata) { " · conflicting metadata" }
                        }
                    }
                }
            }
            button.btn.primary type="submit" disabled[channels.channels.is_empty()] { "Review saved intent" }
        }
        @if let Some(cursor) = &channels.next_cursor {
            form method="get" action=(path) {
                (fields(f, true))
                input type="hidden" name="channel_cursor" value=(&cursor.0);
                button.btn type="submit" { "More outputs" }
            }
        }
        a href="/devices" { "Back to devices" }
    } })
}

pub async fn save(
    State(st): State<Shared>,
    headers: HeaderMap,
    Path(key): Path<String>,
    Form(f): Form<Selection>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, f.csrf.as_deref().unwrap_or("")) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let selection = &f;
    let result = async {
        let target = receiver(&st, &key, selection.rx_channel).await?;
        if Some(previous(&target, selection.rx_channel).as_str()) != f.previous.as_deref() {
            return Err(stale());
        }
        let devices = device_page(&st, selection).await?;
        let source = selected_device(&devices, selection)?;
        let channels = channel_page(&st, &source, selection).await?;
        let channel = selected_channel(&channels, &source, selection.tx_id.ok_or_else(stale)?)?;
        Ok(Command::Subscribe {
            device: DeviceKey(key.clone()),
            rx_channel: Channel::new(selection.rx_channel).ok_or_else(stale)?,
            tx_device: source.label,
            tx_channel: channel.label,
        })
    }
    .await;
    let command = match result {
        Ok(c) => c,
        Err(error) => return failure(&st, &token, &s, &headers, &key, selection.rx_channel, error),
    };
    let outcome = drive(&st, &s, vec![command]).await;
    settle(
        &st,
        &token,
        &s,
        &headers,
        "/devices",
        |_| {
            "Subscription saved. With the native device role the receiver connects it; with the compatibility runtime, existing flows are unchanged.".into()
        },
        outcome,
    )
}
