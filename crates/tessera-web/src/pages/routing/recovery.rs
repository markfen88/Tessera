use super::*;

pub(crate) async fn inspect(State(st): State<Shared>, headers: HeaderMap) -> Response {
    let (token, session) = match need(&st, &headers) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let result = call(
        &st,
        RequestBody::Query(Query::InspectReceiverOperation { operation: None }),
    )
    .await;
    let body = html! { section.card {
        h2 { "Review uncertain routing operation" }
        p { "Read the receiver's current selections before allowing another routing change. Accepting this baseline sends no routing command." }
        @match result {
            Ok(Reply::ReceiverRecovery(review)) => {
                h3 { (review.label) }
                @if let Some(context) = &review.context {
                    p { "Uncertain operation on input " (context.affected_rx) ". Requested source: "
                        @if let Some(desired) = &context.desired { (desired.tx_device) " / " (desired.tx_channel) }
                        @else { "Disconnect" }
                    }
                    details { summary { "Original operation" } pre { (serde_json::to_string_pretty(context).unwrap()) } }
                } @else { p { "This older operation has no saved context. Its original outcome is unknown." } }
                table { thead { tr { th { "Input" } th { "Receiver now reports" } th { "Previously saved" } th { "Raw status" } } }
                    tbody { @for row in &review.observed { tr {
                        th scope="row" { (row.rx) }
                        td { @if let Some(source) = &row.source { (source.tx_device) " / " (source.tx_channel) } @else { "No source" } }
                        td { @if let Some(source) = review.saved.iter().find(|s| s.rx == row.rx).and_then(|s| s.source.as_ref()) { (source.tx_device) " / " (source.tx_channel) } @else { "No source" } }
                        td { (format!("0x{:08x}", row.raw_status)) }
                    } } }
                }
                details { summary { "Configured and stored carriers" }
                    h4 { "Configured" } pre { (serde_json::to_string_pretty(&review.configured_carriers).unwrap()) }
                    h4 { "Stored" } pre { (serde_json::to_string_pretty(&review.stored_carriers).unwrap()) }
                }
                @if review.runtime_reports_fenced { p { "Host carrier protection remains enabled. Accepting selections does not remove that protection." } }
                p.muted { "The original operation remains Unknown. Names do not verify audio or uninterrupted playback. Other controllers may change selections after this review." }
                @if let Some(reason) = &review.unavailable { (error_box(reason)) }
                @if let Some(review_token) = review.review {
                    form method="post" action="/routing/apply" {
                        input type="hidden" name="csrf" value=(session.csrf);
                        input type="hidden" name="command" value=(serde_json::to_string(&Command::ReconcileReceiverOperation { review: review_token }).unwrap());
                        input type="hidden" name="receiver" value="";
                        button.btn type="submit" { "Accept these selections as the saved baseline" }
                    }
                    p.muted { "The daemon reads every input again before saving. A changed selection cancels acceptance. This review expires after 30 seconds." }
                }
            },
            Err(error) => { (error_box(&explain(&error))) },
            Ok(_) => { (error_box("Unexpected receiver review response")) },
        }
        p { a href="/routing/recovery" { "Read again" } " · " a href="/routing/carrier-recovery" { "Review carriers after a restart" } " · " a href="/routing" { "Return to matrix" } }
    } };
    page(
        &st,
        &token,
        &session,
        &headers,
        view("Recover routing", "/routing"),
        &body,
    )
}

