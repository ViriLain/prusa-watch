//! The one place that decides what may appear in logs, CLI output and API diagnostics.
//!
//! Two layers, both applied at every export boundary:
//!   1. Structural: every URL in free text keeps only its scheme and host, and
//!      `Bearer`/`Basic` credentials and `token=`-style pairs are masked. This needs
//!      no configuration, so it also runs where errors are created ([`text`]).
//!   2. Literal: the values the configuration marks as secret (passwords, tokens,
//!      notification topics, webhook URLs, camera credentials) are masked wherever
//!      they appear ([`Redactor`]).
//!
//! `main` installs the configured [`Redactor`] for the log writer, so a message that
//! reaches `tracing` is scrubbed even if its call site forgot to.

use std::io::Write;
use std::sync::{Arc, LazyLock, RwLock};

use regex::Regex;

use crate::config::Config;

pub const MASK: &str = "***";

/// Literal secrets at least this long are masked anywhere, even inside longer words.
/// Shorter ones (a topic called `core`) are masked only as whole words, so they do not
/// chew holes in unrelated text.
const SUBSTRING_MIN_LEN: usize = 8;

/// Values shorter than this are not usable as a literal filter (`channel=1` would mask
/// every "1"). They are still hidden where they sit: in URLs and `key=value` pairs.
const LITERAL_MIN_LEN: usize = 4;

static URL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)[a-z][a-z0-9+.-]*://\S+").unwrap());
static BEARER_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(bearer|basic)\s+[^\s,;]+").unwrap());
/// `token=…`, `"api_key": "…"`, `WEB_TOKEN: …`, `cap=…`: a credential-looking name, then its value.
static PAIR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)([a-z0-9_.-]*(?:token|password|passwd|secret|api[-_]?key)|\bcap)("?\s*[=:]\s*)("(?:[^"\\]|\\.)*"|'[^']*'|[^\s,;&]+)"#,
    )
    .unwrap()
});
/// `key=value` inside a URL path (`/user=admin&password=x&channel=1`).
static PATH_PAIR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"=[^&/;]*").unwrap());

/// Closing punctuation that belongs to the sentence around a URL, not to the URL.
const TRAILING: &[char] = &[')', ']', '}', '>', '"', '\'', ',', '.', ';', ':', '!', '?'];

/// A URL taken apart for redaction.
struct Parts<'a> {
    /// empty for a bare `user:pass@host` address
    scheme: &'a str,
    userinfo: &'a str,
    host: &'a str,
    /// path, query and fragment
    tail: &'a str,
    /// An '@' appears after the host part. Either the path or query really contains one, or
    /// the credentials contain an unescaped `/`, `?` or `#` and what looks like the host is
    /// the start of the password. Nothing can tell these apart, so nothing of it is shown.
    ambiguous: bool,
}

fn split_address<'a>(scheme: &'a str, rest: &'a str) -> Parts<'a> {
    let (authority, tail) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    let (userinfo, host) = match authority.rfind('@') {
        Some(at) => (&authority[..at], &authority[at + 1..]),
        None => ("", authority),
    };
    Parts {
        scheme,
        userinfo,
        host,
        tail,
        ambiguous: tail.contains('@'),
    }
}

fn split_url(url: &str) -> Option<Parts<'_>> {
    let (scheme, rest) = url.split_once("://")?;
    Some(split_address(scheme, rest))
}

impl Parts<'_> {
    /// Scheme and host, with credentials masked; just the scheme when ambiguous.
    fn origin(&self) -> String {
        let scheme = if self.scheme.is_empty() {
            String::new()
        } else {
            format!("{}://", self.scheme)
        };
        if self.ambiguous {
            return format!("{scheme}{MASK}");
        }
        let user = if self.userinfo.is_empty() { "" } else { "***@" };
        format!("{scheme}{user}{}", self.host)
    }

    fn display(&self) -> String {
        if self.ambiguous {
            return self.origin();
        }
        let (path, extra) = match self.tail.find(['?', '#']) {
            Some(at) => (&self.tail[..at], "?***"),
            None => (self.tail, ""),
        };
        let path = PATH_PAIR_RE.replace_all(path, "=***");
        format!("{}{path}{extra}", self.origin())
    }
}

