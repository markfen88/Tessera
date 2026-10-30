//! Device-grouped crosspoint view. All actions use the existing reviewed forms.
//!
//! Layout: transmitters (TX, application → network) down the left, one row
//! per source channel under a device band; receivers (RX, network →
//! application) across the top, one column per input under a vertical device
//! band. A collapsed device band shows read-only route counts in place of its
//! channels; the counts never submit anything.
use super::*;
use tessera_control_types::RoutingReceiverPage;

struct ReceiverGroup {
    endpoint: EndpointRef,
    label: String,
    local: Option<DeviceKey>,
    wire_id: Option<String>,
    page: Option<RoutingReceiverPage>,
    problem: Option<String>,
    /// Opening the receiver can read (more of) its inputs.
    can_read: bool,
}

/// Sources are paged in rows of this many channels.
const SOURCES_PER_PAGE: usize = 16;

pub(super) async fn show(
    st: &Shared,
    token: &str,
    session: &Session,
    headers: &HeaderMap,
    selection: Selection,
) -> Response {
    let result = call(
        st,
        RequestBody::Query(Query::RoutingDevices(RoutingDevicesQuery::new(
            selection.cursor.map(RoutingCursor),
        ))),
    )
    .await;
    let body = match result {
        Ok(Reply::RoutingDevices(mut inventory)) => {
            inventory.devices.sort_by_key(|d| {
                (
                    d.local_device.is_none(),
                    d.label.to_ascii_lowercase(),
                    d.endpoint.id.clone(),
                )
            });
            let status = status_of(st).await.ok();
            let mut groups = Vec::new();
            // Bound network waits per document; remaining devices have an
            // explicit read link. Never guess unadvertised receiver channels.
            for (index, device) in inventory.devices.iter().enumerate() {
                let mut group = ReceiverGroup {
                    endpoint: device.endpoint.clone(),
                    label: device.label.clone(),
                    local: device.local_device.clone(),
                    wire_id: device.wire_id.clone(),
                    page: None,
                    problem: None,
                    can_read: device.rx_channels != Some(0),
                };
                if device.rx_channels == Some(0) {
                    group.problem = Some("No receive inputs".into());
                } else if index >= 4 {
                    group.problem = Some("Open this receiver to read its inputs".into());
                } else {
                    match call(
                        st,
                        RequestBody::Query(Query::RoutingReceiver(RoutingReceiverQuery::new(
                            device.endpoint.clone(),
                            1,
                        ))),
                    )
                    .await
                    {
                        Ok(Reply::RoutingReceiver(page)) => group.page = Some(page),
                        Err(e) => group.problem = Some(explain(&e)),
                        _ => group.problem = Some("Receiver observations unavailable".into()),
                    }
                }
                groups.push(group);
            }
            let mut sources = catalogue(st).await;
            sources.sort_by_key(|s| {
                (
                    s.device.to_ascii_lowercase(),
                    s.reference.endpoint.id.clone(),
                    s.reference.channel,
                )
            });
            let source_page = selection
                .source_page
                .unwrap_or(0)
                .min(sources.len().saturating_sub(1) / SOURCES_PER_PAGE);
            let start = source_page * SOURCES_PER_PAGE;
            let visible = &sources[start..sources.len().min(start + SOURCES_PER_PAGE)];
            html! { section.card.routing-board data-routing-board {
                div.routing-heading {
                    div { h2 { "Routing matrix" } p.muted { "Transmitters (TX, application → network) down the left · receivers (RX, network → application) across the top." } }
                    div.actions {
                        (view_switch())
                        a.btn.sm href="/routing" { "Refresh" }
                        a.btn.sm href="/routing/recovery" { "Recover uncertain operation" }
                        a.btn.sm href="/routing/carrier-recovery" { "Review restart recovery" }
                    }
                }
                @if !inventory.complete { p.flash.bad { "Discovery is incomplete. Refresh before changing a route." } }
                (stale_notice())
                (legend())
                div.routing-desktop { (grid(&groups, visible, status.as_ref())) }
                (receiver_notes(&groups))
                (receiver_list(&groups, &sources))
                @if sources.len() > SOURCES_PER_PAGE { nav.actions aria-label="Source channel pages" {
                    @if source_page > 0 { a.btn.sm href=(format!("/routing?source_page={}", source_page - 1)) { "Previous transmitters" } }
                    span.muted { "Transmit channels " (start + 1) "–" (start + visible.len()) " of " (sources.len()) }
                    @if start + SOURCES_PER_PAGE < sources.len() { a.btn.sm href=(format!("/routing?source_page={}", source_page + 1)) { "Next transmitters" } }
                } }
                @if let Some(cursor) = inventory.next_cursor { form method="get" action="/routing" {
                    input type="hidden" name="cursor" value=(cursor.0);
                    button.btn.sm type="submit" { "More devices" }
                } }
                @if groups.is_empty() { p { "No devices discovered. Check that discovery browsing is enabled." } }
                @if sources.is_empty() { p { "No source channels are currently available. Refresh discovery before routing." } }
                p.muted.routing-help { "Click a crosspoint to connect, replace or disconnect. Changes require confirmation. One source per RX input. A source can feed several inputs; a receiver still on its compatibility runtime takes it on only one of its own inputs. Arrow keys move between cells; Enter opens a change. Counts in a collapsed band are read-only." }
                p.muted { "Receiving means local packet activity, not verified audio. Each receiver enforces its own routing permissions. Audio status is unverified." }
                (dialog())
            } }
        }
        Err(e) => {
            html! { section.card { h2 { "Routing matrix" } (error_box(&explain(&e))) a.btn href="/routing" { "Refresh devices" } } }
        }
        _ => html! { section.card { (error_box("Routing inventory unavailable")) } },
    };
    page(
        st,
        token,
        session,
        headers,
        view("Routing matrix", "/routing").every(10),
        &body,
    )
}

