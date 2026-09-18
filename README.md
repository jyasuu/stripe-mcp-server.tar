# Stripe MCP Server (Streamable HTTP)

An MCP server that exposes the Stripe API to MCP clients (Claude, etc.) over
the current **Streamable HTTP** transport, built with the [rmcp](https://docs.rs/rmcp)
Rust SDK (v3.4.0) and [async-http via reqwest].

## Design

Stripe has several hundred REST endpoints. Rather than hand-writing (and
maintaining) a typed tool per endpoint, this server exposes a single tool:

### `stripe_api`

```json
{
  "method": "GET" | "POST" | "DELETE",
  "path": "/v1/customers",
  "params": { "email": "a@b.com", "metadata": { "order_id": "6735" } },
  "stripe_account": "acct_...",      // optional, for Connect
  "idempotency_key": "..."           // optional, for POST/DELETE
}
```

- `params` is a nested JSON object that mirrors Stripe's documented request
  shape. It's automatically flattened into Stripe's bracket-notation wire
  format (`metadata[order_id]=6735`, `expand[0]=customer`, etc.), so the
  model can call *any* current or future Stripe endpoint without server
  changes — full API coverage, not a curated subset.
- GET/DELETE params become query parameters; POST params become the
  form-encoded body, matching how the Stripe API actually works.
- The raw Stripe JSON response (pretty-printed) is returned as the tool
  result, with the HTTP status prefixed. Non-2xx responses are reported as
  tool errors so the model can see and react to Stripe's error payload.

## Auth

Set `STRIPE_SECRET_KEY` (a standard `sk_...` secret key or a restricted
`rk_...` key — restricted keys are recommended so you can scope exactly which
resources this server can touch). The key is read once at startup and used
as a Bearer token on every request; it is never sent back to the client.

## Running

```bash
export STRIPE_SECRET_KEY=sk_test_...   # or rk_...
cargo run --release
# Stripe MCP server listening on http://127.0.0.1:8080/mcp
```

Optional env vars:
- `BIND_ADDRESS` (default `127.0.0.1:8080`) — set to e.g. `0.0.0.0:8080` to
  listen on all interfaces (do this behind a reverse proxy / auth layer, not
  directly on the open internet, since anyone who can reach the port can use
  your Stripe key).
- `RUST_LOG` — tracing log level (default `info`).

## Connecting a client

Point any MCP Streamable HTTP client at `http://<host>:<port>/mcp`. For
Claude/claude.ai-style clients that expect a custom connector, register that
URL as the server endpoint (no separate auth headers needed from the client
side — the Stripe key lives server-side).

## Security notes

- Prefer a **restricted key** (Dashboard → Developers → API keys → Create
  restricted key) scoped to only the resources you want this server to touch
  (e.g. read-only on Customers, no access to Payouts/Transfers), since the
  `stripe_api` tool is intentionally generic and will call whatever path the
  model asks for.
- Put this behind TLS and some form of network/auth restriction before
  exposing it beyond localhost — the MCP Streamable HTTP transport itself
  doesn't authenticate callers.
- Consider running against `sk_test_...` first; nothing in this server
  distinguishes test vs. live mode beyond whichever key you provide.

## Example calls (once connected)

- List customers: `{"method":"GET","path":"/v1/customers","params":{"limit":5}}`
- Create a customer: `{"method":"POST","path":"/v1/customers","params":{"email":"jane@example.com","name":"Jane Doe"}}`
- Create a PaymentIntent: `{"method":"POST","path":"/v1/payment_intents","params":{"amount":1000,"currency":"usd","automatic_payment_methods":{"enabled":true}}}`
- Retrieve balance: `{"method":"GET","path":"/v1/balance","params":{}}`
- Refund a charge: `{"method":"POST","path":"/v1/refunds","params":{"charge":"ch_123"}}`
