//! End-to-end exercise of the device-login sequence through the crate's
//! PUBLIC surface, against a raw sequenced HTTP server.
//!
//! Extends the one-shot server pattern of `resolve_remote_test.rs` with a
//! `spawn_seq` that serves a list of responses in order — the whole point of
//! this test is that one endpoint (the token endpoint) must answer
//! differently on successive polls. The per-step behaviour (slow_down,
//! access_denied, issuer mismatch, ...) is unit-tested in `src/oidc.rs`;
//! what this file adds is proof the steps compose from outside the crate,
//! exactly as `tracevault login` calls them.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tracevault_cli::oidc;

const RECV_TIMEOUT: Duration = Duration::from_secs(5);

fn http_json(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        body.len(),
        body
    )
}

/// Serve `build(base_url)`'s responses, one per incoming request, capturing
/// each request as `"<request line> | <body>"`. `build` receives the base URL
/// because an OpenID discovery document has to self-report the issuer it is
/// served from, which isn't known until the ephemeral port is bound.
fn spawn_seq<F>(build: F) -> (String, mpsc::Receiver<String>)
where
    F: FnOnce(&str) -> Vec<String>,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let responses = build(&base);
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for response in responses {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            // Consume the WHOLE request (headers and body) before replying,
            // so a client POSTing a form never has its write end reset.
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            let _ = reader.read_line(&mut request_line);
            let mut len = 0usize;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 {
                    break;
                }
                let header = header.trim_end();
                if header.is_empty() {
                    break;
                }
                // Case-insensitive: hyper emits header names lowercased.
                let lower = header.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            if len > 0 {
                let _ = reader.read_exact(&mut body);
            }
            let _ = tx.send(format!(
                "{} | {}",
                request_line.trim_end(),
                String::from_utf8_lossy(&body)
            ));
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (base, rx)
}

/// The exact call sequence `tracevault login` performs: ask the TraceVault
/// server for the realm, discover it, start a device authorization, then poll
/// until the user approves. The first poll answers `authorization_pending`,
/// so this also proves the loop survives the normal case of a human who
/// hasn't clicked yet.
#[tokio::test]
async fn device_login_sequence_completes_through_the_public_api() {
    let (base, rx) = spawn_seq(|base| {
        vec![
            // 1. Which realm does this server trust? (The server publishes
            //    itself as the issuer here only because the fake IdP and the
            //    fake TraceVault server are the same socket.)
            http_json(
                "200 OK",
                &format!(
                    r#"{{"oidc_enabled":true,"issuer":"{base}","audience":"tracevault","cli_client_id":"tracing-cli"}}"#
                ),
            ),
            // 2. Discovery.
            http_json(
                "200 OK",
                &format!(
                    r#"{{"issuer":"{base}","device_authorization_endpoint":"{base}/device","token_endpoint":"{base}/token","revocation_endpoint":"{base}/revoke"}}"#
                ),
            ),
            // 3. Device authorization.
            http_json(
                "200 OK",
                &format!(
                    r#"{{"device_code":"secret-device-code","user_code":"WDJB-MJHT","verification_uri":"{base}/device/verify","verification_uri_complete":"{base}/device/verify?user_code=WDJB-MJHT","expires_in":600,"interval":5}}"#
                ),
            ),
            // 4. User hasn't approved yet.
            http_json("400 Bad Request", r#"{"error":"authorization_pending"}"#),
            // 5. Approved.
            http_json(
                "200 OK",
                r#"{"access_token":"the-access-token","refresh_token":"the-refresh-token","expires_in":300}"#,
            ),
        ]
    });

    let http = reqwest::Client::new();

    let config = oidc::fetch_public_config(&http, &base).await.unwrap();
    assert_eq!(config.cli_client_id, "tracing-cli");

    let discovery = oidc::discover(&http, &config.issuer).await.unwrap();
    let device = oidc::device_start(&http, &discovery, &config.cli_client_id)
        .await
        .unwrap();
    assert_eq!(device.user_code, "WDJB-MJHT");
    assert!(
        device.verification_uri_complete.is_some(),
        "the complete URI is what gets auto-opened in a browser"
    );

    // No-op sleep: the poll loop's timing is unit-tested; here it must not
    // cost the suite 10 wall-clock seconds.
    let tokens = oidc::poll_token_with(&http, &discovery, &config.cli_client_id, &device, |_| {
        std::future::ready(())
    })
    .await
    .unwrap();

    assert_eq!(tokens.access_token, "the-access-token");
    assert_eq!(tokens.refresh_token.as_deref(), Some("the-refresh-token"));
    assert_eq!(tokens.access_expires_at(1_000), 1_300);

    let requests: Vec<String> = (0..5)
        .map(|i| {
            rx.recv_timeout(RECV_TIMEOUT)
                .unwrap_or_else(|_| panic!("missing request {i}"))
        })
        .collect();

    assert!(requests[0].contains("GET /api/v1/auth/public-config"));
    assert!(requests[1].contains("GET /.well-known/openid-configuration"));
    assert!(requests[2].contains("POST /device"));
    assert!(
        requests[2].contains("offline_access"),
        "offline_access is what yields the refresh token: {}",
        requests[2]
    );
    for poll in &requests[3..] {
        assert!(poll.contains("POST /token"), "{poll}");
        assert!(
            poll.contains("device_code=secret-device-code"),
            "the device code must be presented on every poll: {poll}"
        );
    }
    assert!(
        rx.recv_timeout(Duration::from_millis(300)).is_err(),
        "polling must stop as soon as tokens are issued"
    );
}