fn view_switch() -> Markup {
    html! { div.routing-view-switch id="routing-view" data-live-local {
        button.btn.sm type="button" data-routing-view="matrix" aria-pressed="true" { "Matrix" }
        button.btn.sm type="button" data-routing-view="list" aria-pressed="false" { "List" }
    } }
}

fn stale_notice() -> Markup {
    html! { p.flash.bad.routing-stale hidden data-routing-stale data-live-local { "Selections are stale. Refresh before making a change." } }
}

fn filters(id: &str) -> Markup {
    html! { div.routing-filters id=(id) data-live-local {
        label { span { "Filter transmitters" } input type="search" data-routing-filter="tx" placeholder="Device or channel"; }
        label { span { "Filter receivers" } input type="search" data-routing-filter="rx" placeholder="Device or channel"; }
    } }
}

/// The filter/axis corner: filters (script only), then one axis label with
/// expand and collapse controls per side, like a desktop routing matrix.
fn corner(receivers: usize, transmitters: usize) -> Markup {
    html! { th.routing-corner rowspan="2" scope="col" {
        div.routing-corner-box {
            (filters("routing-filters"))
            div.routing-axis.routing-axis-rx {
                span.routing-axis-name { span.routing-vlabel { abbr title="Receivers: network to application" { "RX" } " · Receivers (" (receivers) ")" } }
                span.routing-axis-tools {
                    button.routing-toggle-box type="button" data-routing-expand="false" data-routing-axis="rx" aria-label="Collapse all receivers" title="Collapse all receivers" { "−" }
                    button.routing-toggle-box type="button" data-routing-expand="true" data-routing-axis="rx" aria-label="Expand all receivers" title="Expand all receivers" { "+" }
                }
            }
            div.routing-axis.routing-axis-tx {
                span.routing-axis-tools {
                    button.routing-toggle-box type="button" data-routing-expand="true" data-routing-axis="tx" aria-label="Expand all transmitters" title="Expand all transmitters" { "+" }
                    button.routing-toggle-box type="button" data-routing-expand="false" data-routing-axis="tx" aria-label="Collapse all transmitters" title="Collapse all transmitters" { "−" }
                }
                span.routing-axis-name { abbr title="Transmitters: application to network" { "TX" } " · Transmitters (" (transmitters) ")" }
            }
        }
    } }
}

fn legend() -> Markup {
    html! { div.routing-legend aria-label="Route states" {
        span { i.route-dot.receiving { "✓" } " Receiving" }
        span { i.route-dot.confirmed { "✓" } " Receiver-confirmed" }
        span { i.route-dot.no-media { "!" } " No media" }
        span { i.route-dot.empty { "+" } " Empty" }
        span { i.route-dot.unavailable { "×" } " Unavailable" }
        span { i.route-dot.pending { "…" } " Pending" }
        span { i.route-dot.route-sum { "2" } " Routes in a collapsed band" }
    } }
}

