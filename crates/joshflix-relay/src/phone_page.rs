//! The page a phone opens after scanning the TV's QR code.
//!
//! One self-contained HTML document: inline CSS and JS guarded by a per-request
//! CSP nonce, no external requests. The pairing code is validated with
//! [`is_valid_pairing_code`] before it is embedded, so it only ever contains
//! `[A-Za-z0-9_-]`; anything else renders the page in its "missing" state.

use axum::{
    extract::{Path, State},
    http::{HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use base64::Engine as _;
use joshflix_protocol::{PhonePairingStatus, is_valid_pairing_code};

use crate::{AppState, pairing::phone_touch};

const TEMPLATE: &str = include_str!("phone.html");

/// Renders the page for `code` (empty when invalid) with the given CSP nonce.
pub fn render(code: &str, nonce: &str) -> String {
    let code = if is_valid_pairing_code(code) {
        code
    } else {
        ""
    };
    TEMPLATE
        .replace("{{NONCE}}", nonce)
        .replace("{{CODE}}", code)
}

/// `GET /p/{code}`
pub async fn page(State(state): State<AppState>, Path(code): Path<String>) -> Response {
    let mut nonce_bytes = [0u8; 16];
    rand::fill(&mut nonce_bytes[..]);
    let nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(nonce_bytes);

    // Opening the page counts as "phone connected" on the TV.
    let status = phone_touch(&state, &code);
    let (http_status, embedded) = match status {
        PhonePairingStatus::Missing => (StatusCode::NOT_FOUND, ""),
        _ => (StatusCode::OK, code.as_str()),
    };

    let mut response = (http_status, Html(render(embedded, &nonce))).into_response();
    let headers = response.headers_mut();
    let csp = format!(
        "default-src 'none'; script-src 'nonce-{nonce}'; style-src 'nonce-{nonce}'; \
         connect-src 'self'; img-src 'none'; base-uri 'none'; form-action 'none'; \
         frame-ancestors 'none'"
    );
    if let Ok(csp) = HeaderValue::from_str(&csp) {
        headers.insert(header::CONTENT_SECURITY_POLICY, csp);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_only_valid_codes() {
        let page = render("abcdefghijklmnopqrstu_", "NONCE1");
        assert!(page.contains(r#"data-code="abcdefghijklmnopqrstu_""#));
        assert!(page.contains(r#"nonce="NONCE1""#));
        assert!(!page.contains("{{"));

        let page = render("\"><script>alert(1)</script>", "N");
        assert!(page.contains(r#"data-code="""#));
        assert!(!page.contains("alert(1)"));
    }

    #[test]
    fn page_is_self_contained() {
        let page = render("abcdefghijklmnopqrstu_", "N");
        // Only link out is the Real-Debrid token page.
        let externals: Vec<_> = page
            .match_indices("https://")
            .map(|(i, _)| &page[i..i + 34])
            .collect();
        assert_eq!(externals, ["https://real-debrid.com/apitoken\" "]);
        for copy in [
            "JOSHFLIX TV",
            "Real-Debrid API key",
            "This key goes only to the TV that showed the QR code. Nothing is saved on the server.",
            "Checking this code…",
            "This code is invalid or expired. Go back to the TV and scan a new QR code.",
            "This TV already received a key. You can go back to the TV.",
            "Paste your Real-Debrid API key",
            "Send to TV",
            "Checking with Real-Debrid…",
        ] {
            assert!(page.contains(copy), "missing copy: {copy}");
        }
    }
}
