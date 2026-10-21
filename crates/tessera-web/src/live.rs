//! Small, optional browser enhancement for server-rendered status pages.

use axum::http::header;
use axum::response::IntoResponse;

pub(crate) async fn script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../assets/live.js"),
    )
}
