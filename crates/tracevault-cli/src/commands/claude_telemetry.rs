//! Claude Code OpenTelemetry export to TraceVault (VIS-1081): `init
//! --claude-telemetry` and `tracevault otel-headers`.
//!
//! Claude Code reports its own cost per API call (`api_request.cost_usd`) only
//! over OpenTelemetry. `init --claude-telemetry` points its log exporter at the
//! server's `/otlp/v1/logs` receiver, in the USER-level `~/.claude/settings.json`
//! (Claude Code ignores OTel exporter variables in a repository's settings).
//! The bearer token is never written there: `otelHeadersHelper` runs
//! `tracevault otel-headers`, which prints it from the credential store.
//!
//! Claude Code applies the helper's headers to every OTLP exporter it runs, so
//! an existing exporter setup (the user's own collector, metrics, a static
//! header) is never merged with ours: it would receive TraceVault's token.
//! Such a setup is left untouched and the collector route is printed instead.

use crate::api_client::ApiClient;
use crate::credentials::{same_server, Credential};
use serde_json::{json, Value};
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

pub const ENABLE_TELEMETRY: &str = "CLAUDE_CODE_ENABLE_TELEMETRY";
pub const LOGS_EXPORTER: &str = "OTEL_LOGS_EXPORTER";
pub const LOGS_PROTOCOL: &str = "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL";
pub const LOGS_ENDPOINT: &str = "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT";
/// How often Claude Code re-runs the headers helper. Its default (29 minutes)
/// outlives a Keycloak access token (5 minutes), so the header would go stale.
pub const HELPER_INTERVAL: &str = "CLAUDE_CODE_OTEL_HEADERS_HELPER_DEBOUNCE_MS";
pub const HEADERS_HELPER: &str = "otelHeadersHelper";

/// Path of the server's OTLP logs receiver (VIS-1079).
const LOGS_PATH: &str = "/otlp/v1/logs";
/// Every 3 minutes, in milliseconds (see [`HELPER_INTERVAL`]).
const HELPER_INTERVAL_MS: &str = "180000";
/// The validity `otel-headers` guarantees a token has: one helper interval
/// plus a minute of slack, so the header outlives the gap to the next run.
pub const HEADER_MIN_VALID_SECS: i64 = 180 + 60;
/// What our helper command starts with. Anything else is the user's own.
const HELPER_PREFIX: &str = "tracevault otel-headers";

/// `<server>/otlp/v1/logs`.
pub fn logs_endpoint(server_url: &str) -> String {
    format!("{}{LOGS_PATH}", server_url.trim_end_matches('/'))
}

/// The `otelHeadersHelper` command. It names the server so `otel-headers`
/// can refuse to hand a token for another instance to this endpoint.
pub fn helper_command(server_url: &str) -> String {
    format!(
        "{HELPER_PREFIX} --server-url {}",
        server_url.trim_end_matches('/')
    )
}

/// The server URL must be an http(s) URL that is safe to put unquoted into
/// the helper's command line, which Claude Code runs through a shell.
fn validate_server_url(server_url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(server_url)
        .map_err(|e| format!("invalid TraceVault server URL `{server_url}`: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!(
            "TraceVault server URL `{server_url}` must be http:// or https://"
        ));
    }
    let shell_safe = server_url
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || ":/._~%-".contains(c));
    if !shell_safe {
        return Err(format!(
            "TraceVault server URL `{server_url}` contains characters that cannot go into \
             Claude Code's otelHeadersHelper command"
        ));
    }
    Ok(())
}

fn is_our_helper(value: &Value) -> bool {
    value.as_str().is_some_and(|s| s.starts_with(HELPER_PREFIX))
}