/// A configured URL for display: credentials, query and `key=value` path parts masked;
/// host and path kept. `rtsp://user:pass@cam/live?key=1` becomes `rtsp://***@cam/live?***`.
pub fn url_credentials(url: &str) -> String {
    split_url(url).map_or_else(|| url.to_string(), |parts| parts.display())
}

/// Like [`url_credentials`], for a setting that also accepts a bare `host[:port]`
/// (`printer.host`), where someone may have written `user:pass@host`.
pub fn host_credentials(host: &str) -> String {
    match split_url(host) {
        Some(parts) => parts.display(),
        None if host.contains('@') => split_address("", host).display(),
        None => host.to_string(),
    }
}

/// A URL found in free text: only the scheme and host survive, because for notification
/// services the path *is* the credential (ntfy topics, Discord and Home Assistant hooks).
fn url_in_text(url: &str) -> String {
    let trimmed = url.trim_end_matches(TRAILING);
    let suffix = &url[trimmed.len()..];
    let Some(parts) = split_url(trimmed) else {
        return url.to_string();
    };
    let rest = if parts.ambiguous || parts.tail.is_empty() {
        ""
    } else if parts.tail == "/" {
        "/"
    } else {
        "/***"
    };
    format!("{}{rest}{suffix}", parts.origin())
}

