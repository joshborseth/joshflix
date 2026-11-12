//! App updates: the manifest and the APKs `cargo xtask publish` writes into
//! `<data_dir>/updates/`.

use std::io::ErrorKind;

use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use joshflix_protocol::UpdateManifest;
use tower::ServiceExt as _;
use tower_http::services::ServeFile;

use crate::{AppState, api_error, error::json_no_store};

const APK_MIME: &str = "application/vnd.android.package-archive";

/// `true` for `^[A-Za-z0-9._-]+\.apk$`. No slashes, so no traversal.
pub fn is_valid_apk_name(name: &str) -> bool {
    name.len() <= 200
        && name.strip_suffix(".apk").is_some_and(|stem| {
            !stem.is_empty()
                && stem
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
}

/// `GET /api/v1/updates/latest`
pub async fn manifest(State(state): State<AppState>) -> Response {
    let path = state.config().updates_dir().join("manifest.json");
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return api_error(
                StatusCode::NOT_FOUND,
                "no_update",
                "No update has been published.",
            );
        }
        Err(error) => {
            tracing::error!(%error, path = %path.display(), "cannot read update manifest");
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "manifest_unreadable",
                "The update manifest could not be read.",
            );
        }
    };
    match serde_json::from_slice::<UpdateManifest>(&bytes) {
        Ok(manifest) => json_no_store(StatusCode::OK, manifest),
        Err(error) => {
            tracing::error!(%error, "update manifest is malformed");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "bad_manifest",
                "The update manifest is malformed.",
            )
        }
    }
}

/// `GET /updates/{file}`: streams one APK, with Range support.
pub async fn apk(
    State(state): State<AppState>,
    Path(file): Path<String>,
    request: Request,
) -> Response {
    if !is_valid_apk_name(&file) {
        return not_found();
    }
    let path = state.config().updates_dir().join(&file);
    match ServeFile::new(&path).oneshot(request).await {
        Ok(response) => {
            let status = response.status();
            if status == StatusCode::NOT_FOUND {
                return not_found();
            }
            let mut response = response.map(Body::new);
            if status.is_success() {
                response
                    .headers_mut()
                    .insert(header::CONTENT_TYPE, HeaderValue::from_static(APK_MIME));
            }
            response
        }
        Err(error) => {
            tracing::error!(%error, file, "cannot serve APK");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn not_found() -> Response {
    api_error(StatusCode::NOT_FOUND, "not_found", "No such update file.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apk_names() {
        for ok in [
            "joshflix-0.2.0-arm64-v8a.apk",
            "a.apk",
            "A_b-1.2.apk",
            "..apk",
        ] {
            assert!(is_valid_apk_name(ok), "{ok}");
        }
        for bad in [
            ".apk",
            "x.APK",
            "manifest.json",
            "../x.apk",
            "a/b.apk",
            "a\\b.apk",
            "a b.apk",
            "x.apk.txt",
            "%2e%2e.apk",
            "",
        ] {
            assert!(!is_valid_apk_name(bad), "{bad}");
        }
    }
}
