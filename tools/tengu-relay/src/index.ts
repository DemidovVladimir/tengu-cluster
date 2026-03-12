/**
 * Tengu Relay — Cloudflare Worker
 *
 * LEGACY: This is the old KV session bridge for the wallet page flow.
 * It will be rewritten as a stateless API key injection proxy.
 *
 * Planned rewrite:
 *   /molecule/* → forward to Molecule GraphQL API, inject x-api-key from Worker secret
 *   /poi/*      → forward to POI registration API, inject Authorization Bearer from Worker secret
 *   /beach/*    → forward to Beach.science API, inject Authorization Bearer from Worker secret
 *
 * The agent will only need TENGU_RELAY_URL + TENGU_RELAY_TOKEN.
 * All sensitive API keys (MOLECULE_API_KEY, POI_API_KEY, BEACH_SCIENCE_API_KEY)
 * will live as Cloudflare Worker secrets, never in the agent's process env.
 *
 * Current (legacy) endpoints — KV bridge, will be removed:
 *   PUT  /s/:session/:key   — write a value (body = JSON)
 *   GET  /s/:session/:key   — read a value
 *   DELETE /s/:session/:key — delete a value
 */

interface Env {
  RELAY: KVNamespace;
}

const TTL = 300; // 5 minutes
const CORS = {
  "Access-Control-Allow-Origin": "*",
  "Access-Control-Allow-Methods": "GET, PUT, DELETE, OPTIONS",
  "Access-Control-Allow-Headers": "Content-Type",
};

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    if (request.method === "OPTIONS") {
      return new Response(null, { status: 204, headers: CORS });
    }

    const url = new URL(request.url);
    const match = url.pathname.match(/^\/s\/([a-zA-Z0-9_-]+)\/([a-zA-Z0-9_-]+)$/);
    if (!match) {
      return json({ error: "Not found. Use /s/:session/:key" }, 404);
    }

    const [, session, key] = match;
    const kvKey = `${session}:${key}`;

    switch (request.method) {
      case "PUT": {
        const body = await request.text();
        if (!body) return json({ error: "Empty body" }, 400);
        await env.RELAY.put(kvKey, body, { expirationTtl: TTL });
        return json({ ok: true });
      }

      case "GET": {
        const value = await env.RELAY.get(kvKey);
        if (value === null) {
          return json({ pending: false }, 200);
        }
        return new Response(value, {
          headers: { "Content-Type": "application/json", ...CORS },
        });
      }

      case "DELETE": {
        await env.RELAY.delete(kvKey);
        return json({ ok: true });
      }

      default:
        return json({ error: "Method not allowed" }, 405);
    }
  },
};

function json(data: unknown, status = 200): Response {
  return new Response(JSON.stringify(data), {
    status,
    headers: { "Content-Type": "application/json", ...CORS },
  });
}
