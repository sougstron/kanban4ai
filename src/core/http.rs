//! The one way anything in kanban4ai talks to HTTPS: a generated config
//! file piped into `curl -K -`.
//!
//! Piping the request — URL, headers, and any body — through curl's stdin
//! rather than its argv keeps the dependency set unchanged (no TLS stack is
//! linked into the crate) and keeps bearer tokens and request bodies out of
//! the process command line, where `ps` would show them. Current callers:
//! the provider-limits fetches in [`crate::core::limits`] and the update
//! check in [`crate::core::update`].
//!
//! `curl` is an optional runtime dependency: a missing binary, a network
//! failure, or a non-2xx status all surface as [`HttpError`], which callers
//! degrade to a note instead of an error.

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::Value;

/// Timeout for every outbound request, in seconds.
pub(crate) const HTTP_TIMEOUT_SECS: u32 = 15;

/// `GET url` through curl. See [`http_request_json`].
pub(crate) fn http_get_json(url: &str, headers: &[(&str, String)]) -> Result<Value, HttpError> {
    http_request_json(url, headers, None)
}

/// One HTTP request through curl, with headers and any request body passed on
/// stdin so secrets stay out of the command line (a refresh token in `ps`
/// output would be as good as the credential file itself). A body makes it a
/// POST. Returns the parsed JSON body, or the HTTP status when the request
/// completed with a non-2xx code.
pub(crate) fn http_request_json(
    url: &str,
    headers: &[(&str, String)],
    body: Option<&str>,
) -> Result<Value, HttpError> {
    let mut config = String::new();
    config.push_str(&format!("url = {}\n", quote_curl(url)));
    for (name, value) in headers {
        config.push_str(&format!(
            "header = {}\n",
            quote_curl(&format!("{name}: {value}"))
        ));
    }
    if let Some(body) = body {
        config.push_str(&format!("data = {}\n", quote_curl(body)));
    }
    config.push_str("silent\n");
    config.push_str("show-error\n");
    config.push_str("location\n");
    config.push_str(&format!("max-time = {HTTP_TIMEOUT_SECS}\n"));
    config.push_str("write-out = \"\\n%{http_code}\"\n");

    let mut child = Command::new("curl")
        .arg("-K")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| HttpError::Transport(format!("curl unavailable: {err}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(config.as_bytes())
            .map_err(|err| HttpError::Transport(format!("curl input failed: {err}")))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|err| HttpError::Transport(format!("curl failed: {err}")))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        return Err(HttpError::Transport(
            message
                .lines()
                .next_back()
                .unwrap_or("request failed")
                .trim()
                .to_string(),
        ));
    }
    let body = String::from_utf8_lossy(&output.stdout).to_string();
    let (payload, status) = split_status(&body);
    if !(200..300).contains(&status) {
        return Err(HttpError::Status(status));
    }
    serde_json::from_str(payload).map_err(|err| HttpError::Transport(format!("bad JSON: {err}")))
}

/// Timeout for release-archive downloads, in seconds. [`HTTP_TIMEOUT_SECS`]
/// covers small API payloads; an archive is a few megabytes.
pub(crate) const DOWNLOAD_TIMEOUT_SECS: u64 = 300;

/// The curl config for a file download, factored out for tests. `fail` turns
/// HTTP errors (404, 5xx) into a non-zero curl exit instead of silently
/// writing the error page to `dest`.
fn download_config(url: &str, dest: &std::path::Path) -> String {
    let mut config = String::new();
    config.push_str(&format!("url = {}\n", quote_curl(url)));
    config.push_str(&format!(
        "output = {}\n",
        quote_curl(&dest.to_string_lossy())
    ));
    config.push_str("silent\nshow-error\nlocation\nfail\n");
    config.push_str(&format!("max-time = {DOWNLOAD_TIMEOUT_SECS}\n"));
    config
}

/// `GET url` into `dest` through the same curl config pipe as
/// [`http_request_json`] — one dependency set, one convention. Returns the
/// curl error text on failure; the caller owns `dest` and cleans it up.
pub(crate) fn http_download(url: &str, dest: &std::path::Path) -> Result<(), HttpError> {
    // Unit tests must never dial out: cargo test --locked stays network-free.
    if cfg!(test) {
        return Err(HttpError::Transport(
            "download skipped: network disabled under cfg(test)".to_string(),
        ));
    }
    let config = download_config(url, dest);
    let mut child = Command::new("curl")
        .arg("-K")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| HttpError::Transport(format!("curl unavailable: {err}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(config.as_bytes())
            .map_err(|err| HttpError::Transport(format!("curl input failed: {err}")))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|err| HttpError::Transport(format!("curl failed: {err}")))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        return Err(HttpError::Transport(
            message
                .lines()
                .next_back()
                .unwrap_or("download failed")
                .trim()
                .to_string(),
        ));
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) enum HttpError {
    Status(u16),
    Transport(String),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Status(code) => write!(f, "HTTP {code}"),
            Self::Transport(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for HttpError {}

/// curl's config parser reads double-quoted values with backslash escapes.
fn quote_curl(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Split a curl body written with a trailing `\n%{http_code}` into the payload
/// and the status code.
fn split_status(body: &str) -> (&str, u16) {
    match body.rsplit_once('\n') {
        Some((payload, status)) => (payload, status.trim().parse().unwrap_or(0)),
        None => (body, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curl_status_and_quoting_survive_odd_payloads() {
        assert_eq!(split_status("{\"a\":1}\n200"), ("{\"a\":1}", 200));
        assert_eq!(split_status("no status"), ("no status", 0));
        assert_eq!(quote_curl("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn download_config_names_url_output_and_fails_on_http_errors() {
        let config = download_config("https://example.com/a.tar.gz", std::path::Path::new("/x/y"));
        assert!(
            config.contains("url = \"https://example.com/a.tar.gz\"\n"),
            "{config}"
        );
        assert!(config.contains("output = \"/x/y\"\n"), "{config}");
        assert!(config.contains("fail\n"), "{config}");
    }

    #[test]
    fn download_never_dials_out_under_test() {
        let err = http_download(
            "https://example.com/x.tar.gz",
            std::path::Path::new("/tmp/x"),
        )
        .unwrap_err();
        assert!(matches!(err, HttpError::Transport(_)), "{err:?}");
    }
}
