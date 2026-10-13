//! Mask a route's key out of a reply — fail closed: every header and every
//! body byte, whatever the content type (text, JSON, event stream, binary)
//! and whatever the size. There is no pass-through branch.
//!
//! | Form of the key masked | Key `ab/c+d=…` found as |
//! |---|---|
//! | raw | `ab/c+d=…` |
//! | percent-encoded (every byte but `A-Z a-z 0-9 - . _ ~`), upper / lower hex | `ab%2Fc%2Bd%3D…` / `ab%2fc%2bd%3d…` |
//! | JSON string (`"` → `\"`, `\` → `\\`), `/` plain or `\/` | `ab/c+d=…` / `ab\/c+d=…` |
//!
//! Forms match in any ASCII case (an upper-cased hex key is still the key).
//! Each copy becomes [`MASK`]. Other transforms (base64 of the key, HTML
//! entities, a truncated copy) are not recognised.
//!
//! [`Masker`] scans a body as it streams: a chunk is emitted at once except
//! a tail that could still start a form (at most the longest form − 1
//! bytes), held until the next chunk or [`Masker::finish`]. A key split
//! across chunks is masked exactly as in one piece; an event stream is not
//! delayed past its `\n\n` (no form starts with a newline).

use crate::{Code, Error};

/// Replacement for a key found in a reply.
pub const MASK: &str = "[REDACTED]";
/// A route key shorter than this cannot be masked (it would cut ordinary
/// words), so the Worker refuses the call before it reaches the upstream.
pub const MASK_MIN_SECRET: usize = 8;

/// Every form of `secret` a reply is scanned for: distinct, longest first.
pub fn forms(secret: &str) -> Vec<Vec<u8>> {
    let raw = secret.as_bytes();
    let mut out = vec![
        raw.to_vec(),
        percent(raw, b"0123456789ABCDEF"),
        percent(raw, b"0123456789abcdef"),
        json(raw, false),
        json(raw, true),
    ];
    out.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    out.dedup();
    out
}

fn percent(raw: &[u8], hex: &[u8; 16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() * 3);
    for &b in raw {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b);
        } else {
            out.extend_from_slice(&[b'%', hex[(b >> 4) as usize], hex[(b & 15) as usize]]);
        }
    }
    out
}

fn json(raw: &[u8], escape_slash: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() * 2);
    for &b in raw {
        match b {
            b'"' | b'\\' => out.extend_from_slice(&[b'\\', b]),
            b'/' if escape_slash => out.extend_from_slice(b"\\/"),
            _ => out.push(b),
        }
    }
    out
}

/// Streaming masker for one reply. Feed body chunks to [`push`](Self::push)
/// in order and send what it returns; then send [`finish`](Self::finish).
/// The concatenated output equals [`mask_bytes`](Self::mask_bytes) of the
/// whole body, however it was chunked.
#[derive(Debug, Clone)]
pub struct Masker {
    /// [`forms`], lower-cased and distinct, longest first; compared
    /// ignoring ASCII case.
    forms: Vec<Vec<u8>>,
    /// `first[b]`: some form starts with byte `b`, either case (skip the
    /// rest fast).
    first: [bool; 256],
    tail: Vec<u8>,
    hits: usize,
}

enum Step {
    /// A form matches here; its length.
    Match(usize),
    /// The bytes from here could still start a form: wait for more.
    Hold,
    /// No form starts here.
    Pass,
}

impl Masker {
    /// A masker for `secret`. A key shorter than [`MASK_MIN_SECRET`] is a
    /// Worker misconfiguration: it could not be masked safely.
    pub fn new(secret: &str) -> Result<Self, Error> {
        if secret.len() < MASK_MIN_SECRET {
            return Err(Error::new(
                Code::Misconfigured,
                format!(
                    "the route's Worker secret is shorter than {MASK_MIN_SECRET} characters — \
                     it could not be masked out of replies"
                ),
            ));
        }
        let mut forms: Vec<Vec<u8>> = forms(secret)
            .iter()
            .map(|f| f.to_ascii_lowercase())
            .collect();
        forms.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        forms.dedup();
        let mut first = [false; 256];
        for f in &forms {
            first[f[0].to_ascii_lowercase() as usize] = true;
            first[f[0].to_ascii_uppercase() as usize] = true;
        }
        Ok(Self {
            forms,
            first,
            tail: Vec::new(),
            hits: 0,
        })
    }

