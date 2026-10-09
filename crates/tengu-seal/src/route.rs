//! The Worker's two config tables, both plain `wrangler.toml` vars (no
//! secrets in either):
//!
//! ```toml
//! [vars]
//! ROUTES = '''{
//!   "openrouter": { "upstream": "https://openrouter.ai/api", "secret": "OPENROUTER_API_KEY",
//!                   "header": "Authorization: Bearer {secret}",
//!                   "allow": ["v1/chat/completions", "v1/embeddings", "v1/models", "alpha/decisions"] },
//!   "telegram":   { "upstream": "https://api.telegram.org", "secret": "TELEGRAM_BOT_TOKEN",
//!                   "path": ["bot{secret}/", "file/bot{secret}/"] },
//!   "solana-rpc": { "upstream": "https://mainnet.helius-rpc.com", "secret": "HELIUS_API_KEY",
//!                   "query": "api-key" }
//! }'''
//! CLIENTS = '''{
//!   "SHA256:<fingerprint>": { "label": "mac-attended", "session_hours": 24, "routes": ["*"] }
//! }'''
//! ```
//!
//! Each route says exactly where its key goes — one of:
//!
//! | Field | Key goes | Caller sends |
//! |---|---|---|
//! | `header` | `<Name>: <value with {secret}>` | no [`PLACEHOLDER`] anywhere |
//! | `path` | at the start of the path, in one of these templates (`{secret}` once each) | [`PLACEHOLDER`] exactly there, once |
//! | `query` | as the value of this query parameter | `<param>=TENGU_SECRET`, once |
//!
//! So a session holder can never copy a key into another spot (a webhook
//! URL, a message text) — the Worker refuses the request.
//!
//! | Other field | Rule |
//! |---|---|
//! | route name | 1–32 chars `[a-z0-9_-]`, not `healthz` / `session` / `whoami` |
//! | `upstream` | `https://host[:port][/base]`, no query, fragment or userinfo |
//! | `secret` | a Worker secret name `[A-Z0-9_]`, never `SESSION_KEY` |
//! | `allow` | optional path prefixes (after a `path` key part) the route accepts; empty = any |
//! | client key | full OpenSSH fingerprint `SHA256:…` |
//! | `session_hours` | 1–168, default 24 |
//! | `routes` | route names the client may use, or `["*"]`; empty = none; each must exist |

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::{Code, Error};

/// Literal a caller puts where a `path` / `query` route's key belongs
/// (Telegram `bot<token>`, `?api-key=`); the Worker swaps it.
pub const PLACEHOLDER: &str = "TENGU_SECRET";
/// The Worker's own endpoints; never route names.
pub const RESERVED: &[&str] = &["healthz", "session", "whoami"];
/// The Worker secret that signs sessions; never sent upstream.
pub const SESSION_KEY: &str = "SESSION_KEY";
/// Default and ceiling of a client's session lifetime.
pub const DEFAULT_SESSION_HOURS: u64 = 24;
pub const MAX_SESSION_HOURS: u64 = 168;

fn misconfigured(m: impl Into<String>) -> Error {
    Error::new(Code::Misconfigured, m)
}

/// Route names: 1–32 chars of `[a-z0-9_-]`, not a reserved endpoint.
pub fn is_route_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && !RESERVED.contains(&s)
}

fn is_secret_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn is_plain_path(s: &str) -> bool {
    !s.starts_with('/')
        && !s.contains("..")
        && !s.contains("//")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.' | '{' | '}'))
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub upstream: String,
    /// Worker secret holding the key.
    pub secret: String,
    /// Key in a header: `<Name>: <value with {secret}>`.
    #[serde(default)]
    pub header: Option<String>,
    /// Key in the path: allowed path starts, each with `{secret}` once.
    #[serde(default)]
    pub path: Vec<String>,
    /// Key in the query: the parameter name.
    #[serde(default)]
    pub query: Option<String>,
    /// Optional allowed path prefixes (after the key part of a `path` route).
    #[serde(default)]
    pub allow: Vec<String>,
}