/// Structural redaction of free text. Safe to call anywhere; needs no configuration.
pub fn text(input: &str) -> String {
    let out = URL_RE.replace_all(input, |c: &regex::Captures<'_>| url_in_text(&c[0]));
    let out = BEARER_RE.replace_all(&out, "$1 ***");
    PAIR_RE.replace_all(&out, "$1$2***").into_owned()
}

fn word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn mask_literal(input: &str, secret: &str) -> String {
    if secret.is_empty() || !input.contains(secret) {
        return input.to_string();
    }
    if secret.len() >= SUBSTRING_MIN_LEN {
        return input.replace(secret, MASK);
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(at) = rest.find(secret) {
        let end = at + secret.len();
        let before = rest[..at].chars().next_back().is_some_and(word_char);
        let after = rest[end..].chars().next().is_some_and(word_char);
        out.push_str(&rest[..at]);
        out.push_str(if before || after { secret } else { MASK });
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn percent_decoded(value: &str) -> Option<String> {
    if !value.contains('%') {
        return None;
    }
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = (bytes[i] == b'%')
            .then(|| bytes.get(i + 1..i + 3))
            .flatten()
            .and_then(|pair| std::str::from_utf8(pair).ok())
            .and_then(|pair| u8::from_str_radix(pair, 16).ok());
        match hex {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok().filter(|decoded| decoded != value)
}

/// The forms a value takes inside a URL: fully escaped, and as the HTTP client writes it
/// into a path, a query and the credentials (each escapes a different set of characters).
fn url_forms(value: &str) -> Vec<String> {
    let mut strict = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            strict.push(byte as char);
        } else {
            strict.push_str(&format!("%{byte:02X}"));
        }
    }
    let mut forms = vec![strict];
    if let Ok(mut url) = reqwest::Url::parse("http://h/") {
        if let Ok(mut segments) = url.path_segments_mut() {
            segments.push(value);
        }
        forms.push(url.path().trim_start_matches('/').to_string());
        url.query_pairs_mut().append_pair("k", value);
        forms.push(url.query().unwrap_or_default().trim_start_matches("k=").to_string());
        if url.set_password(Some(value)).is_ok() {
            forms.push(url.password().unwrap_or_default().to_string());
        }
    }
    forms.retain(|form| form != value);
    forms
}

/// Path words that belong to a hook API's own vocabulary, not to one installation.
const HOOK_VOCABULARY: [&str; 6] = ["webhook", "webhooks", "services", "incoming", "triggers", "workflows"];

/// A hook URL's path segment that identifies this installation.
fn looks_secret(segment: &str) -> bool {
    segment.len() >= SUBSTRING_MIN_LEN && !HOOK_VOCABULARY.contains(&segment.to_lowercase().as_str())
}

/// Knows this installation's secret values and removes them from anything exported.
#[derive(Default)]
pub struct Redactor {
    /// longest first, so a secret that contains another is masked whole
    secrets: Vec<String>,
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Redactor({} secrets)", self.secrets.len())
    }
}

impl Redactor {
    pub fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        let mut secrets: Vec<String> = secrets
            .into_iter()
            .flat_map(|secret| {
                // A secret shows up raw, as written in a URL, or as a client re-encodes it.
                let decoded = percent_decoded(&secret);
                let mut forms = url_forms(decoded.as_deref().unwrap_or(&secret));
                forms.extend(decoded);
                forms.push(secret);
                forms
            })
            .filter(|secret| secret.trim().len() >= LITERAL_MIN_LEN)
            .collect();
        secrets.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        secrets.dedup();
        Self { secrets }
    }

    /// Every value the configuration treats as a credential or a private address.
    pub fn from_config(cfg: &Config) -> Self {
        let mut secrets = vec![
            cfg.printer.password.clone(),
            cfg.web.token.clone(),
            cfg.notify.ntfy.token.clone(),
            cfg.notify.ntfy.topic.clone(),
            cfg.notify.ntfy.reply_topic.clone(),
            cfg.notify.discord.webhook_url.clone(),
            cfg.notify.webhook.url.clone(),
            cfg.health.heartbeat_url.clone(),
            cfg.health.heartbeat_fail_url.clone(),
        ];
        // Hook URLs carry their secret in the path; the others in credentials, the query
        // and `key=value` path parts.
        for (url, path_is_secret) in [
            (&cfg.camera.url, false),
            (&cfg.printer.host, false),
            (&cfg.notify.ntfy.url, false),
            (&cfg.web.public_url, false),
            (&cfg.notify.discord.webhook_url, true),
            (&cfg.notify.webhook.url, true),
            (&cfg.health.heartbeat_url, true),
            (&cfg.health.heartbeat_fail_url, true),
        ] {
            let bare = std::ptr::eq(url, &cfg.printer.host) && url.contains('@');
            let Some(parts) = split_url(url).or_else(|| bare.then(|| split_address("", url))) else {
                continue;
            };
            let mut credentials = vec![parts.userinfo];
            if parts.ambiguous {
                // The other reading: everything up to the last '@' is the credentials.
                let rest = url.split_once("://").map_or(url.as_str(), |(_, rest)| rest);
                credentials.push(&rest[..rest.rfind('@').unwrap_or(0)]);
            }
            for userinfo in credentials {
                secrets.push(userinfo.to_string());
                if let Some((user, password)) = userinfo.split_once(':') {
                    secrets.extend([user.to_string(), password.to_string()]);
                }
            }
            let (path, query) = parts.tail.split_once(['?', '#']).unwrap_or((parts.tail, ""));
            secrets.extend(
                query
                    .split(['&', '#', ';'])
                    .chain(path.split(['/', '&', ';']))
                    .filter_map(|pair| pair.split_once('='))
                    .map(|(_, value)| value.to_string()),
            );
            if path_is_secret {
                secrets.extend(
                    path.split('/')
                        .filter(|segment| looks_secret(segment))
                        .map(str::to_string),
                );
            }
        }
        Self::new(secrets)
    }

    /// Mask configured secret values only. For structured output whose non-secret
    /// addresses (camera host, dashboard URL) should stay readable.
    pub fn literals(&self, input: &str) -> String {
        let mut out = input.to_string();
        for secret in &self.secrets {
            out = mask_literal(&out, secret);
        }
        out
    }

    /// Mask configured secrets, then apply the structural rules of [`text`].
    pub fn text(&self, input: &str) -> String {
        text(&self.literals(input))
    }

    /// Redact every string in a JSON document in place (keys are left alone).
    pub fn json(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(s) => *s = self.text(s),
            serde_json::Value::Array(items) => items.iter_mut().for_each(|item| self.json(item)),
            serde_json::Value::Object(map) => map.values_mut().for_each(|item| self.json(item)),
            _ => {}
        }
    }
}