pub(crate) async fn inspect_carriers(State(st): State<Shared>, headers: HeaderMap) -> Response {
    let (token, session) = match need(&st, &headers) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let result = call(&st, RequestBody::Query(Query::InspectRuntimeCarriers)).await;
    let body = html! { section.card {
        h2 { "Review carriers after a restart" }
        p { "A previous runtime may have left stored audio carriers. Review the entries below before returning eligible carriers to the current runtime's management." }
        @match result {
            Ok(Reply::RuntimeCarrierRecovery(inspection)) => {
                @if inspection.carriers.is_empty() { p { "No stored carriers." } }
                @else { table {
                    thead { tr { th { "Entry" } th { "Carrier" } th { "Recorded origin" } th { "Action" } } }
                    tbody { @for row in &inspection.carriers { tr {
                        th scope="row" { (u64::from(row.occurrence) + 1) }
                        td { (row.carrier.id) details { summary { "Carrier details" } pre { (serde_json::to_string_pretty(&row.carrier).unwrap()) } } }
                        td { @if let Some(origin) = &row.origin_runtime { code { (origin) } } @else { "Unknown or host protected" } }
                        td { @if row.releasable { "Return to current runtime management" }
                            @else if inspection.next_run_occurrences.contains(&row.occurrence) { "Included in next-start authorization" }
                            @else { "Keep protected" }
                        }
                    } } }
                } }
                details { summary { "Current runtime" } code { (inspection.current_runtime) } }
                p { "Confirming changes management rights only. A later complete runtime report may replace or remove the eligible whole carriers, affecting audio that uses them. Receiver selections are not changed by confirmation." }
                p.muted { "Unknown origins, host-owned carriers and protected empty slots stay protected. Audio and routing readiness still need verification." }
                @if let Some(reason) = &inspection.unavailable { (error_box(reason)) }
                @if inspection.unavailable.is_none() {
                    @if let Some(review) = &inspection.review {
                        form method="post" action="/routing/apply" {
                            input type="hidden" name="csrf" value=(session.csrf);
                            input type="hidden" name="command" value=(serde_json::to_string(&Command::ReleaseRuntimeCarriers { review: review.clone() }).unwrap());
                            button.btn type="submit" { "Return eligible carriers to runtime management" }
                        }
                        p.muted { "The daemon checks this complete review again before saving. This review expires after 30 seconds and can be used once." }
                    }
                }
                h3 { "Authorize a subsequent runtime start" }
                p { "This is a new administrator decision for every stored carrier of the first local device. It does not prove who created them. The next run may replace or remove these complete carriers; other-device and configured content stays protected." }
                details { summary { "Saved receiver selections to preserve" } pre { (serde_json::to_string_pretty(&inspection.saved).unwrap()) } }
                details { summary { "Configured carriers to preserve" } pre { (serde_json::to_string_pretty(&inspection.configured_carriers).unwrap()) } }
                @if inspection.next_run_authorized { p { "An authorization for the next start is already recorded. An intervening host write, binding change, changed selection or daemon restart invalidates it." } }
                @if let Some(reason) = &inspection.next_run_unavailable { (error_box(reason)) }
                @if inspection.next_run_unavailable.is_none() {
                    @if let Some(review) = &inspection.next_run_review {
                        form method="post" action="/routing/apply" {
                            input type="hidden" name="csrf" value=(session.csrf);
                            input type="hidden" name="command" value=(serde_json::to_string(&Command::GrantRuntimeCarriersToNextRun { review: review.clone() }).unwrap());
                            button.btn type="submit" { "Authorize the next runtime start to replace these carriers" }
                        }
                        p { "Restarting the runtime may interrupt audio. This button records permission only; use the attended service update procedure. The new run must deliver its own complete report before routing becomes ready." }
                        p.muted { "The review expires after 30 seconds and can be used once. Existing host protection for carriers and empty slots is retained." }
                    }
                }
            },
            Err(error) => { (error_box(&explain(&error))) },
            Ok(_) => { (error_box("Unexpected carrier review response")) },
        }
        p { a href="/routing/carrier-recovery" { "Read again" } " · " a href="/routing/recovery" { "Review uncertain operation" } " · " a href="/routing" { "Return to matrix" } }
    } };
    page(
        &st,
        &token,
        &session,
        &headers,
        view("Recover carriers", "/routing"),
        &body,
    )
}
