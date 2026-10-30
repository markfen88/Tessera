//! Receiver observations through the same typed API as other portal clients.
use super::*;
use axum::extract::Query as Params;
use tessera_control_types::{
    EndpointRef, ReceiverChange, ReceiverOperationState, RoutingChannelsQuery, RoutingCursor,
    RoutingDevicesQuery, RoutingReceiverChannel, RoutingReceiverPage, RoutingReceiverQuery,
    RoutingSourceRef,
};

mod board;
mod recovery;
pub(crate) use recovery::inspect;
pub(crate) use recovery::inspect_carriers;

#[derive(Default, Deserialize)]
pub(crate) struct Selection {
    endpoint: Option<String>,
    first: Option<u16>,
    cursor: Option<String>,
    source_page: Option<usize>,
}

pub(crate) async fn receivers(
    State(st): State<Shared>,
    headers: HeaderMap,
    params: Result<Params<Selection>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let (token, session) = match need(&st, &headers) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let selection = match params {
        Ok(Params(p)) => p,
        Err(_) => return (StatusCode::BAD_REQUEST, "Invalid receiver selection").into_response(),
    };
    if selection.endpoint.is_none() {
        return board::show(&st, &token, &session, &headers, selection).await;
    }
    let query = if let Some(endpoint) = &selection.endpoint {
        let endpoint: EndpointRef = match serde_json::from_str(endpoint) {
            Ok(endpoint) => endpoint,
            Err(_) => {
                return (StatusCode::BAD_REQUEST, "Invalid receiver selection").into_response();
            }
        };
        Query::RoutingReceiver(RoutingReceiverQuery::new(
            endpoint,
            selection.first.unwrap_or(1),
        ))
    } else {
        Query::RoutingDevices(RoutingDevicesQuery::new(
            selection.cursor.map(RoutingCursor),
        ))
    };
    let result = call(&st, RequestBody::Query(query)).await;
    let sources = if matches!(&result, Ok(Reply::RoutingReceiver(_))) {
        catalogue(&st).await
    } else {
        Vec::new()
    };
    let body = html! { section.card {
        h2 { "Audio routing" }
        p.muted { "Choose a receiving device, then connect a source to an input. RX is network to application; TX is application to network." }
        @match result {
            Ok(Reply::RoutingDevices(devices)) => {
                p { "Choose a receiving device." }
                @if !devices.complete { p.muted { "Discovery is incomplete. Some devices may be missing." } }
                @if devices.devices.is_empty() { p { "No devices are available." } }
                @for device in devices.devices {
                    form method="get" action="/routing" {
                        input type="hidden" name="endpoint" value=(serde_json::to_string(&device.endpoint).expect("reference serializes"));
                        button.btn type="submit" { (device.label) }
                    }
                }
                @if let Some(cursor) = devices.next_cursor {
                    form method="get" action="/routing" {
                        input type="hidden" name="cursor" value=(cursor.0);
                        button.btn type="submit" { "More devices" }
                    }
                }
            },
            Ok(Reply::RoutingReceiver(receiver)) => {
                h3 { (receiver.label) }
                @if receiver.channels.is_empty() {
                    p { "The receiver returned no channel observations. Its channel inventory and selections remain unknown." }
                } @else {
                    @if let Some(reason) = &receiver.operation_unavailable { (error_box(reason)) }
                    (matrix(&receiver, &sources))
                }
                p.muted { "Audio status is unverified. Each page is a separate observation. Selections can change between reads." }
                @if let Some(first) = receiver.next_first {
                    form method="get" action="/routing" {
                        input type="hidden" name="endpoint" value=(serde_json::to_string(&receiver.endpoint).expect("reference serializes"));
                        input type="hidden" name="first" value=(first);
                        button.btn type="submit" { "More inputs" }
                    }
                }
                form method="get" action="/routing" {
                    input type="hidden" name="endpoint" value=(serde_json::to_string(&receiver.endpoint).expect("reference serializes"));
                    input type="hidden" name="first" value=(receiver.first);
                    button.btn type="submit" { "Refresh selections" }
                }
                p { a href="/routing" { "Choose another receiver" } }
            },
            Ok(_) => { (error_box("Receiver observations are unavailable.")) },
            Err(error) => {
                (error_box(&explain(&error)))
                p { a href="/routing" { "Refresh device choices" } }
            },
        }
    } };
    page(
        &st,
        &token,
        &session,
        &headers,
        view("Routing", "/routing").every(10),
        &body,
    )
}