    /// Copies masked in the body so far.
    pub fn hits(&self) -> usize {
        self.hits
    }

    /// Bytes held back, waiting for the next chunk (≤ longest form − 1).
    pub fn held(&self) -> usize {
        self.tail.len()
    }

    /// Longest form, in bytes.
    pub fn longest(&self) -> usize {
        self.forms[0].len()
    }

    /// Mask the next body chunk; returns the bytes safe to send now.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.tail);
        buf.extend_from_slice(chunk);
        let (out, held_from, n) = self.scan(&buf, false);
        self.hits += n;
        self.tail = buf[held_from..].to_vec();
        out
    }

    /// End of the body: the held tail, masked.
    pub fn finish(&mut self) -> Vec<u8> {
        let buf = std::mem::take(&mut self.tail);
        let (out, _, n) = self.scan(&buf, true);
        self.hits += n;
        out
    }

    /// One-shot mask of a whole buffer (a header value); `self` unchanged.
    pub fn mask_bytes(&self, bytes: &[u8]) -> (Vec<u8>, usize) {
        let (out, _, n) = self.scan(bytes, true);
        (out, n)
    }

    /// [`mask_bytes`](Self::mask_bytes) on text.
    pub fn mask_text(&self, text: &str) -> (String, usize) {
        let (out, n) = self.mask_bytes(text.as_bytes());
        // Forms are whole UTF-8 strings, so a match never splits a char.
        (String::from_utf8_lossy(&out).into_owned(), n)
    }

    /// Reply headers to send back: every value masked; a header whose name
    /// holds a form of the key (the runtime lowercases names) dropped;
    /// `content-length` / `content-encoding` dropped (the body is re-emitted
    /// decoded, and masking may change its length). Returns the headers and
    /// the copies masked (a dropped name counts one).
    pub fn reply_headers<I>(&self, headers: I) -> (Vec<(String, String)>, usize)
    where
        I: IntoIterator<Item = (String, String)>,
    {
        let mut out = Vec::new();
        let mut hits = 0;
        for (name, value) in headers {
            let lower = name.to_ascii_lowercase();
            if lower == "content-length" || lower == "content-encoding" {
                continue;
            }
            if self.mask_bytes(lower.as_bytes()).1 > 0 {
                hits += 1;
                continue;
            }
            let (value, n) = self.mask_text(&value);
            hits += n;
            out.push((name, value));
        }
        (out, hits)
    }

    fn step(&self, buf: &[u8], i: usize, last: bool) -> Step {
        let rest = &buf[i..];
        // Longest first, so a longer form wins over a shorter one at `i`.
        for f in &self.forms {
            if f.len() <= rest.len() {
                if rest[..f.len()].eq_ignore_ascii_case(f) {
                    return Step::Match(f.len());
                }
            } else if !last && f[..rest.len()].eq_ignore_ascii_case(rest) {
                return Step::Hold;
            }
        }
        Step::Pass
    }

    /// Mask `buf` left to right; returns (output, index of the first held
    /// byte = `buf.len()` when nothing is held, copies masked). With `last`
    /// nothing is held.
    fn scan(&self, buf: &[u8], last: bool) -> (Vec<u8>, usize, usize) {
        let mut out = Vec::with_capacity(buf.len());
        let (mut i, mut run, mut hits) = (0, 0, 0);
        while i < buf.len() {
            if self.first[buf[i] as usize] {
                match self.step(buf, i, last) {
                    Step::Match(n) => {
                        out.extend_from_slice(&buf[run..i]);
                        out.extend_from_slice(MASK.as_bytes());
                        i += n;
                        run = i;
                        hits += 1;
                        continue;
                    }
                    Step::Hold => break,
                    Step::Pass => {}
                }
            }
            i += 1;
        }
        out.extend_from_slice(&buf[run..i]);
        (out, i, hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk-or-v1-0123456789abcdef";
    /// A base64-style key: `/`, `+`, `=` give it distinct encoded forms.
    const B64: &str = "YXBwOnNlY3JldA/x+y==";

    fn has(hay: &[u8], needle: &str) -> bool {
        hay.windows(needle.len()).any(|w| w == needle.as_bytes())
    }

    /// Mask `body` pushed as `chunks` pieces (split points), then finish.
    fn streamed(key: &str, body: &[u8], splits: &[usize]) -> (Vec<u8>, usize) {
        let mut m = Masker::new(key).unwrap();
        let mut out = Vec::new();
        let mut at = 0;
        for &s in splits {
            out.extend(m.push(&body[at..s]));
            assert!(m.held() < m.longest(), "tail is bounded");
            at = s;
        }
        out.extend(m.push(&body[at..]));
        out.extend(m.finish());
        (out, m.hits())
    }

    /// Deterministic pseudo-random bytes (xorshift), every value 0..=255.
    fn noise(len: usize, mut seed: u64) -> Vec<u8> {
        (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                (seed >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn forms_cover_raw_percent_and_json() {
        let f = forms(B64);
        for want in [
            "YXBwOnNlY3JldA/x+y==",
            "YXBwOnNlY3JldA%2Fx%2By%3D%3D",
            "YXBwOnNlY3JldA%2fx%2by%3d%3d",
            "YXBwOnNlY3JldA\\/x+y==",
        ] {
            assert!(f.contains(&want.as_bytes().to_vec()), "{want}");
        }
        assert_eq!(f.len(), 4, "the plain JSON form equals raw here");
        assert!(f.windows(2).all(|w| w[0].len() >= w[1].len()));
        // A key with no reserved byte has one form.
        assert_eq!(forms(KEY), vec![KEY.as_bytes().to_vec()]);
    }

    #[test]
    fn short_keys_are_refused() {
        let e = Masker::new("1234567").unwrap_err();
        assert_eq!(e.code, Code::Misconfigured);
        assert!(Masker::new("").is_err());
        assert!(Masker::new("12345678").is_ok());
    }

    #[test]
    fn body_over_2_mib_masked_near_the_end() {
        let mut body = vec![b'a'; 3 * 1024 * 1024];
        let at = body.len() - 100;
        body[at..at + KEY.len()].copy_from_slice(KEY.as_bytes());
        let splits: Vec<usize> = (1..body.len() / 65_536).map(|i| i * 65_536).collect();
        let (out, n) = streamed(KEY, &body, &splits);
        assert_eq!(n, 1);
        assert!(!has(&out, KEY));
        assert!(has(&out[out.len() - 120..], MASK));
        assert_eq!(out.len(), body.len() - KEY.len() + MASK.len());
    }

    #[test]
    fn event_stream_masked_and_not_delayed() {
        let mut m = Masker::new(KEY).unwrap();
        let ev = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
        assert_eq!(m.push(ev), ev.to_vec(), "a clean event goes out whole");
        assert_eq!(m.held(), 0);
        let leak = format!("data: {{\"echo\":\"Bearer {KEY}\"}}\n\n");
        let (a, b) = leak.as_bytes().split_at(30);
        let mut out = m.push(a);
        out.extend(m.push(b));
        assert_eq!(m.held(), 0, "the event's end is not held");
        out.extend(m.finish());
        assert_eq!(out, b"data: {\"echo\":\"Bearer [REDACTED]\"}\n\n".to_vec());
        assert_eq!(m.hits(), 1);
    }

    #[test]
    fn binary_body_masked_other_bytes_kept() {
        let mut body = noise(200_000, 0x9e37_79b9_7f4a_7c15);
        body.splice(5_000..5_000, KEY.bytes());
        body.splice(150_000..150_000, B64.bytes());
        let one = Masker::new(KEY).unwrap().mask_bytes(&body);
        assert_eq!(one.1, 1);
        assert!(!has(&one.0, KEY));
        let mut want = body.clone();
        want.splice(5_000..5_000 + KEY.len(), MASK.bytes());
        assert_eq!(one.0, want, "every other byte (0x00..0xff) unchanged");
        let (out, n) = streamed(KEY, &body, &[1, 4_999, 5_010, 99_999, 150_003]);
        assert_eq!((out, n), one);
    }

    #[test]
    fn key_split_at_every_chunk_boundary() {
        for key in [KEY, B64] {
            for form in forms(key) {
                let mut body = b"{\"url\":\"https://x/?k=".to_vec();
                body.extend_from_slice(&form);
                body.extend_from_slice(b"\",\"again\":\"");
                body.extend_from_slice(&form);
                body.extend_from_slice(b"\"}");
                let whole = Masker::new(key).unwrap().mask_bytes(&body);
                assert_eq!(whole.1, 2);
                for s in 0..=body.len() {
                    assert_eq!(streamed(key, &body, &[s]), whole, "split {s}");
                    for t in s..=body.len() {
                        assert_eq!(streamed(key, &body, &[s, t]), whole, "{s} {t}");
                    }
                }
                let bytes: Vec<usize> = (1..body.len()).collect();
                assert_eq!(streamed(key, &body, &bytes), whole, "byte at a time");
                let s = String::from_utf8_lossy(&whole.0);
                assert!(!s.contains(key) && s.matches(MASK).count() == 2, "{s}");
            }
        }
    }

    #[test]
    fn reply_headers_masked() {
        let m = Masker::new(B64).unwrap();
        let (h, n) = m.reply_headers([
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Content-Length".to_string(), "42".to_string()),
            ("Content-Encoding".to_string(), "gzip".to_string()),
            ("X-Echo".to_string(), format!("Basic {B64}")),
            (
                "Location".to_string(),
                "https://x/cb?k=YXBwOnNlY3JldA%2Fx%2By%3D%3D".to_string(),
            ),
            // A key as a header name: the runtime lowercases it.
            (B64_NAME.to_string(), "1".to_string()),
        ]);
        assert_eq!(n, 3);
        assert_eq!(
            h,
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("X-Echo".to_string(), "Basic [REDACTED]".to_string()),
                (
                    "Location".to_string(),
                    "https://x/cb?k=[REDACTED]".to_string()
                ),
            ]
        );
        let m = Masker::new("TokenAbcdEFGH12").unwrap();
        let (h, n) = m.reply_headers([("x-tokenabcdefgh12-seen".to_string(), "y".to_string())]);
        assert_eq!((h.len(), n), (0, 1), "name matched in any case");
    }

    /// A header-name-shaped key form (base64 `/+=` are not name chars).
    const B64_NAME: &str = "x-YXBwOnNlY3JldA%2Fx%2By%3D%3D";

    #[test]
    fn no_false_positives() {
        let m = Masker::new(KEY).unwrap();
        let body = noise(100_000, 7);
        assert_eq!(m.mask_bytes(&body), (body.clone(), 0));
        let splits: Vec<usize> = (1..100).map(|i| i * 997).collect();
        assert_eq!(streamed(KEY, &body, &splits), (body.clone(), 0));
        // A body ending in a key prefix: held, then sent unchanged.
        let prefix = format!("tail sk-or-v1-0123 and {}", &KEY[..KEY.len() - 1]);
        let mut s = Masker::new(KEY).unwrap();
        let mut out = s.push(prefix.as_bytes());
        assert!(s.held() > 0);
        out.extend(s.finish());
        assert_eq!((out, s.hits()), (prefix.as_bytes().to_vec(), 0));
        let (h, n) = m.reply_headers([("Server".to_string(), "nginx".to_string())]);
        assert_eq!((h, n), (vec![("Server".into(), "nginx".into())], 0));
        assert_eq!(m.mask_text("short k"), ("short k".to_string(), 0));
    }

    #[test]
    fn any_case_is_masked() {
        let m = Masker::new(KEY).unwrap();
        let upper = KEY.to_ascii_uppercase();
        let body = format!("{{\"error\":\"INVALID KEY {upper}\",\"k\":\"{KEY}\"}}");
        let (out, n) = m.mask_bytes(body.as_bytes());
        assert_eq!(n, 2);
        assert_eq!(
            out,
            b"{\"error\":\"INVALID KEY [REDACTED]\",\"k\":\"[REDACTED]\"}"
        );
        let split = upper.len() / 2 + 22;
        assert_eq!(streamed(KEY, body.as_bytes(), &[split]), (out, 2));
    }

    #[test]
    fn invalid_utf8_survives() {
        let body = [b"\xff\xfe x ".as_slice(), KEY.as_bytes(), b" y\xc3"].concat();
        let (out, n) = Masker::new(KEY).unwrap().mask_bytes(&body);
        assert_eq!(n, 1);
        assert_eq!(out, b"\xff\xfe x [REDACTED] y\xc3".to_vec());
    }
}