/// Where a route's key goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAt<'a> {
    Header(&'a str, &'a str),
    Path(&'a [String]),
    Query(&'a str),
}

impl Route {
    pub fn validate(&self, name: &str) -> Result<(), Error> {
        let bad = |m: &str| misconfigured(format!("route '{name}': {m}"));
        if !is_route_name(name) {
            return Err(bad(
                "name must be 1-32 chars of [a-z0-9_-], not healthz/session/whoami",
            ));
        }
        crate::target::parse_upstream(&self.upstream).map_err(|e| bad(&e.message))?;
        if !is_secret_name(&self.secret) || self.secret == SESSION_KEY {
            return Err(bad(
                "secret must be a Worker secret name of [A-Z0-9_], never SESSION_KEY",
            ));
        }
        let set = usize::from(self.header.is_some())
            + usize::from(!self.path.is_empty())
            + usize::from(self.query.is_some());
        if set != 1 {
            return Err(bad("set exactly one of header, path, query"));
        }
        if self.header.is_some() {
            let (h, _) = self
                .header_parts()
                .ok_or_else(|| bad("header must be '<Name>: <value with {secret}>'"))?;
            crate::target::check_header_name(h).map_err(|e| bad(&e.message))?;
        }
        for t in &self.path {
            if t.matches("{secret}").count() != 1 || !is_plain_path(t) {
                return Err(bad(
                    "each path template needs {secret} once, a relative plain path",
                ));
            }
        }
        if let Some(q) = &self.query {
            let ok = !q.is_empty()
                && q.len() <= 64
                && q.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if !ok {
                return Err(bad("query must be a parameter name of [A-Za-z0-9_-]"));
            }
        }
        for a in &self.allow {
            if a.is_empty() || a.contains("{secret}") || !is_plain_path(a) {
                return Err(bad("allow entries are relative plain path prefixes"));
            }
        }
        Ok(())
    }

    /// `(name, value template)` of [`Route::header`].
    pub fn header_parts(&self) -> Option<(&str, &str)> {
        let (name, value) = self.header.as_deref()?.split_once(':')?;
        let (name, value) = (name.trim(), value.trim());
        (!name.is_empty() && value.matches("{secret}").count() == 1).then_some((name, value))
    }

    /// Where the key goes (valid routes have exactly one).
    pub fn key_at(&self) -> Option<KeyAt<'_>> {
        if let Some((n, v)) = self.header_parts() {
            return Some(KeyAt::Header(n, v));
        }
        if !self.path.is_empty() {
            return Some(KeyAt::Path(&self.path));
        }
        self.query.as_deref().map(KeyAt::Query)
    }
}

/// The `ROUTES` table, every entry validated.
#[derive(Debug, Clone, Default)]
pub struct Routes(BTreeMap<String, Route>);

impl Routes {
    pub fn parse(json: &str) -> Result<Self, Error> {
        let map: BTreeMap<String, Route> = serde_json::from_str(json)
            .map_err(|e| misconfigured(format!("ROUTES is not valid JSON: {e}")))?;
        for (name, route) in &map {
            route.validate(name)?;
        }
        Ok(Self(map))
    }