pub(super) fn single(receiver: &RoutingReceiverPage, sources: &[SourceChoice]) -> Markup {
    let groups = [ReceiverGroup {
        endpoint: receiver.endpoint.clone(),
        label: receiver.label.clone(),
        local: None,
        wire_id: None,
        page: Some(receiver.clone()),
        problem: None,
        can_read: false,
    }];
    html! { div.routing-board data-routing-board {
        (view_switch()) (stale_notice()) (legend())
        div.routing-desktop { (grid(&groups, sources, None)) }
        (receiver_notes(&groups))
        (receiver_list(&groups, sources))
        (dialog())
    } }
}

fn source_groups(sources: &[SourceChoice]) -> Vec<&[SourceChoice]> {
    let mut groups = Vec::new();
    let mut start = 0;
    while start < sources.len() {
        let mut end = start + 1;
        while end < sources.len()
            && sources[end].reference.endpoint == sources[start].reference.endpoint
        {
            end += 1;
        }
        groups.push(&sources[start..end]);
        start = end;
    }
    groups
}

/// One receiver column: an input, or a placeholder when the receiver's
/// inputs are not readable in this document.
enum Column<'a> {
    Input {
        group: &'a ReceiverGroup,
        page: &'a RoutingReceiverPage,
        row: &'a RoutingReceiverChannel,
    },
    Void {
        group: &'a ReceiverGroup,
        reason: &'a str,
        short: &'static str,
    },
}

impl Column<'_> {
    fn group(&self) -> &ReceiverGroup {
        match self {
            Column::Input { group, .. } | Column::Void { group, .. } => group,
        }
    }
    fn channel(&self) -> String {
        match self {
            Column::Input { row, .. } => row.id.to_string(),
            Column::Void { .. } => String::new(),
        }
    }
}

fn columns_of(group: &ReceiverGroup) -> Vec<Column<'_>> {
    match (&group.page, &group.problem) {
        (Some(page), _) if page.channels.is_empty() => vec![Column::Void {
            group,
            reason: "No channel observations",
            short: "Unknown",
        }],
        (Some(page), _) => page
            .channels
            .iter()
            .map(|row| Column::Input { group, page, row })
            .collect(),
        (None, Some(reason)) => vec![Column::Void {
            group,
            reason: reason.as_str(),
            short: if group.can_read {
                "Unread"
            } else {
                "No inputs"
            },
        }],
        (None, None) => vec![Column::Void {
            group,
            reason: "Receiver observations unavailable",
            short: "Unavailable",
        }],
    }
}

/// Whether a receiver input's reported source is one of these channels.
fn fed_by(row: &RoutingReceiverChannel, sources: &[SourceChoice]) -> bool {
    row.source
        .as_ref()
        .is_some_and(|selected| sources.iter().any(|s| s.matches(selected)))
}

fn routes_into(page: &RoutingReceiverPage, sources: &[SourceChoice]) -> usize {
    page.channels
        .iter()
        .filter(|row| fed_by(row, sources))
        .count()
}

fn band_title(group: &ReceiverGroup) -> String {
    let mut title = group.label.clone();
    if group.local.is_some() {
        title.push_str(" · This host");
    }
    if let Some(wire_id) = &group.wire_id {
        title.push_str(" · Device ID ");
        title.push_str(wire_id);
    }
    if let Some(problem) = &group.problem {
        title.push_str(" · ");
        title.push_str(problem);
    }
    if let Some(reason) = group
        .page
        .as_ref()
        .and_then(|p| p.operation_unavailable.as_ref())
    {
        title.push_str(" · ");
        title.push_str(reason);
    }
    title
}

