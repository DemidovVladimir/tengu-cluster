//! `KeyVault` Durable Object: owns the one 32-byte seed every key is derived
//! from. Made on first use with the Workers RNG, kept in the object's SQLite
//! storage, never returned to anything but this Worker (a Durable Object is
//! reachable only through its binding, not from the internet).

use base64::Engine;
use worker::*;

const SEED_KEY: &str = "seed_v1";

#[durable_object(fetch)]
pub struct KeyVault {
    state: State,
}

impl DurableObject for KeyVault {
    fn new(state: State, _env: Env) -> Self {
        Self { state }
    }

    async fn fetch(&self, req: Request) -> Result<Response> {
        if req.path() != "/seed" {
            return Response::error("not found", 404);
        }
        // A Durable Object runs one event at a time, so get-or-create is atomic.
        let storage = self.state.storage();
        if let Some(seed) = storage.get::<String>(SEED_KEY).await? {
            return Response::ok(seed);
        }
        let mut raw = [0u8; 32];
        getrandom::getrandom(&mut raw).map_err(|e| Error::RustError(e.to_string()))?;
        let seed = b64().encode(raw);
        storage.put(SEED_KEY, seed.clone()).await?;
        Response::ok(seed)
    }
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

pub fn decode_seed(s: &str) -> Option<[u8; 32]> {
    b64().decode(s.trim()).ok()?.try_into().ok()
}