    pub fn get(&self, name: &str) -> Option<&Route> {
        self.0.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Client {
    pub label: String,
    #[serde(default)]
    pub session_hours: Option<u64>,
    /// Route names this key may use, or `["*"]`; empty = none.
    #[serde(default)]
    pub routes: Vec<String>,
}

impl Client {
    pub fn session_ttl_ms(&self) -> u64 {
        self.session_hours
            .unwrap_or(DEFAULT_SESSION_HOURS)
            .clamp(1, MAX_SESSION_HOURS)
            * 3_600_000
    }

    pub fn allows(&self, route: &str) -> bool {
        self.routes.iter().any(|r| r == "*" || r == route)
    }
}

/// The `CLIENTS` table (OpenSSH fingerprint → client).
#[derive(Debug, Clone, Default)]
pub struct Clients(BTreeMap<String, Client>);

impl Clients {
    /// Parse and check against `routes`: every listed route must exist.
    pub fn parse(json: &str, routes: &Routes) -> Result<Self, Error> {
        let map: BTreeMap<String, Client> = serde_json::from_str(json)
            .map_err(|e| misconfigured(format!("CLIENTS is not valid JSON: {e}")))?;
        for (fp, c) in &map {
            if !fp.starts_with("SHA256:") || fp.len() != "SHA256:".len() + 43 {
                return Err(misconfigured(format!(
                    "CLIENTS key '{fp}' must be a full OpenSSH SHA256: fingerprint"
                )));
            }
            if c.label.is_empty() {
                return Err(misconfigured(format!("CLIENTS '{fp}': empty label")));
            }
            for r in &c.routes {
                if r != "*" && routes.get(r).is_none() {
                    return Err(misconfigured(format!(
                        "CLIENTS '{fp}': route '{r}' is not in ROUTES"
                    )));
                }
            }
        }
        Ok(Self(map))
    }

    pub fn get(&self, fingerprint: &str) -> Option<&Client> {
        self.0.get(fingerprint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `'''…'''` value of `name` in the shipped `wrangler.toml`.
    fn shipped_var(name: &str) -> String {
        let toml = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../cloudflare/seal-worker/wrangler.toml"
        ))
        .unwrap();
        let start = toml
            .find(&format!("{name} = '''"))
            .expect("var in wrangler.toml");
        let body = &toml[start + name.len() + 6..];
        body[..body.find("'''").expect("closing quotes")].to_string()
    }

    #[test]
    fn shipped_wrangler_toml_parses() {
        let routes = Routes::parse(&shipped_var("ROUTES")).unwrap();
        for r in ["openrouter", "telegram", "solana-rpc", "evm-rpc"] {
            assert!(routes.get(r).is_some(), "{r}");
        }
        Clients::parse(&shipped_var("CLIENTS"), &routes).unwrap();
    }

    const FP: &str = "SHA256:/CMhsyI30nZqNgEvX+CAAtPMbvCTpbh4w7azkA12NWw";

    fn table() -> Routes {
        Routes::parse(
            r#"{
              "openrouter": {"upstream": "https://openrouter.ai/api", "secret": "OPENROUTER_API_KEY",
                             "header": "Authorization: Bearer {secret}",
                             "allow": ["v1/chat/completions", "alpha/decisions"]},
              "telegram": {"upstream": "https://api.telegram.org", "secret": "TELEGRAM_BOT_TOKEN",
                           "path": ["bot{secret}/", "file/bot{secret}/"]},
              "solana-rpc": {"upstream": "https://mainnet.helius-rpc.com", "secret": "HELIUS_API_KEY",
                             "query": "api-key"}
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn routes_parse_and_say_where_the_key_goes() {
        let r = table();
        assert_eq!(
            r.names().collect::<Vec<_>>(),
            vec!["openrouter", "solana-rpc", "telegram"]
        );
        assert_eq!(
            r.get("openrouter").unwrap().key_at(),
            Some(KeyAt::Header("Authorization", "Bearer {secret}"))
        );
        assert!(
            matches!(r.get("telegram").unwrap().key_at(), Some(KeyAt::Path(p)) if p.len() == 2)
        );
        assert_eq!(
            r.get("solana-rpc").unwrap().key_at(),
            Some(KeyAt::Query("api-key"))
        );
    }

    #[test]
    fn bad_routes_are_misconfigured() {
        for json in [
            r#"{"session": {"upstream": "https://a.example", "secret": "K", "query": "k"}}"#,
            r#"{"Bad": {"upstream": "https://a.example", "secret": "K", "query": "k"}}"#,
            r#"{"a": {"upstream": "http://a.example", "secret": "K", "query": "k"}}"#,
            r#"{"a": {"upstream": "https://a.example", "secret": "lower", "query": "k"}}"#,
            r#"{"a": {"upstream": "https://a.example", "secret": "SESSION_KEY", "query": "k"}}"#,
            r#"{"a": {"upstream": "https://a.example", "secret": "K"}}"#,
            r#"{"a": {"upstream": "https://a.example", "secret": "K", "query": "k", "path": ["{secret}"]}}"#,
            r#"{"a": {"upstream": "https://a.example", "secret": "K", "header": "x-api-key: fixed"}}"#,
            r#"{"a": {"upstream": "https://a.example", "secret": "K", "header": "Host: {secret}"}}"#,
            r#"{"a": {"upstream": "https://a.example", "secret": "K", "path": ["bot/"]}}"#,
            r#"{"a": {"upstream": "https://a.example", "secret": "K", "path": ["{secret}/{secret}"]}}"#,
            r#"{"a": {"upstream": "https://a.example", "secret": "K", "path": ["../{secret}"]}}"#,
            r#"{"a": {"upstream": "https://a.example", "secret": "K", "query": "a b"}}"#,
            r#"{"a": {"upstream": "https://a.example", "secret": "K", "query": "k", "allow": ["/x"]}}"#,
            r#"{"a": {"upstream": "https://a.example", "secret": "K", "query": "k", "extra": 1}}"#,
            "not json",
        ] {
            let e = Routes::parse(json).unwrap_err();
            assert_eq!(e.code, Code::Misconfigured, "{json}");
        }
    }

    #[test]
    fn clients_parse_allow_and_check_routes() {
        let r = table();
        let c = Clients::parse(
            &format!(
                r#"{{"{FP}": {{"label": "mac", "session_hours": 500, "routes": ["openrouter"]}}}}"#
            ),
            &r,
        )
        .unwrap();
        let mac = c.get(FP).unwrap();
        assert_eq!(mac.session_ttl_ms(), 168 * 3_600_000);
        assert!(mac.allows("openrouter"));
        assert!(!mac.allows("telegram"));
        let none = Client {
            label: "x".into(),
            session_hours: None,
            routes: vec![],
        };
        assert!(!none.allows("telegram"), "empty = none");
        assert_eq!(none.session_ttl_ms(), 24 * 3_600_000);
        let all = Client {
            routes: vec!["*".into()],
            ..none
        };
        assert!(all.allows("telegram"));
        assert!(Clients::parse(r#"{"SHA256:short": {"label": "x"}}"#, &r).is_err());
        assert!(Clients::parse(&format!(r#"{{"{FP}": {{"label": ""}}}}"#), &r).is_err());
        let typo = format!(r#"{{"{FP}": {{"label": "x", "routes": ["open-router"]}}}}"#);
        assert!(Clients::parse(&typo, &r).is_err());
    }
}
