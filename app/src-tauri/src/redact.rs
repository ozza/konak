//! The one redactor. Everything Konak returns to the UI or to MCP clients (logs, error text) passes
//! through it. It masks the secret values generated for installed apps (read from their `.env`
//! files inside the distro) and anything that looks like `password=...`, `token: ...`,
//! `scheme://user:password@host`, `Authorization: <scheme> <credentials>`, `?key=...` or a
//! Nextcloud share link `/s/<token>`.

use crate::paths::Paths;
use crate::{catalog, shell};
use regex::Regex;
use std::sync::OnceLock;

pub const MASK: &str = "[redacted]";

pub struct Redactor {
    /// Known secret values, longest first so a value that contains another is masked whole.
    secrets: Vec<String>,
}

/// Each pattern captures (1) the text kept before the secret, (2) the secret, and optionally (3) the
/// text kept after it.
fn patterns() -> &'static [Regex; 5] {
    static P: OnceLock<[Regex; 5]> = OnceLock::new();
    P.get_or_init(|| {
        [
            // key=value / key: value / "key": "value", where the key mentions a secret-ish word.
            Regex::new(r#"(?i)(\b[\w.-]*(?:password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|private[_-]?key)[\w.-]*["']?\s*[:=]\s*["']?)([^\s"'&,;]+)"#).unwrap(),
            // Credentials inside URLs.
            Regex::new(r"(?i)(\b[a-z][a-z0-9+.-]*://[^\s/:@]+:)([^\s/@]+)(@)").unwrap(),
            // HTTP Authorization headers, `Authorization: <scheme> <credentials>`; the scheme stays.
            Regex::new(r#"(?i)(\b(?:proxy-)?authorization["']?\s*[:=]\s*["']?(?:[a-z][\w-]*\s+)?)([^\s"',;]+)"#).unwrap(),
            // A bare `key` query parameter (API keys in URLs).
            Regex::new(r#"(?i)([?&]key=)([^\s&#"']+)"#).unwrap(),
            // Nextcloud public share links, `/s/<token>`.
            Regex::new(r"(/s/)([A-Za-z0-9]{10,})").unwrap(),
        ]
    })
}

/// Secret values from `.env` text, for the given variable names.
pub fn env_values(env: &str, keys: &[&str]) -> Vec<String> {
    env.lines()
        .filter_map(|l| l.split_once('='))
        .filter(|(k, v)| keys.contains(&k.trim()) && !v.trim().is_empty())
        .map(|(_, v)| v.trim().to_string())
        .collect()
}

impl Redactor {
    pub fn new(mut secrets: Vec<String>) -> Self {
        // Very short values would mask ordinary words; generated secrets are 64 hex characters.
        secrets.retain(|s| s.len() >= 6);
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        secrets.dedup();
        Redactor { secrets }
    }

    /// Loads the secret values of every app installed in the distro (one wsl call). When the distro
    /// cannot be read, only the generic patterns apply.
    pub fn load(paths: &Paths) -> Self {
        let keys: Vec<&str> = catalog::all().iter().flat_map(|m| m.secrets.iter().map(String::as_str)).collect();
        if keys.is_empty() {
            return Redactor::new(vec![]);
        }
        let out = shell::distro_script(&paths.distro, "cat /var/lib/konak/apps/*/.env 2>/dev/null || true\n", &[]);
        Redactor::new(if out.ok { env_values(&out.text, &keys) } else { vec![] })
    }

    pub fn apply(&self, text: &str) -> String {
        let mut s = text.to_string();
        for secret in &self.secrets {
            s = s.replace(secret.as_str(), MASK);
        }
        for p in patterns() {
            s = p.replace_all(&s, |c: &regex::Captures| format!("{}{MASK}{}", &c[1], c.get(3).map_or("", |m| m.as_str()))).into_owned();
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_known_values_and_patterns() {
        let env = "KONAK_HOST=x.local\nDB_PASSWORD=0123456789abcdef\nADMIN_TOKEN=feedfacecafe\nSHORT=abc\n";
        let r = Redactor::new(env_values(env, &["DB_PASSWORD", "ADMIN_TOKEN", "SHORT"]));
        let out = r.apply("connecting with 0123456789abcdef; header feedfacecafe; abc stays");
        assert_eq!(out, "connecting with [redacted]; header [redacted]; abc stays");

        let cases = [
            ("password=hunter22 next", "password=[redacted] next"),
            ("DB_PASSWORD: s3cr3t", "DB_PASSWORD: [redacted]"),
            (r#"{"admin_token": "abc123", "user": "bob"}"#, r#"{"admin_token": "[redacted]", "user": "bob"}"#),
            ("?api_key=XYZ&x=1", "?api_key=[redacted]&x=1"),
            ("postgres://immich:pw123@db:5432/immich", "postgres://immich:[redacted]@db:5432/immich"),
            ("Secret = value", "Secret = [redacted]"),
            ("nothing to see here", "nothing to see here"),
            // Authorization headers keep the scheme, lose the credentials.
            ("Authorization: Bearer eyJhbGciOi.abc-def_1", "Authorization: Bearer [redacted]"),
            ("authorization: Basic dXNlcjpwYXNz==", "authorization: Basic [redacted]"),
            (r#"{"Authorization": "token abc123"}"#, r#"{"Authorization": "token [redacted]"}"#),
            ("Proxy-Authorization: Negotiate YIIG", "Proxy-Authorization: Negotiate [redacted]"),
            ("Authorization: opaque123", "Authorization: [redacted]"),
            // A bare key query parameter, not other parameters that end in "key".
            ("GET /api?key=AIzaSyA1b2&format=json", "GET /api?key=[redacted]&format=json"),
            ("/x?a=1&key=abc#frag", "/x?a=1&key=[redacted]#frag"),
            ("?monkey=banana", "?monkey=banana"),
            // Nextcloud share links; short paths stay.
            ("GET /s/Ab3dE6fG9hJ2kLm HTTP/1.1", "GET /s/[redacted] HTTP/1.1"),
            ("https://cloud.local/index.php/s/qwertyuiopASDFG/download", "https://cloud.local/index.php/s/[redacted]/download"),
            ("/s/short", "/s/short"),
        ];
        for (input, want) in cases {
            assert_eq!(Redactor::new(vec![]).apply(input), want, "{input}");
        }
    }
}