fn grid(groups: &[ReceiverGroup], sources: &[SourceChoice], status: Option<&Status>) -> Markup {
    let tx_groups = source_groups(sources);
    let rx_columns: Vec<Vec<Column<'_>>> = groups.iter().map(columns_of).collect();
    html! { div.routing-scroll tabindex="0" aria-label="Audio routing crosspoints" {
        table.routing-matrix {
            caption.sr { "Transmit source channels down rows and receive destination inputs across columns" }
            thead {
                tr {
                    (corner(groups.len(), tx_groups.len()))
                    @for (group, columns) in groups.iter().zip(&rx_columns) {
                        th.routing-rx-device scope="colgroup" colspan=(columns.len()) data-rx-heading=(group.endpoint.id) data-search=(group.label) title=(band_title(group)) {
                            button type="button" data-routing-toggle="rx" data-group=(group.endpoint.id) aria-expanded="true" {
                                span.routing-toggle-box aria-hidden="true" {}
                                span.routing-vlabel { bdi { (group.label) } }
                                span.sr { (band_title(group)) }
                            }
                            @if group.local.is_some() { span.routing-band-tag { "host" } }
                            @if group.problem.is_some() { span.routing-band-tag.bad aria-hidden="true" { "!" } }
                        }
                    }
                }
                tr {
                    @for (group, columns) in groups.iter().zip(&rx_columns) {
                        @for (index, column) in columns.iter().enumerate() { (rx_header(column, index == 0)) }
                        @if let Some(page) = &group.page { @if !page.channels.is_empty() {
                            th.routing-rx-channel.routing-rx-summary.routing-group-start scope="col" hidden data-rx-summary=(group.endpoint.id) title=(format!("{}: {} inputs (collapsed)", group.label, page.channels.len())) {
                                span.routing-vlabel { (page.channels.len()) " inputs" }
                            }
                        } }
                    }
                }
            }
            @for tx in &tx_groups { tbody data-live-key=(format!("tx:{}", tx[0].reference.endpoint.id)) {
                tr.routing-tx-device data-tx-heading=(tx[0].reference.endpoint.id) data-search=(tx[0].device) {
                    th scope="rowgroup" title=(tx[0].device) {
                        button type="button" data-routing-toggle="tx" data-group=(tx[0].reference.endpoint.id) aria-expanded="true" {
                            span.routing-toggle-box aria-hidden="true" {}
                            bdi { (tx[0].device) }
                        }
                    }
                    @for (group, columns) in groups.iter().zip(&rx_columns) {
                        @for (index, column) in columns.iter().enumerate() {
                            @match column {
                                Column::Input { row, .. } => { (summary_cell(group, column.channel(), usize::from(fed_by(row, tx)), &format!("{} / input {}", group.label, row.id), &tx[0].device, index == 0)) },
                                Column::Void { .. } => { (void_cell(group, index == 0)) },
                            }
                        }
                        @if let Some(page) = &group.page { @if !page.channels.is_empty() {
                            (group_summary_cell(group, routes_into(page, tx), &tx[0].device))
                        } }
                    }
                }
                @for source in *tx {
                    tr data-tx=(source.reference.endpoint.id) data-tx-channel=(source.reference.channel) data-search=(format!("{} {}", source.device, source.channel)) {
                        th.routing-tx-channel scope="row" title=(format!("{} / {}", source.device, source.channel)) {
                            span.routing-tree aria-hidden="true" {}
                            bdi { (source.channel) }
                        }
                        @for (group, columns) in groups.iter().zip(&rx_columns) {
                            @for (index, column) in columns.iter().enumerate() {
                                @match column {
                                    Column::Input { page, row, .. } => {
                                        td.routing-crosspoint.routing-group-start[index == 0] data-rx=(group.endpoint.id) data-rx-channel=(row.id) {
                                            (cell(page, row, source, media(group, row, status)))
                                        }
                                    },
                                    Column::Void { .. } => { (void_cell(group, index == 0)) },
                                }
                            }
                            @if let Some(page) = &group.page { @if !page.channels.is_empty() {
                                (group_summary_cell(group, routes_into(page, std::slice::from_ref(source)), &format!("{} / {}", source.device, source.channel)))
                            } }
                        }
                    }
                }
            } }
        }
    } }
}

