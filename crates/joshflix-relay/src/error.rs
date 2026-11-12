use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use joshflix_protocol::ApiError;

/// A JSON [`ApiError`] response. Never cached.
pub fn api_error(status: StatusCode, code: &str, message: &str) -> Response {
    no_store(
        (
            status,
            Json(ApiError {
                code: code.to_owned(),
                message: message.to_owned(),
            }),
        )
            .into_response(),
    )
}

/// Marks a response as uncacheable. Every pairing response can carry secrets.
pub(crate) fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// JSON body with `Cache-Control: no-store`.
pub(crate) fn json_no_store<T: serde::Serialize>(status: StatusCode, body: T) -> Response {
    no_store((status, Json(body)).into_response())
}