fn as_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Exporter settings that are not TraceVault's, each described for the user
/// (header values are never printed). `settings` is the parsed
/// `~/.claude/settings.json`; `process_env` the `OTEL_*` variables of the
/// environment `init`/`status` runs in, which Claude Code also sees when
/// started from it. An endpoint counts as ours when it is `expected`, or the
/// receiver of the server our helper names (an install for another server,
/// which a re-run replaces). Detailed beta tracing is foreign too: with it,
/// Claude Code sends its logs to `BETA_TRACING_ENDPOINT` instead.
pub fn foreign_exporter_settings(
    settings: &Value,
    process_env: &[(String, String)],
    expected_endpoint: Option<&str>,
) -> Vec<String> {
    let helper = settings.get(HEADERS_HELPER);
    let helper_endpoint = helper
        .and_then(Value::as_str)
        .and_then(|h| h.strip_prefix(HELPER_PREFIX))
        .and_then(|rest| rest.trim().strip_prefix("--server-url "))
        .map(|server| logs_endpoint(server.trim()));
    let mut reasons = Vec::new();
    if let Some(h) = helper.filter(|h| !is_our_helper(h)) {
        reasons.push(format!(
            "{HEADERS_HELPER} = `{}` (in ~/.claude/settings.json)",
            as_text(h)
        ));
    }

    let settings_env = settings
        .get("env")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.clone(), as_text(v), "~/.claude/settings.json"));
    let shell_env = process_env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone(), "the shell environment"));
    for (key, value, source) in settings_env.chain(shell_env) {
        let foreign = match key.as_str() {
            LOGS_EXPORTER => value != "otlp",
            "OTEL_METRICS_EXPORTER" | "OTEL_TRACES_EXPORTER" => {
                !matches!(value.as_str(), "" | "none")
            }
            LOGS_ENDPOINT => {
                expected_endpoint != Some(value.as_str())
                    && helper_endpoint.as_deref() != Some(value.as_str())
            }
            "BETA_TRACING_ENDPOINT" => true,
            k if k.starts_with("OTEL_EXPORTER_OTLP_") && k.ends_with("ENDPOINT") => true,
            k if k.starts_with("OTEL_EXPORTER_OTLP_") && k.ends_with("HEADERS") => {
                reasons.push(format!("{key} is set (in {source})"));
                continue;
            }
            _ => false,
        };
        if foreign {
            reasons.push(format!("{key}={value} (in {source})"));
        }
    }
    reasons
}

/// The `OTEL_*` (and beta tracing) variables of this process's environment.
pub fn otel_process_env() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(k, _)| k.starts_with("OTEL_") || k == "BETA_TRACING_ENDPOINT")
        .collect()
}

/// What `init --claude-telemetry` did to `~/.claude/settings.json`.
#[derive(Debug, PartialEq, Eq)]
pub enum InstallOutcome {
    /// Written (or already exactly so) for this server.
    Configured,
    /// Another exporter setup is present; nothing was changed.
    Foreign(Vec<String>),
}

/// Bring `settings` to export Claude Code's logs to `server_url`, unless
/// another exporter setup is present (then nothing is changed). Every key we
/// do not own is kept; a helper interval the user set is theirs to keep.
pub fn merge_telemetry(
    settings: &mut Value,
    server_url: &str,
    process_env: &[(String, String)],
) -> io::Result<InstallOutcome> {
    let endpoint = logs_endpoint(server_url);
    let root = settings.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "~/.claude/settings.json is not a JSON object",
        )
    })?;
    if root.get("env").is_some_and(|e| !e.is_object()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "~/.claude/settings.json \"env\" is not a JSON object",
        ));
    }
    let reasons =
        foreign_exporter_settings(&Value::Object(root.clone()), process_env, Some(&endpoint));
    if !reasons.is_empty() {
        return Ok(InstallOutcome::Foreign(reasons));
    }

    let env = root
        .entry("env")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("checked above");
    for (key, value) in [
        (ENABLE_TELEMETRY, "1"),
        (LOGS_EXPORTER, "otlp"),
        (LOGS_PROTOCOL, "http/protobuf"),
        (LOGS_ENDPOINT, endpoint.as_str()),
    ] {
        env.insert(key.to_string(), json!(value));
    }
    env.entry(HELPER_INTERVAL)
        .or_insert_with(|| json!(HELPER_INTERVAL_MS));
    root.insert(
        HEADERS_HELPER.to_string(),
        json!(helper_command(server_url)),
    );
    Ok(InstallOutcome::Configured)
}

/// [`merge_telemetry`] applied to `<claude_dir>/settings.json` (created if
/// absent, written atomically, untouched when already current or foreign).
pub fn install(
    claude_dir: &Path,
    server_url: &str,
    process_env: &[(String, String)],
) -> io::Result<InstallOutcome> {
    validate_server_url(server_url).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    fs::create_dir_all(claude_dir)?;
    let base = claude_dir.canonicalize()?;
    let path = crate::commands::init::safe_join(&base, "settings.json")?;
    let mut outcome = InstallOutcome::Configured;
    crate::commands::init::update_json_file(&path, "Claude Code telemetry", |root| {
        outcome = merge_telemetry(root, server_url, process_env)?;
        Ok(())
    })?;
    Ok(outcome)
}