fn rx_header(column: &Column<'_>, first: bool) -> Markup {
    let group = column.group();
    match column {
        Column::Input { row, .. } => {
            let label = row.label.as_deref().unwrap_or("Input");
            let title = format!(
                "{} / {} (input {}) · {} · receiver status (raw) 0x{:08x}",
                group.label,
                label,
                row.id,
                selected(row),
                row.raw_status
            );
            html! {
                th.routing-rx-channel.routing-group-start[first] scope="col" data-rx=(group.endpoint.id) data-rx-channel=(row.id)
                    data-search=(format!("{} {}", group.label, label))
                    data-route-key=(format!("{}:{}", group.endpoint.id, row.id)) data-selection=(serde_json::to_string(&row.source).unwrap()) title=(title) {
                    span.routing-vlabel { bdi { (label) } }
                    span.sr { (selected(row)) }
                    span.routing-external hidden { span aria-hidden="true" { "!" } span.sr { "Changed externally" } }
                }
            }
        }
        Column::Void { reason, short, .. } => html! {
            th.routing-rx-channel.routing-rx-void.routing-group-start[first] scope="col" data-rx=(group.endpoint.id) data-rx-channel="" data-search=(group.label) title=(format!("{}: {}", group.label, reason)) {
                @if group.page.is_none() && group.can_read {
                    (open_receiver(&group.endpoint, "Read inputs"))
                } @else {
                    span.routing-vlabel { (short) }
                }
                span.sr { (reason) }
            }
        },
    }
}

/// A read-only count in a device band. Nothing here is a crosspoint.
fn summary_cell(
    group: &ReceiverGroup,
    channel: String,
    count: usize,
    destination: &str,
    origin: &str,
    first: bool,
) -> Markup {
    html! { td.routing-summary.routing-group-start[first] data-rx=(group.endpoint.id) data-rx-channel=(channel) {
        (summary_mark(count, destination, origin))
    } }
}

fn group_summary_cell(group: &ReceiverGroup, count: usize, origin: &str) -> Markup {
    html! { td.routing-summary.routing-rx-summary.routing-group-start hidden data-rx-summary=(group.endpoint.id) {
        (summary_mark(count, &group.label, origin))
    } }
}

fn summary_mark(count: usize, destination: &str, origin: &str) -> Markup {
    let plural = if count == 1 { "" } else { "s" };
    html! { @if count > 0 {
        span.route-sum title=(format!("{count} route{plural} from {origin} into {destination}; expand to change")) {
            (count) span.sr { " route" (plural) " from " (origin) " into " (destination) "; expand to change" }
        }
    } }
}

fn void_cell(group: &ReceiverGroup, first: bool) -> Markup {
    html! { td.routing-crosspoint.routing-void.routing-group-start[first] data-rx=(group.endpoint.id) data-rx-channel="" {} }
}

fn media<'a>(
    group: &ReceiverGroup,
    row: &RoutingReceiverChannel,
    status: Option<&'a Status>,
) -> Option<&'a SubscriptionState> {
    let key = group.local.as_ref()?;
    let channel = status?
        .devices
        .iter()
        .find(|d| &d.key == key)?
        .rx_channels
        .iter()
        .find(|c| c.channel.get() == row.id)?;
    let route = channel.routing.as_ref()?;
    let intent = route.intent.as_ref()?;
    let selected = row.source.as_ref()?;
    if intent.tx_device != selected.tx_device || intent.tx_channel != selected.tx_channel {
        return None;
    }
    route.media.as_ref()
}

fn cell(
    page: &RoutingReceiverPage,
    row: &RoutingReceiverChannel,
    source: &SourceChoice,
    media: Option<&SubscriptionState>,
) -> Markup {
    let active = row.source.as_ref().is_some_and(|p| source.matches(p));
    let allowed = row.expectation.is_some() && source.available_to(page, row);
    let (class, symbol, state) = if active {
        match media {
            Some(SubscriptionState::Receiving) => ("receiving", "✓", "Receiving"),
            Some(SubscriptionState::InProgress) => ("pending", "…", "Pending media"),
            Some(_) => ("no-media", "!", "No media"),
            None => ("confirmed", "✓", "Receiver-confirmed"),
        }
    } else if !allowed {
        ("unavailable", "×", "Unavailable")
    } else {
        ("empty", "+", "Empty")
    };
    let action = if active {
        "Disconnect"
    } else if row.source.is_some() {
        "Replace with"
    } else {
        "Connect"
    };
    let description = format!(
        "{action} {} / {} → {} / input {}. {state}{}",
        source.device,
        source.channel,
        page.label,
        row.id,
        if allowed {
            ""
        } else if !active && source.feeds_another(page, row) {
            ". This source already feeds another input on this receiver"
        } else {
            ". Read only or unsupported on this host"
        }
    );
    let change = if active {
        change(row, None)
    } else {
        change(row, Some(source))
    };
    let expected = if active {
        None
    } else {
        Some(tessera_control_types::ReceiverSelection::new(
            &source.device,
            &source.channel,
        ))
    };
    html! { form.routing-cell-form method="get" action="/routing/review" data-routing-choice data-expected-selection=(serde_json::to_string(&expected).unwrap()) {
        input type="hidden" name="receiver" value=(serde_json::to_string(&page.endpoint).unwrap());
        input type="hidden" name="rx" value=(row.id);
        @if let Some(change) = change { input type="hidden" name="change" value=(serde_json::to_string(&change).unwrap()); }
        button class=(format!("route-cell {class}")) type="submit" disabled[!allowed] data-allowed=(allowed) aria-label=(description) title=(description) aria-pressed=(active) {
            span aria-hidden="true" { (symbol) } span.sr { (state) }
        }
    } }
}

