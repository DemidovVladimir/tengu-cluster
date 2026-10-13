//! Minimal SSH-agent client (draft-miller-ssh-agent): list identities and
//! sign. The private key never leaves the agent — with Secretive it stays in
//! the Secure Enclave and each signature may ask for Touch ID.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use tengu_seal::ssh::{put_string, PublicKey};

const SSH_AGENT_FAILURE: u8 = 5;
const SSH_AGENTC_REQUEST_IDENTITIES: u8 = 11;
const SSH_AGENT_IDENTITIES_ANSWER: u8 = 12;
const SSH_AGENTC_SIGN_REQUEST: u8 = 13;
const SSH_AGENT_SIGN_RESPONSE: u8 = 14;

/// A Touch ID prompt waits for the operator.
const SIGN_TIMEOUT_SECS: u64 = 120;

/// One usable agent key (ECDSA P-256 or Ed25519; other kinds are skipped).
#[derive(Debug, Clone)]
pub(crate) struct Identity {
    pub key: PublicKey,
    pub comment: String,
}

/// Every usable key the agent holds.
pub(crate) fn identities(socket: &Path) -> Result<Vec<Identity>> {
    let reply = request(socket, &[SSH_AGENTC_REQUEST_IDENTITIES], 10)?;
    let mut r = Reader(&reply);
    if r.byte()? != SSH_AGENT_IDENTITIES_ANSWER {
        bail!("ssh-agent refused to list identities");
    }
    let n = r.u32()?;
    let mut out = Vec::new();
    for _ in 0..n {
        let blob = r.string()?.to_vec();
        let comment = String::from_utf8_lossy(r.string()?).into_owned();
        if let Ok(key) = PublicKey::from_blob(&blob) {
            out.push(Identity { key, comment });
        }
    }
    Ok(out)
}

/// SSH signature blob (`string alg, string sig`) over `data`.
pub(crate) fn sign(socket: &Path, key: &PublicKey, data: &[u8]) -> Result<Vec<u8>> {
    let mut msg = vec![SSH_AGENTC_SIGN_REQUEST];
    put_string(&mut msg, &key.blob);
    put_string(&mut msg, data);
    msg.extend_from_slice(&0u32.to_be_bytes());
    let reply = request(socket, &msg, SIGN_TIMEOUT_SECS)?;
    let mut r = Reader(&reply);
    match r.byte()? {
        SSH_AGENT_SIGN_RESPONSE => Ok(r.string()?.to_vec()),
        SSH_AGENT_FAILURE => {
            bail!("ssh-agent refused to sign (Touch ID cancelled, or the key is not in this agent)")
        }
        other => bail!("unexpected ssh-agent reply type {other}"),
    }
}

#[cfg(unix)]
fn request(socket: &Path, msg: &[u8], timeout_secs: u64) -> Result<Vec<u8>> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let mut s = UnixStream::connect(socket)
        .with_context(|| format!("connect to ssh-agent at {}", socket.display()))?;
    s.set_read_timeout(Some(Duration::from_secs(timeout_secs)))?;
    s.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut framed = (msg.len() as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(msg);
    s.write_all(&framed).context("write to ssh-agent")?;
    let mut len = [0u8; 4];
    s.read_exact(&mut len).context("read from ssh-agent")?;
    let n = u32::from_be_bytes(len) as usize;
    if n == 0 || n > 256 * 1024 {
        bail!("ssh-agent reply has an invalid length {n}");
    }
    let mut body = vec![0u8; n];
    s.read_exact(&mut body).context("read from ssh-agent")?;
    Ok(body)
}

#[cfg(not(unix))]
fn request(_socket: &Path, _msg: &[u8], _timeout_secs: u64) -> Result<Vec<u8>> {
    bail!("[keys] needs an ssh-agent Unix socket (macOS / Linux)")
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8> {
        let (&b, rest) = self
            .0
            .split_first()
            .ok_or_else(|| anyhow!("truncated ssh-agent reply"))?;
        self.0 = rest;
        Ok(b)
    }

    fn u32(&mut self) -> Result<u32> {
        if self.0.len() < 4 {
            bail!("truncated ssh-agent reply");
        }
        let (n, rest) = self.0.split_at(4);
        self.0 = rest;
        Ok(u32::from_be_bytes([n[0], n[1], n[2], n[3]]))
    }

    fn string(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        if self.0.len() < n {
            bail!("truncated ssh-agent reply");
        }
        let (s, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(s)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    /// A one-key fake agent: answers one identities request and one sign
    /// request with an Ed25519 signature, like `ssh-agent` would.
    fn fake_agent(dir: &Path) -> (std::path::PathBuf, PublicKey) {
        use ed25519_dalek::Signer;
        let sk = ed25519_dalek::SigningKey::from_bytes(&[21u8; 32]);
        let mut blob = Vec::new();
        put_string(&mut blob, b"ssh-ed25519");
        put_string(&mut blob, sk.verifying_key().as_bytes());
        let key = PublicKey::from_blob(&blob).unwrap();
        let path = dir.join("agent.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            for conn in listener.incoming().take(2) {
                let mut c = conn.unwrap();
                let mut len = [0u8; 4];
                c.read_exact(&mut len).unwrap();
                let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
                c.read_exact(&mut body).unwrap();
                let mut reply = Vec::new();
                if body[0] == SSH_AGENTC_REQUEST_IDENTITIES {
                    reply.push(SSH_AGENT_IDENTITIES_ANSWER);
                    reply.extend_from_slice(&1u32.to_be_bytes());
                    put_string(&mut reply, &blob);
                    put_string(&mut reply, b"tengu-test");
                } else {
                    let mut r = Reader(&body[1..]);
                    let _key = r.string().unwrap();
                    let data = r.string().unwrap();
                    let mut sig = Vec::new();
                    put_string(&mut sig, b"ssh-ed25519");
                    put_string(&mut sig, &sk.sign(data).to_bytes());
                    reply.push(SSH_AGENT_SIGN_RESPONSE);
                    put_string(&mut reply, &sig);
                }
                let mut framed = (reply.len() as u32).to_be_bytes().to_vec();
                framed.extend_from_slice(&reply);
                c.write_all(&framed).unwrap();
            }
        });
        (path, key)
    }

    #[test]
    fn lists_and_signs_through_a_fake_agent() {
        let dir = tempfile::tempdir().unwrap();
        let (sock, key) = fake_agent(dir.path());
        let ids = identities(&sock).unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0].comment, "tengu-test");
        assert_eq!(ids[0].key, key);
        let sig = sign(&sock, &key, b"hello").unwrap();
        key.verify(b"hello", &sig).unwrap();
    }

    #[test]
    fn missing_socket_is_a_clear_error() {
        let err = identities(Path::new("/nonexistent/agent.sock")).unwrap_err();
        assert!(format!("{err:#}").contains("connect to ssh-agent"));
    }
}