static ACTIVE: RwLock<Option<Arc<Redactor>>> = RwLock::new(None);

/// Make `redactor` the process-wide scrubber for log output.
pub fn install(redactor: Arc<Redactor>) {
    *ACTIVE.write().unwrap_or_else(|e| e.into_inner()) = Some(redactor);
}

/// Redact with the installed [`Redactor`]; structural rules only before one is installed.
pub fn active(input: &str) -> String {
    match ACTIVE.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
        Some(redactor) => redactor.text(input),
        None => text(input),
    }
}

/// `tracing` writer that scrubs each formatted event before it reaches stderr.
#[derive(Clone, Copy, Default)]
pub struct LogWriter;

/// Collects one event; the scrubbed text is written to `out` when the event is complete.
pub struct LogLine<W: Write> {
    event: Vec<u8>,
    out: W,
}

impl<W: Write> LogLine<W> {
    pub fn new(out: W) -> Self {
        Self {
            event: Vec::with_capacity(256),
            out,
        }
    }
}

impl<W: Write> Write for LogLine<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.event.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !self.event.is_empty() {
            let line = active(&String::from_utf8_lossy(&self.event));
            self.event.clear();
            self.out.write_all(line.as_bytes())?;
        }
        self.out.flush()
    }
}

impl<W: Write> Drop for LogLine<W> {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogWriter {
    type Writer = LogLine<std::io::Stderr>;