/// Telemetry as `status` reports it.
#[derive(Debug, PartialEq, Eq)]
pub enum TelemetryState {
    /// No TraceVault telemetry settings.
    Off,
    /// TraceVault's settings, with whatever is missing or wrong in them.
    Ours {
        endpoint: Option<String>,
        missing: Vec<&'static str>,
    },
    /// Another exporter setup; `ours_too` when our helper is also installed,
    /// i.e. TraceVault's token goes to those exporters as well.
    Foreign {
        reasons: Vec<String>,
        ours_too: bool,
    },
}

pub fn inspect(
    settings: &Value,
    process_env: &[(String, String)],
    expected_endpoint: Option<&str>,
) -> TelemetryState {
    let helper_ours = settings.get(HEADERS_HELPER).is_some_and(is_our_helper);
    let reasons = foreign_exporter_settings(settings, process_env, expected_endpoint);
    if !reasons.is_empty() {
        return TelemetryState::Foreign {
            reasons,
            ours_too: helper_ours,
        };
    }
    let env = settings.get("env");
    let get = |key: &str| env.and_then(|e| e.get(key)).map(as_text);
    let endpoint = get(LOGS_ENDPOINT);
    if !helper_ours && endpoint.is_none() {
        return TelemetryState::Off;
    }
    let mut missing = Vec::new();
    if get(ENABLE_TELEMETRY).as_deref() != Some("1") {
        missing.push(ENABLE_TELEMETRY);
    }
    if get(LOGS_EXPORTER).as_deref() != Some("otlp") {
        missing.push(LOGS_EXPORTER);
    }
    if get(LOGS_PROTOCOL).as_deref() != Some("http/protobuf") {
        missing.push(LOGS_PROTOCOL);
    }
    if endpoint.is_none() {
        missing.push(LOGS_ENDPOINT);
    }
    if !helper_ours {
        missing.push(HEADERS_HELPER);
    }
    TelemetryState::Ours { endpoint, missing }
}

/// What to do instead when Claude Code telemetry already goes elsewhere: send
/// a copy of the `api_request` events to TraceVault from the user's own
/// OpenTelemetry Collector.
pub fn collector_instructions(server_url: &str, reasons: &[String]) -> String {
    let server = server_url.trim_end_matches('/');
    let mut out = String::from(
        "Claude Code telemetry is already exported elsewhere, so TraceVault left \
         ~/.claude/settings.json unchanged:\n",
    );
    for r in reasons {
        out.push_str(&format!("  - {r}\n"));
    }
    out.push_str(&format!(
        "To also send Claude Code's cost to TraceVault, forward its `api_request` events from \
         your OpenTelemetry Collector (authenticate with a TraceVault API key):\n\
         \n\
         processors:\n\
         \x20 filter/tracevault:\n\
         \x20   error_mode: ignore\n\
         \x20   logs:\n\
         \x20     log_record:\n\
         \x20       - 'attributes[\"event.name\"] != \"api_request\"'\n\
         exporters:\n\
         \x20 otlphttp/tracevault:\n\
         \x20   endpoint: {server}/otlp\n\
         \x20   headers:\n\
         \x20     Authorization: \"Bearer ${{env:TRACEVAULT_API_KEY}}\"\n\
         service:\n\
         \x20 pipelines:\n\
         \x20   logs/tracevault:\n\
         \x20     receivers: [otlp]\n\
         \x20     processors: [filter/tracevault]\n\
         \x20     exporters: [otlphttp/tracevault]\n"
    ));
    out
}

/// The server telemetry is exported to: the one the resolved credential
/// belongs to. `flag` (`init --server-url`) must agree with it, since
/// `otel-headers` will only ever hand out that credential.
pub fn telemetry_server_url(project_root: &Path, flag: Option<&str>) -> Result<String, String> {
    let (resolved, credential) =
        crate::credentials::resolve_credentials_quiet(project_root).map_err(|e| e.to_string())?;
    if credential.is_none() {
        return Err(not_logged_in(flag.or(resolved.as_deref())));
    }
    match (flag, resolved.as_deref()) {
        (Some(flag), Some(resolved)) if !same_server(flag, resolved) => Err(format!(
            "you are logged in to {resolved}, not {flag}: Claude Code telemetry can only be \
             authenticated with the credential of the server it is sent to. Run \
             `tracevault login --server-url {flag}` first."
        )),
        (Some(url), _) | (None, Some(url)) => Ok(url.trim_end_matches('/').to_string()),
        (None, None) => Err(
            "no TraceVault server URL configured: run `tracevault login --server-url <URL>` \
             or set TRACEVAULT_SERVER_URL"
                .to_string(),
        ),
    }
}