fn open_receiver(endpoint: &EndpointRef, text: &str) -> Markup {
    html! { form.routing-open method="get" action="/routing" {
        input type="hidden" name="endpoint" value=(serde_json::to_string(endpoint).unwrap());
        button type="submit" { (text) }
    } }
}

/// Per-receiver notes that do not fit a vertical band: reasons, paging and
/// the read link for receivers not read in this document.
fn receiver_notes(groups: &[ReceiverGroup]) -> Markup {
    let noted: Vec<&ReceiverGroup> = groups
        .iter()
        .filter(|g| {
            let page_note = g.page.as_ref().is_some_and(|p| {
                p.operation_unavailable.is_some() || p.next_first.is_some() || p.channels.is_empty()
            });
            g.problem.is_some() || g.wire_id.is_some() || page_note
        })
        .collect();
    html! { @if !noted.is_empty() { ul.routing-notes aria-label="Receiver notes" {
        @for group in noted { li data-live-key=(format!("note:{}", group.endpoint.id)) {
            strong { bdi { (group.label) } }
            @if group.local.is_some() { " " span.routing-host { "This host" } }
            @if let Some(wire_id) = &group.wire_id { " " span.routing-host { "Device ID " (wire_id) } }
            @if let Some(problem) = &group.problem { span.routing-reason { (problem) } }
            @if let Some(page) = &group.page {
                @if let Some(reason) = &page.operation_unavailable { span.routing-reason { (reason) } }
                @if page.channels.is_empty() { span.routing-reason { "The receiver returned no channel observations; selections remain unknown." } }
                @if let Some(first) = page.next_first { form.routing-open method="get" action="/routing" {
                    input type="hidden" name="endpoint" value=(serde_json::to_string(&group.endpoint).unwrap());
                    button type="submit" name="first" value=(first) { "More inputs" }
                } }
            }
            @if group.can_read { (open_receiver(&group.endpoint, "Open receiver")) }
        } }
    } } }
}

fn receiver_list(groups: &[ReceiverGroup], sources: &[SourceChoice]) -> Markup {
    html! { div.routing-list { h3 { "Receiver-first list" }
        (filters("routing-list-filters"))
        @for group in groups { details open data-list-device data-live-key=(format!("list:{}", group.endpoint.id)) {
            summary { (group.label) }
            @if let Some(wire_id) = &group.wire_id { p.muted { "Device ID " (wire_id) } }
            @if let Some(page) = &group.page {
                @if let Some(reason) = &page.operation_unavailable { p.muted { (reason) } }
                @for row in &page.channels { article data-list-search=(format!("{} {}", group.label, row.label.as_deref().unwrap_or(""))) {
                    h4 { (row.label.as_deref().unwrap_or("Input")) " (" (row.id) ")" }
                    p { (selected(row)) }
                    p.muted { "Receiver status (raw) " (format!("0x{:08x}", row.raw_status)) }
                    @if row.expectation.is_some() {
                        details { summary { "Choose a source" }
                            @for source in sources.iter().filter(|s| s.available_to(page, row)) { (choice(page, row, Some(source), false)) }
                        }
                        @if row.source.is_some() { (choice(page, row, None, false)) }
                    }
                } }
            } @else if let Some(reason) = &group.problem {
                p { (reason) }
                @if group.can_read { (open_receiver(&group.endpoint, "Read receiver inputs")) }
            }
        } }
    } }
}

fn dialog() -> Markup {
    html! { dialog.routing-dialog id="routing-dialog" aria-label="Confirm routing change" data-live-local {
        div data-routing-dialog-content {}
        button.btn.sm type="button" data-routing-close { "Close" }
    } }
}