    fn make_writer(&'a self) -> Self::Writer {
        LogLine::new(std::io::stderr())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_in_text_keep_only_scheme_and_host() {
        assert_eq!(
            text("could not open rtsp://admin:hunter2@10.0.0.5:554/live?key=abc: Connection refused"),
            "could not open rtsp://***@10.0.0.5:554/***: Connection refused"
        );
        assert_eq!(
            text("error sending request for url (https://ntfy.sh/my-private-topic)"),
            "error sending request for url (https://ntfy.sh/***)"
        );
        assert_eq!(text("GET http://printer/ ok"), "GET http://printer/ ok");
        // an unescaped '@' or ')' inside a password must not end the match early
        assert_eq!(text("rtsp://u:p@ss)w@cam/live"), "rtsp://***@cam/***");
        // a '/' inside the password makes the host unknowable: nothing but the scheme is kept
        assert_eq!(text("rtsp://u:pa/ss@cam/live"), "rtsp://***");
    }

    #[test]
    fn credentials_outside_urls_are_masked() {
        assert_eq!(text("Authorization: Bearer tk_abcdef"), "Authorization: Bearer ***");
        assert_eq!(text("retry with token=abc123&id=4"), "retry with token=***&id=4");
        assert_eq!(text("X-Api-Key: s3cret, next"), "X-Api-Key: ***, next");
        assert_eq!(
            text("web.token is required for a non-loopback web.host"),
            "web.token is required for a non-loopback web.host"
        );
    }

    #[test]
    fn credentials_with_url_special_characters_never_show_a_prefix() {
        for (url, shown, password) in [
            (
                "rtsp://admin:abc@At-Secret-77@cam/live",
                "rtsp://***@cam/live",
                "abc@At-Secret-77",
            ),
            // '#', '?' or '/' in a password: nothing after the scheme can be trusted
            (
                "rtsp://admin:abc#Sharp-Secret-77@cam/live",
                "rtsp://***",
                "abc#Sharp-Secret-77",
            ),
            (
                "rtsp://admin:abc?Query-Secret-77@cam/live",
                "rtsp://***",
                "abc?Query-Secret-77",
            ),
            (
                "rtsp://admin:abc/Slash-Secret-77@cam/live",
                "rtsp://***",
                "abc/Slash-Secret-77",
            ),
            // ... and the same shape read the other way: an '@' inside the query or path
            (
                "rtsp://cam/live?user=admin&password=p@Query-Secret-77",
                "rtsp://***",
                "p@Query-Secret-77",
            ),
            (
                "rtsp://cam/user=admin&password=p@Path-Secret-77",
                "rtsp://***",
                "p@Path-Secret-77",
            ),
            (
                "rtsp://cam/user=admin&password=PathPass-55&channel=1",
                "rtsp://cam/user=***&password=***&channel=***",
                "PathPass-55",
            ),
        ] {
            assert_eq!(url_credentials(url), shown, "{url}");
            let in_text = text(&format!("could not open {url}: refused"));
            for secret in ["Secret-77", "abc", "PathPass", "admin", "p@"] {
                assert!(!in_text.contains(secret), "{url} -> {in_text}");
            }
            // and the password is known on its own, whichever way the URL is read
            let mut cfg = Config::default();
            cfg.camera.url = url.into();
            let redactor = Redactor::from_config(&cfg);
            assert_eq!(
                redactor.text(&format!("login with {password} failed")),
                "login with *** failed",
                "{url}"
            );
        }
        assert_eq!(
            url_credentials("rtsp://[fe80::1]:554/live"),
            "rtsp://[fe80::1]:554/live"
        );
        // a bare address is only read that way where one is expected (printer.host)
        assert_eq!(host_credentials("admin:HostPass-Q7@127.0.0.1:9"), "***@127.0.0.1:9");
        assert_eq!(host_credentials("192.168.1.50"), "192.168.1.50");
        assert_eq!(host_credentials("http://u:p@192.168.1.50"), "http://***@192.168.1.50");
        assert_eq!(url_credentials("videos/clip@2x.avi"), "videos/clip@2x.avi");
        let mut cfg = Config::default();
        cfg.camera.url = "videos/clip@2x.avi".into();
        cfg.printer.host = "operator:HostPass-Q7@127.0.0.1:9".into();
        let redactor = Redactor::from_config(&cfg);
        assert_eq!(
            redactor.text("HostPass-Q7 then operator then videos/clip"),
            "*** then *** then videos/clip"
        );
        // a value too short to be told from ordinary text is not turned into a filter
        cfg.camera.url = "rtsp://cam/user=viewer&password=PathPass-55&channel=1".into();
        let redactor = Redactor::from_config(&cfg);
        assert_eq!(
            redactor.text("frame 1 of 12 from 127.0.0.1"),
            "frame 1 of 12 from 127.0.0.1"
        );
    }

    #[test]
    fn structural_rules_cover_glued_and_prefixed_forms() {
        assert_eq!(text("url_rtsp://u:p@h/x"), "url_rtsp://***@h/***");
        assert_eq!(
            text("\u{1b}[32mhttp://u:p@h/x\u{1b}[0m done"),
            "\u{1b}[32mhttp://***@h/*** done"
        );
        assert_eq!(
            text("access_token=abc WEB_TOKEN=def auth_token: ghi"),
            "access_token=*** WEB_TOKEN=*** auth_token: ***"
        );
        assert_eq!(text(r#"{"token":"abc","n":1}"#), r#"{"token":***,"n":1}"#);
        assert_eq!(text(r#"{"token":"ab\"cd","n":1}"#), r#"{"token":***,"n":1}"#);
        // prose about credentials is left alone
        assert_eq!(
            text("tokens: 5, the password is required"),
            "tokens: 5, the password is required"
        );
    }

    #[test]
    fn secrets_are_masked_in_the_form_a_client_sends_them() {
        // Built by the same URL type the HTTP client uses, so these are the real wire forms.
        for secret in ["my topic+x", "p+ss w0rd!", "weird/topic#1", "ünï-topic"] {
            let redactor = Redactor::new([secret.to_string()]);
            let mut url = reqwest::Url::parse("https://ntfy.example/").unwrap();
            url.path_segments_mut().unwrap().push(secret);
            url.query_pairs_mut().append_pair("auth", secret);
            url.set_password(Some(secret)).unwrap();
            let wire_forms = [
                url.path().trim_start_matches('/').to_string(),
                url.query().unwrap().trim_start_matches("auth=").to_string(),
                url.password().unwrap().to_string(),
            ];
            for wire in wire_forms {
                assert_eq!(
                    redactor.literals(&format!("GET <{wire}> failed")),
                    "GET <***> failed",
                    "{secret:?} on the wire as {wire:?}"
                );
            }
        }
    }

    #[test]
    fn a_hook_url_does_not_make_ordinary_words_secret() {
        let mut cfg = Config::default();
        cfg.notify.discord.webhook_url =
            "https://discord.example/api/webhooks/123456789012345678/AbCdEfGhIjKlMnOpQrStUvWx".into();
        let redactor = Redactor::from_config(&cfg);
        assert_eq!(redactor.literals("webhooks are configured"), "webhooks are configured");
        assert_eq!(
            redactor.literals("id 123456789012345678 key AbCdEfGhIjKlMnOpQrStUvWx"),
            "id *** key ***"
        );
        // a hook id made only of letters is still this installation's secret
        cfg.notify.webhook.url = "https://ha.local/api/webhook/printer_failed_hook".into();
        let redactor = Redactor::from_config(&cfg);
        assert_eq!(redactor.literals("id printer_failed_hook unknown"), "id *** unknown");
        assert_eq!(redactor.literals("the webhook api"), "the webhook api");
        // printing the redactor itself never prints what it protects
        let debug = format!("{redactor:?}");
        assert!(debug.starts_with("Redactor(") && !debug.contains("AbCdEf"), "{debug}");
    }

    #[test]
    fn configured_urls_keep_their_path() {
        assert_eq!(
            url_credentials("rtsp://user:pass@192.168.1.51/live"),
            "rtsp://***@192.168.1.51/live"
        );
        assert_eq!(url_credentials("http://cam/stream?key=abc"), "http://cam/stream?***");
        assert_eq!(url_credentials("/videos/clip.avi"), "/videos/clip.avi");
    }

    #[test]
    fn short_secrets_only_match_whole_words() {
        let redactor = Redactor::new(["core".to_string(), "long-secret-value".to_string()]);
        assert_eq!(redactor.text("core-one saw core"), "core-one saw ***");
        assert_eq!(redactor.text("xlong-secret-valuex"), "x***x");
    }

    #[test]
    fn log_lines_are_scrubbed_when_the_event_is_complete() {
        install(Arc::new(Redactor::new(["installed-log-secret".to_string()])));
        let mut sink = Vec::new();
        {
            // one event may arrive in several writes; a secret can straddle them
            let mut line = LogLine::new(&mut sink);
            line.write_all(b"WARN upload to https://ntfy.sh/private-topic failed, key installed-")
                .unwrap();
            line.write_all(b"log-secret rejected\n").unwrap();
        }
        assert_eq!(
            String::from_utf8(sink).unwrap(),
            "WARN upload to https://ntfy.sh/*** failed, key *** rejected\n"
        );
    }

    #[test]
    fn percent_encoded_credentials_are_masked_in_both_forms() {
        let mut cfg = Config::default();
        cfg.camera.url = "rtsp://viewer:p%40ssw0rd-9@cam/live".into();
        let redactor = Redactor::from_config(&cfg);
        assert_eq!(redactor.text("login p@ssw0rd-9 failed"), "login *** failed");
        assert_eq!(redactor.text("login p%40ssw0rd-9 failed"), "login *** failed");
    }
}