fn not_logged_in(url: Option<&str>) -> String {
    format!(
        "Not logged in to TraceVault, so Claude Code telemetry cannot be authenticated. Run \
         `tracevault login --server-url {}` (or set TRACEVAULT_API_KEY).",
        url.unwrap_or("<URL>")
    )
}

/// Ask whether to enable telemetry, during an interactive `init` (stdin is a
/// TTY) that did not pass `--claude-telemetry`. Defaults to no: the server
/// must run the OTLP receiver (VIS-1079) before exports are accepted.
pub fn prompt() -> io::Result<bool> {
    if !io::stdin().is_terminal() {
        return Ok(false);
    }
    let mut stdout = io::stdout().lock();
    write!(
        stdout,
        "Also send Claude Code's per-request cost to TraceVault over OpenTelemetry \
         (writes ~/.claude/settings.json)? [y/N]: "
    )?;
    stdout.flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
}

/// Whether `<claude_dir>/settings.json` has no TraceVault or other telemetry
/// configured yet, i.e. whether the interactive prompt has anything to offer.
pub fn is_off(claude_dir: &Path) -> bool {
    let settings = fs::read_to_string(claude_dir.join("settings.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or_else(|| json!({}));
    inspect(&settings, &otel_process_env(), None) == TelemetryState::Off
}

/// `tracevault otel-headers`: the JSON object Claude Code's
/// `otelHeadersHelper` expects, `{"Authorization": "Bearer …"}`, built from
/// the credential that resolves in `cwd`. With `server_url` (what `init`
/// writes into the helper command) that credential must belong to that
/// server, so a later login elsewhere never sends its token to this endpoint.
pub async fn otel_headers(cwd: &Path, server_url: Option<&str>) -> Result<String, String> {
    let project_root = crate::paths::resolve_project_root(cwd).root;
    let (resolved, credential) =
        crate::credentials::resolve_credentials_quiet(&project_root).map_err(|e| e.to_string())?;
    let Some(credential) = credential else {
        return Err(not_logged_in(server_url.or(resolved.as_deref())));
    };
    if let (Some(wanted), Some(resolved)) = (server_url, resolved.as_deref()) {
        if !same_server(wanted, resolved) {
            return Err(format!(
                "the TraceVault credential is for {resolved}, but Claude Code telemetry is \
                 configured for {wanted}. Run `tracevault login --server-url {wanted}`, or \
                 `tracevault init --claude-telemetry` to export to {resolved} instead."
            ));
        }
    }
    let token = match &credential {
        Credential::ApiKey(key) => key.clone(),
        Credential::Keycloak(_) => {
            let base = server_url.or(resolved.as_deref()).unwrap_or_default();
            ApiClient::with_credential(base, Some(credential))
                .bearer_valid_for(HEADER_MIN_VALID_SECS)
                .await
                .map_err(|e| e.to_string())?
                .unwrap_or_default()
        }
    };
    Ok(json!({ "Authorization": format!("Bearer {token}") }).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: &str = "https://tv.example.com";

    fn merged(mut settings: Value, env: &[(String, String)]) -> (Value, InstallOutcome) {
        let outcome = merge_telemetry(&mut settings, SERVER, env).unwrap();
        (settings, outcome)
    }

    #[test]
    fn fresh_settings_get_env_and_helper() {
        let (s, outcome) = merged(json!({}), &[]);
        assert_eq!(outcome, InstallOutcome::Configured);
        assert_eq!(
            s,
            json!({
                "env": {
                    "CLAUDE_CODE_ENABLE_TELEMETRY": "1",
                    "OTEL_LOGS_EXPORTER": "otlp",
                    "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL": "http/protobuf",
                    "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT": "https://tv.example.com/otlp/v1/logs",
                    "CLAUDE_CODE_OTEL_HEADERS_HELPER_DEBOUNCE_MS": "180000"
                },
                "otelHeadersHelper": "tracevault otel-headers --server-url https://tv.example.com"
            })
        );
        let text = s.to_string();
        assert!(!text.contains("tvk_") && !text.contains("Bearer"), "{text}");
    }

    #[test]
    fn existing_env_and_other_keys_are_kept() {
        let (s, outcome) = merged(
            json!({
                "model": "opus",
                "hooks": {"Stop": []},
                "env": {"FOO": "bar", "CLAUDE_CODE_OTEL_HEADERS_HELPER_DEBOUNCE_MS": "60000",
                        "OTEL_METRICS_EXPORTER": "none"}
            }),
            &[],
        );
        assert_eq!(outcome, InstallOutcome::Configured);
        assert_eq!(s["model"], "opus");
        assert_eq!(s["hooks"], json!({"Stop": []}));
        assert_eq!(s["env"]["FOO"], "bar");
        assert_eq!(s["env"]["OTEL_METRICS_EXPORTER"], "none");
        // The user's own helper interval wins.
        assert_eq!(s["env"][HELPER_INTERVAL], "60000");
        assert_eq!(s["env"][LOGS_EXPORTER], "otlp");
    }

    #[test]
    fn rerun_is_idempotent() {
        let (once, _) = merged(json!({"a": 1}), &[]);
        let (twice, outcome) = merged(once.clone(), &[]);
        assert_eq!(outcome, InstallOutcome::Configured);
        assert_eq!(once, twice);
    }

    #[test]
    fn our_install_for_another_server_is_replaced() {
        let mut s = json!({});
        merge_telemetry(&mut s, "https://old.example.com", &[]).unwrap();
        let (s, outcome) = merged(s, &[]);
        assert_eq!(outcome, InstallOutcome::Configured);
        assert_eq!(
            s["env"][LOGS_ENDPOINT],
            "https://tv.example.com/otlp/v1/logs"
        );
        assert_eq!(
            s[HEADERS_HELPER],
            "tracevault otel-headers --server-url https://tv.example.com"
        );
    }

    fn assert_left_alone(settings: Value, env: &[(String, String)], mentions: &str) {
        let (after, outcome) = merged(settings.clone(), env);
        assert_eq!(after, settings, "a foreign setup must not be touched");
        match outcome {
            InstallOutcome::Foreign(reasons) => assert!(
                reasons.iter().any(|r| r.contains(mentions)),
                "{reasons:?} should mention {mentions}"
            ),
            other => panic!("expected Foreign, got {other:?}"),
        }
    }

    #[test]
    fn foreign_logs_exporter_is_left_alone() {
        assert_left_alone(
            json!({"env": {"OTEL_LOGS_EXPORTER": "console"}}),
            &[],
            "OTEL_LOGS_EXPORTER=console",
        );
    }

    #[test]
    fn foreign_logs_endpoint_is_left_alone() {
        assert_left_alone(
            json!({"env": {
                "OTEL_LOGS_EXPORTER": "otlp",
                "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT": "https://collector.corp/v1/logs"
            }}),
            &[],
            "https://collector.corp/v1/logs",
        );
    }

    #[test]
    fn generic_endpoint_metrics_and_static_headers_are_foreign() {
        assert_left_alone(
            json!({"env": {"OTEL_EXPORTER_OTLP_ENDPOINT": "http://localhost:4318"}}),
            &[],
            "OTEL_EXPORTER_OTLP_ENDPOINT",
        );
        assert_left_alone(
            json!({"env": {"OTEL_METRICS_EXPORTER": "otlp"}}),
            &[],
            "OTEL_METRICS_EXPORTER",
        );
        assert_left_alone(
            json!({"env": {"OTEL_EXPORTER_OTLP_HEADERS": "Authorization=Bearer secret"}}),
            &[],
            "OTEL_EXPORTER_OTLP_HEADERS is set",
        );
    }

    #[test]
    fn beta_tracing_endpoint_is_foreign() {
        assert_left_alone(
            json!({"env": {"ENABLE_BETA_TRACING_DETAILED": "1",
                           "BETA_TRACING_ENDPOINT": "https://collector.corp"}}),
            &[],
            "BETA_TRACING_ENDPOINT",
        );
        assert_left_alone(
            json!({}),
            &[(
                "BETA_TRACING_ENDPOINT".to_string(),
                "https://collector.corp".to_string(),
            )],
            "BETA_TRACING_ENDPOINT",
        );
    }

    #[test]
    fn a_receiver_path_on_another_host_is_not_ours() {
        // Our helper names tv.example.com; the user then pointed the
        // endpoint at their own collector, which happens to use our path.
        let (mut s, _) = merged(json!({}), &[]);
        s["env"][LOGS_ENDPOINT] = json!("https://collector.corp/otlp/v1/logs");
        assert_left_alone(s, &[], "collector.corp");
    }

    #[test]
    fn foreign_header_values_are_never_reported() {
        let reasons = foreign_exporter_settings(
            &json!({"env": {"OTEL_EXPORTER_OTLP_HEADERS": "Authorization=Bearer secret"}}),
            &[],
            None,
        );
        assert!(reasons.iter().all(|r| !r.contains("secret")), "{reasons:?}");
    }

    #[test]
    fn foreign_headers_helper_is_left_alone() {
        assert_left_alone(
            json!({"otelHeadersHelper": "/usr/local/bin/corp-otel-token"}),
            &[],
            "corp-otel-token",
        );
    }

    #[test]
    fn exporter_in_the_shell_environment_is_foreign() {
        assert_left_alone(
            json!({"model": "opus"}),
            &[(
                "OTEL_EXPORTER_OTLP_ENDPOINT".to_string(),
                "http://collector:4318".to_string(),
            )],
            "the shell environment",
        );
    }

    #[test]
    fn malformed_settings_are_an_error() {
        assert!(merge_telemetry(&mut json!([]), SERVER, &[]).is_err());
        assert!(merge_telemetry(&mut json!({"env": "x"}), SERVER, &[]).is_err());
    }

    #[test]
    fn install_writes_file_and_rerun_keeps_it_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, r#"{"model": "opus", "env": {"FOO": "1"}}"#).unwrap();
        assert_eq!(
            install(dir.path(), SERVER, &[]).unwrap(),
            InstallOutcome::Configured
        );
        let first = fs::read_to_string(&path).unwrap();
        let v: Value = serde_json::from_str(&first).unwrap();
        assert_eq!(v["model"], "opus");
        assert_eq!(v["env"]["FOO"], "1");
        assert_eq!(v["env"][ENABLE_TELEMETRY], "1");
        install(dir.path(), SERVER, &[]).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), first);
    }

    #[test]
    fn install_creates_settings_but_not_for_a_foreign_shell_setup() {
        let dir = tempfile::tempdir().unwrap();
        let env = [("OTEL_LOGS_EXPORTER".to_string(), "console".to_string())];
        assert!(matches!(
            install(dir.path(), SERVER, &env).unwrap(),
            InstallOutcome::Foreign(_)
        ));
        assert!(!dir.path().join("settings.json").exists());
        install(dir.path(), SERVER, &[]).unwrap();
        assert!(dir.path().join("settings.json").exists());
    }

    #[test]
    fn install_rejects_urls_unsafe_for_the_helper_command() {
        let dir = tempfile::tempdir().unwrap();
        for bad in [
            "https://tv.example.com/;rm -rf ~",
            "https://tv.example.com/$(id)",
            "ftp://tv.example.com",
            "not a url",
        ] {
            assert!(install(dir.path(), bad, &[]).is_err(), "{bad}");
        }
        assert!(!dir.path().join("settings.json").exists());
        install(dir.path(), "http://localhost:3000/", &[]).unwrap();
    }

    #[test]
    fn inspect_reports_off_ours_incomplete_and_foreign() {
        assert_eq!(inspect(&json!({}), &[], None), TelemetryState::Off);
        assert_eq!(
            inspect(
                &json!({"env": {"CLAUDE_CODE_ENABLE_TELEMETRY": "1"}}),
                &[],
                None
            ),
            TelemetryState::Off
        );

        let (configured, _) = merged(json!({}), &[]);
        assert_eq!(
            inspect(&configured, &[], None),
            TelemetryState::Ours {
                endpoint: Some("https://tv.example.com/otlp/v1/logs".to_string()),
                missing: vec![],
            }
        );

        let mut incomplete = configured.clone();
        incomplete["env"]
            .as_object_mut()
            .unwrap()
            .remove(LOGS_EXPORTER);
        assert!(matches!(
            inspect(&incomplete, &[], None),
            TelemetryState::Ours { missing, .. } if missing == vec![LOGS_EXPORTER]
        ));

        let mut also_metrics = configured;
        also_metrics["env"]["OTEL_METRICS_EXPORTER"] = json!("otlp");
        assert!(matches!(
            inspect(&also_metrics, &[], None),
            TelemetryState::Foreign { ours_too: true, .. }
        ));
    }

    #[test]
    fn collector_instructions_name_the_receiver_and_filter() {
        let text = collector_instructions("https://tv.example.com/", &["X=1".to_string()]);
        assert!(
            text.contains("endpoint: https://tv.example.com/otlp\n"),
            "{text}"
        );
        assert!(text.contains("\"api_request\""), "{text}");
        assert!(text.contains("  - X=1"), "{text}");
    }
}
