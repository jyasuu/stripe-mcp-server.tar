//! Stripe MCP server, exposed over the MCP Streamable HTTP transport.
//!
//! Rather than hand-writing a typed wrapper for every one of Stripe's few
//! hundred REST endpoints, this server exposes one generic `stripe_api` tool
//! that can call *any* Stripe API path with any method. This gives full API
//! coverage (whatever Stripe supports today, including new endpoints added
//! after this file was written) while keeping the tool surface small enough
//! for a model to reason about.
//!
//! Auth: the Stripe secret key is read once from the `STRIPE_SECRET_KEY`
//! environment variable at startup and used for every request. It is never
//! echoed back to the client.

use std::sync::Arc;

use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars, tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde::Deserialize;
use serde_json::Value;

const STRIPE_API_BASE: &str = "https://api.stripe.com";
const BIND_ADDRESS_ENV: &str = "BIND_ADDRESS";
const DEFAULT_BIND_ADDRESS: &str = "127.0.0.1:8080";

#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
enum HttpMethod {
    Get,
    Post,
    Delete,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct StripeApiRequest {
    /// HTTP method to use for the Stripe API call.
    method: HttpMethod,
    /// API path, starting with /v1/, e.g. "/v1/customers",
    /// "/v1/payment_intents/pi_123", "/v1/checkout/sessions".
    path: String,
    /// Parameters for the call, as a nested JSON object mirroring Stripe's
    /// documented request body / query shape, e.g.
    /// {"email": "a@b.com", "metadata": {"order_id": "6735"}, "expand": ["subscriptions"]}.
    /// For GET/DELETE these become query parameters; for POST they become
    /// the form-encoded request body. Omit or use {} for no parameters.
    #[serde(default)]
    params: Value,
    /// Optional Stripe-Account header value, for making requests on behalf
    /// of a connected account.
    #[serde(default)]
    stripe_account: Option<String>,
    /// Optional Idempotency-Key header value for POST/DELETE requests.
    #[serde(default)]
    idempotency_key: Option<String>,
}

/// Flattens a nested JSON value into Stripe's bracket-notation key/value
/// pairs, e.g. {"metadata": {"order_id": "6735"}} -> [("metadata[order_id]", "6735")]
/// and {"expand": ["a", "b"]} -> [("expand[0]", "a"), ("expand[1]", "b")].
fn flatten_params(prefix: &str, value: &Value, out: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}[{k}]")
                };
                flatten_params(&key, v, out);
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                let key = format!("{prefix}[{i}]");
                flatten_params(&key, v, out);
            }
        }
        Value::Null => {}
        Value::String(s) => out.push((prefix.to_string(), s.clone())),
        Value::Bool(b) => out.push((prefix.to_string(), b.to_string())),
        Value::Number(n) => out.push((prefix.to_string(), n.to_string())),
    }
}

#[derive(Clone)]
struct StripeServer {
    http: reqwest::Client,
    secret_key: Arc<String>,
    tool_router: ToolRouter<StripeServer>,
}

#[tool_router]
impl StripeServer {
    fn new(secret_key: String) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("failed to build reqwest client"),
            secret_key: Arc::new(secret_key),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Call the Stripe API. Works against any Stripe REST endpoint \
        (customers, charges, payment_intents, subscriptions, invoices, products, prices, \
        refunds, balance, events, checkout/sessions, payment_links, coupons, disputes, \
        transfers, payouts, files, webhooks endpoints, etc). Pass the documented Stripe \
        path and a nested JSON object of parameters matching Stripe's API docs for that \
        endpoint; nesting and arrays are automatically converted to Stripe's expected \
        wire format. List endpoints support pagination via params like \
        {\"limit\": 10, \"starting_after\": \"cus_123\"}."
    )]
    async fn stripe_api(
        &self,
        Parameters(req): Parameters<StripeApiRequest>,
    ) -> Result<CallToolResult, McpError> {
        if !req.path.starts_with('/') {
            return Err(McpError::invalid_params(
                "path must start with '/', e.g. /v1/customers",
                None,
            ));
        }

        let url = format!("{STRIPE_API_BASE}{}", req.path);
        let mut pairs = Vec::new();
        flatten_params("", &req.params, &mut pairs);

        let mut builder = match req.method {
            HttpMethod::Get => self.http.get(&url).query(&pairs),
            HttpMethod::Post => self.http.post(&url).form(&pairs),
            HttpMethod::Delete => {
                if pairs.is_empty() {
                    self.http.delete(&url)
                } else {
                    self.http.delete(&url).query(&pairs)
                }
            }
        };

        builder = builder.bearer_auth(self.secret_key.as_str());

        if let Some(account) = &req.stripe_account {
            builder = builder.header("Stripe-Account", account);
        }
        if let Some(key) = &req.idempotency_key {
            builder = builder.header("Idempotency-Key", key);
        }

        let response = builder.send().await.map_err(|e| {
            McpError::internal_error(format!("request to Stripe failed: {e}"), None)
        })?;

        let status = response.status();
        let body_text = response
            .text()
            .await
            .unwrap_or_else(|e| format!("<failed to read response body: {e}>"));

        // Pretty-print if it parses as JSON, otherwise pass through raw.
        let pretty = serde_json::from_str::<Value>(&body_text)
            .map(|v| serde_json::to_string_pretty(&v).unwrap_or(body_text.clone()))
            .unwrap_or(body_text);

        let content = vec![ContentBlock::text(format!(
            "HTTP {}\n{}",
            status.as_u16(),
            pretty
        ))];

        if status.is_success() {
            Ok(CallToolResult::success(content))
        } else {
            Ok(CallToolResult::error(content))
        }
    }
}

#[tool_handler]
impl ServerHandler for StripeServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder().enable_tools().build(),
        )
        .with_server_info(Implementation::new("stripe-mcp-server", env!("CARGO_PKG_VERSION")))
        .with_protocol_version(ProtocolVersion::V_2024_11_05)
        .with_instructions(
            "This server exposes the Stripe REST API through a single generic tool, \
            `stripe_api`. Give it an HTTP method, a Stripe API path (e.g. /v1/customers), \
            and a params object shaped like Stripe's documented request body for that \
            endpoint. It covers the full Stripe API surface, not just a curated subset."
                .to_string(),
        )
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let secret_key = std::env::var("STRIPE_SECRET_KEY")
        .map_err(|_| anyhow::anyhow!("STRIPE_SECRET_KEY environment variable is not set"))?;
    if !secret_key.starts_with("sk_") && !secret_key.starts_with("rk_") {
        tracing::warn!(
            "STRIPE_SECRET_KEY does not look like a Stripe secret/restricted key (expected it to start with sk_ or rk_)"
        );
    }

    let bind_address =
        std::env::var(BIND_ADDRESS_ENV).unwrap_or_else(|_| DEFAULT_BIND_ADDRESS.to_string());

    let ct = tokio_util::sync::CancellationToken::new();
    let service = StreamableHttpService::new(
        move || Ok(StripeServer::new(secret_key.clone())),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default().with_cancellation_token(ct.child_token()),
    );

    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(&bind_address).await?;
    tracing::info!("Stripe MCP server listening on http://{bind_address}/mcp");

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            ct.cancel();
        })
        .await?;

    Ok(())
}