struct SourceChoice {
    reference: RoutingSourceRef,
    device: String,
    channel: String,
    aliases: Vec<String>,
    ambiguous: bool,
}

// Bounded UI catalogue. Daemon references, ambiguity, format, host qualification
// and locks are revalidated by the authority; UI visibility never grants rights.
async fn catalogue(st: &Shared) -> Vec<SourceChoice> {
    let mut choices = Vec::new();
    let mut cursor = None;
    for _ in 0..8 {
        let Ok(Reply::RoutingDevices(page)) = call(
            st,
            RequestBody::Query(Query::RoutingDevices(RoutingDevicesQuery::new(cursor))),
        )
        .await
        else {
            break;
        };
        for device in page.devices {
            let mut channel_cursor = None;
            for _ in 0..8 {
                let Ok(Reply::RoutingChannels(channels)) = call(
                    st,
                    RequestBody::Query(Query::RoutingChannels(RoutingChannelsQuery::new(
                        device.endpoint.clone(),
                        Direction::Tx,
                        channel_cursor,
                    ))),
                )
                .await
                else {
                    break;
                };
                for channel in channels.channels {
                    if choices.len() >= 256 {
                        return unambiguous_choices(choices);
                    }
                    choices.push(SourceChoice {
                        reference: RoutingSourceRef::new(device.endpoint.clone(), channel.id),
                        device: device.label.clone(),
                        channel: channel.label,
                        aliases: channel.aliases,
                        ambiguous: channel.unavailable.contains(
                            &tessera_control_types::RoutingUnavailableReason::ConflictingMetadata,
                        ),
                    });
                }
                channel_cursor = channels.next_cursor;
                if channel_cursor.is_none() {
                    break;
                }
            }
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    unambiguous_choices(choices)
}

fn unambiguous_choices(mut choices: Vec<SourceChoice>) -> Vec<SourceChoice> {
    let mut names = std::collections::BTreeMap::new();
    for source in &choices {
        let mut aliases = source.aliases.clone();
        aliases.push(source.channel.clone());
        aliases.sort();
        aliases.dedup();
        for name in aliases {
            *names.entry((source.device.clone(), name)).or_insert(0usize) += 1;
        }
    }
    for source in &mut choices {
        source.ambiguous |= std::iter::once(&source.channel)
            .chain(source.aliases.iter())
            .any(|name| {
                names
                    .get(&(source.device.clone(), name.clone()))
                    .copied()
                    .unwrap_or(0)
                    > 1
            });
    }
    choices
}

fn selected(row: &RoutingReceiverChannel) -> String {
    row.source.as_ref().map_or_else(
        || "Receiver reports no source".into(),
        |s| format!("{} → {}", s.tx_device, s.tx_channel),
    )
}

fn change(row: &RoutingReceiverChannel, source: Option<&SourceChoice>) -> Option<ReceiverChange> {
    match (&row.source, source) {
        (None, Some(source)) => Some(ReceiverChange::Connect {
            source: source.reference.clone(),
        }),
        (Some(previous), Some(source)) if !source.matches(previous) => {
            Some(ReceiverChange::Replace {
                previous: previous.clone(),
                source: source.reference.clone(),
            })
        }
        (Some(previous), None) => Some(ReceiverChange::Disconnect {
            previous: previous.clone(),
        }),
        _ => None,
    }
}

fn choice(
    receiver: &RoutingReceiverPage,
    row: &RoutingReceiverChannel,
    source: Option<&SourceChoice>,
    compact: bool,
) -> Markup {
    let active = source.is_some_and(|s| row.source.as_ref().is_some_and(|p| s.matches(p)));
    let change = if active {
        change(row, None)
    } else {
        change(row, source)
    };
    let text = match (compact, active, source) {
        (true, true, _) => "● Disconnect".into(),
        (true, false, _) => {
            if row.source.is_some() {
                "↔ Replace".into()
            } else {
                "+ Connect".into()
            }
        }
        (false, true, Some(s)) => format!(
            "Disconnect {} / {} from input {}",
            s.device, s.channel, row.id
        ),
        (false, false, Some(s)) => format!(
            "{} {} → {} / {}",
            if row.source.is_some() {
                "Replace with"
            } else {
                "Connect"
            },
            s.device,
            s.channel,
            row.id
        ),
        _ => "Disconnect".into(),
    };
    html! {
        @if let Some(change) = change {
            form method="get" action="/routing/review" data-source-search=[source.map(|s| format!("{} {}", s.device, s.channel))] {
                input type="hidden" name="receiver" value=(serde_json::to_string(&receiver.endpoint).unwrap());
                input type="hidden" name="rx" value=(row.id);
                input type="hidden" name="change" value=(serde_json::to_string(&change).unwrap());
                button.btn type="submit" disabled[row.expectation.is_none() || source.is_some_and(|s| !s.available_to(receiver, row))] { (text) }
            }
        }
    }
}

fn matrix(receiver: &RoutingReceiverPage, sources: &[SourceChoice]) -> Markup {
    board::single(receiver, sources)
}

impl SourceChoice {
    fn available_to(&self, receiver: &RoutingReceiverPage, row: &RoutingReceiverChannel) -> bool {
        // The host's own transmitter may feed any other device's receiver;
        // that receiver applies its own locks to the write.
        !self.ambiguous
            && self.reference.endpoint != receiver.endpoint
            && (receiver.fan_out
                || row.source.as_ref().is_some_and(|p| self.matches(p))
                || !self.feeds_another(receiver, row))
    }
    /// A receiver without `fan_out` (its compatibility runtime) cannot carry
    /// one source channel to two of its inputs; its daemon refuses it too.
    /// Disconnecting stays available.
    fn feeds_another(&self, receiver: &RoutingReceiverPage, row: &RoutingReceiverChannel) -> bool {
        receiver
            .channels
            .iter()
            .any(|r| r.id != row.id && r.source.as_ref().is_some_and(|s| self.matches(s)))
    }
    fn matches(&self, selection: &tessera_control_types::ReceiverSelection) -> bool {
        !self.ambiguous
            && selection.tx_device == self.device
            && (selection.tx_channel == self.channel
                || self.aliases.contains(&selection.tx_channel))
    }
}

#[derive(Deserialize)]
pub(crate) struct Review {
    receiver: String,
    rx: u16,
    change: String,
}

pub(crate) async fn review(
    State(st): State<Shared>,
    headers: HeaderMap,
    Params(f): Params<Review>,
) -> Response {
    let (token, session) = match need(&st, &headers) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let preparation: Result<(Command, String, Option<tessera_control_types::ReceiverSelection>), String> = async {
        let endpoint: EndpointRef = serde_json::from_str(&f.receiver).map_err(|_| "Invalid receiver".to_owned())?;
        let change: ReceiverChange = serde_json::from_str(&f.change).map_err(|_| "Invalid change".to_owned())?;
        let mut first = 1;
        let mut found = None;
        for _ in 0..32 {
            let Reply::RoutingReceiver(page) = call(&st, RequestBody::Query(Query::RoutingReceiver(RoutingReceiverQuery::new(endpoint.clone(), first)))).await.map_err(|e| explain(&e))? else { return Err("Receiver unavailable".into()); };
            if let Some(row) = page.channels.iter().find(|c| c.id == f.rx) { found = Some((page.label, row.clone())); break; }
            let Some(next) = page.next_first else { break; };
            if next <= first { break; }
            first = next;
        }
        let (label, row) = found.ok_or_else(|| "Receiver channel unavailable".to_owned())?;
        let expectation = row.expectation.ok_or_else(|| "Routing is unavailable for this receiver. Return to Routing to see its current reason.".to_owned())?;
        let (previous, source) = match &change {
            ReceiverChange::Connect { source } => (None, Some(source)),
            ReceiverChange::Replace { previous, source } => (Some(previous), Some(source)),
            ReceiverChange::Disconnect { previous } => (Some(previous), None),
            _ => return Err("Unsupported change".into()),
        };
        if row.source.as_ref() != previous { return Err("The receiver changed externally. Refresh and choose again.".into()); }
        let description = if let Some(source) = source {
            let sources = catalogue(&st).await;
            let source = sources.iter().find(|s| &s.reference == source).ok_or_else(|| "Source changed; refresh and choose again".to_owned())?;
            format!("{} {} / {} → {} / input {}", if previous.is_some() { "Replace with" } else { "Connect" }, source.device, source.channel, label, f.rx)
        } else { format!("Disconnect {} / input {}", label, f.rx) };
        let previous = previous.cloned();
        Ok((Command::RouteReceiver { expectation, change }, description, previous))
    }.await;
    let body = html! { section.card id="routing-confirmation" { h2 { "Review routing change" }
        @match preparation {
            Ok((command, description, previous)) => {
                p { (description) }
                @if let Some(previous) = previous { p { "Current source: " (previous.tx_device) " / " (previous.tx_channel) } }
                p.muted { "The daemon checks the selection and both routing locks again before sending one command. Receiver confirmation does not verify audio." }
                p.muted { "Allow about 5 seconds for audio to settle. Other inputs on this receiver fed from the same source device may drop briefly during the change." }
                form method="post" action="/routing/apply" {
                    input type="hidden" name="csrf" value=(session.csrf);
                    input type="hidden" name="command" value=(serde_json::to_string(&command).unwrap());
                    input type="hidden" name="receiver" value=(f.receiver);
                    button.btn type="submit" { "Confirm change" }
                }
            },
            Err(error) => { (error_box(&error)) },
        }
        a href="/routing" { "Cancel / return to routing" }
    } };
    page(
        &st,
        &token,
        &session,
        &headers,
        view("Review routing", "/routing"),
        &body,
    )
}

#[derive(Deserialize)]
pub(crate) struct Apply {
    csrf: String,
    command: String,
    #[serde(rename = "receiver", default)]
    _receiver: String,
}

pub(crate) async fn apply(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(f): Form<Apply>,
) -> Response {
    let (token, session) = match need_post(&st, &headers, &f.csrf) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let command: Command = match serde_json::from_str(&f.command) {
        Ok(
            c @ (Command::RouteReceiver { .. }
            | Command::ReconcileReceiverOperation { .. }
            | Command::ReleaseRuntimeCarriers { .. }
            | Command::GrantRuntimeCarriersToNextRun { .. }),
        ) => c,
        _ => return (StatusCode::BAD_REQUEST, "Invalid receiver command").into_response(),
    };
    let carrier_release = matches!(command, Command::ReleaseRuntimeCarriers { .. });
    let next_run_grant = matches!(command, Command::GrantRuntimeCarriersToNextRun { .. });
    let peer_operation = match &command {
        Command::RouteReceiver { expectation, .. } if expectation.0.starts_with("peer-") => {
            Some(expectation.0.clone())
        }
        _ => None,
    };
    let result = call(&st, RequestBody::Command(command)).await;
    let outcome = match &result {
        Ok(Reply::Done) if next_run_grant => "next_runtime_authorized",
        Ok(Reply::Done) if carrier_release => "carrier_management_released",
        Ok(Reply::ReceiverOperation(result))
            if result.state == ReceiverOperationState::ReceiverConfirmed =>
        {
            "receiver_confirmed"
        }
        _ => "unconfirmed",
    };
    let body = html! { section.card id="routing-result" data-routing-outcome=(outcome) { h2 { "Routing result" }
        @match result {
            Ok(Reply::Done) if next_run_grant => {
                p { "The next runtime start is authorized to replace the reviewed first-device carriers. Current routes and saved receiver selections remain unchanged." }
                p { "A runtime restart can interrupt audio. Use the attended service update procedure; the portal has not restarted it. Routing becomes ready only after that new run delivers an accepted complete report." }
                p { a href="/routing/carrier-recovery" { "Inspect next-start authorization" } }
            },
            Ok(Reply::Done) if carrier_release => {
                p { "Reviewed carriers returned to runtime management. Stored routes and receiver selections were not changed." }
                p { "A later complete runtime report may replace those carriers. Routing still requires an accepted report; this does not verify audio." }
                p { a href="/routing/carrier-recovery" { "Inspect carrier ownership again" } }
            },
            Ok(Reply::ReceiverOperation(result)) => {
                p { @match result.state {
                    ReceiverOperationState::ReceiverConfirmed => { "Receiver confirmed the selection. Audio remains unverified." },
                    ReceiverOperationState::Reconciled { original_unknown } => { "Fresh receiver selections accepted. " (original_unknown) " Audio remains unverified." },
                    ReceiverOperationState::Pending => { "Operation pending. No additional command was sent." },
                    ReceiverOperationState::Conflict { reason } => { "Changed externally: " (reason) },
                    ReceiverOperationState::Refused { reason } => { "Refused: " (reason) },
                    ReceiverOperationState::Unknown { reason } => { "Outcome unknown: " (reason) " No retry was sent. Further changes are blocked pending reconciliation." },
                    _ => { "Unrecognized operation state; inspect before continuing." },
                } }
            },
            Ok(_) => { (error_box("Unexpected daemon response. Inspect the receiver before continuing.")) },
            Err(error) => { (error_box(&explain(&error))) },
        }
        p { a.btn href="/routing" { "Return to routing matrix" } }
        @if let Some(operation) = &peer_operation {
            p { a href=(format!("/routing/peer-operation?operation={operation}")) { "Inspect receiver-host operation status" } }
            p.muted { "If recovery is required, open the receiving machine's existing portal. The receiving daemon keeps its own uncertainty and carrier protection." }
        } @else {
            p { a href="/routing/recovery" { "Inspect an uncertain routing operation" } }
        }
    } };
    page(
        &st,
        &token,
        &session,
        &headers,
        view("Routing result", "/routing"),
        &body,
    )
}

pub(crate) async fn peer_operation(
    State(st): State<Shared>,
    headers: HeaderMap,
    Params(params): Params<std::collections::BTreeMap<String, String>>,
) -> Response {
    let (token, session) = match need(&st, &headers) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Some(operation) = params.get("operation") else {
        return (StatusCode::BAD_REQUEST, "Operation is required").into_response();
    };
    let result = st.routing_peers.operation(&st, operation).await;
    let body = html! { section.card {
        h2 { "Receiver-host operation status" }
        @match result {
            Ok(Reply::ReceiverOperation(result)) => {
                p { "Input " (result.rx_channel) }
                pre { (serde_json::to_string_pretty(&result.state).unwrap()) }
            },
            Err(error) => { (error_box(&explain(&error))) },
            _ => { (error_box("Receiver-host accounting is unavailable")) },
        }
        p { "This inspection sends no routing command. A confirmed selection does not verify audio. If the receiver requires reconciliation, use its existing portal's recovery page." }
        form method="post" action="/routing/peer-operation" {
            input type="hidden" name="csrf" value=(session.csrf);
            input type="hidden" name="operation" value=(operation);
            button.btn type="submit" { "Retire unused submission" }
        }
        p.muted { "Retirement consumes an unused expectation so a delayed request cannot execute. It sends no routing mutation and cannot cancel a dispatched operation or clear receiver uncertainty." }
        p { a href=(format!("/routing/peer-operation?operation={operation}")) { "Read status again" } " · " a href="/routing" { "Return to matrix" } }
    } };
    page(
        &st,
        &token,
        &session,
        &headers,
        view("Receiver-host status", "/routing"),
        &body,
    )
}

#[derive(Deserialize)]
pub(crate) struct RetirePeer {
    csrf: String,
    operation: String,
}

pub(crate) async fn retire_peer_operation(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(form): Form<RetirePeer>,
) -> Response {
    let (token, _) = match need_post(&st, &headers, &form.csrf) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match st.routing_peers.retire(&form.operation).await {
        Ok(_) => Redirect::to(&format!(
            "/routing/peer-operation?operation={}",
            form.operation
        ))
        .into_response(),
        Err(error) => {
            st.set_flash(&token, false, explain(&error));
            Redirect::to("/routing").into_response()
        }
    }
}
