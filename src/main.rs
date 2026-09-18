//! Stripe MCP server, exposed over the MCP Streamable HTTP transport.
//!
//! Two layers of tools, both built on the same request pipeline:
//!
//! - `stripe_api`: a generic tool that can call *any* Stripe REST endpoint
//!   with any method, giving full API coverage without per-endpoint code.
//! - ~15 explicit tools (`list_customers`, `create_payment_intent`, etc.)
//!   for the common operations, so a model can pick a well-known tool by
//!   name instead of assembling a raw path/params call. Each explicit tool
//!   still accepts an `extra_params` object for anything not covered by its
//!   named fields, so nothing is lost relative to the generic tool.
//!
//! Auth: the Stripe secret key is read once from the `STRIPE_SECRET_KEY`
//! environment variable at startup and used for every request. It is never
//! echoed back to the client.

use std::sync::Arc;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
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
use serde_json::{Map, Value, json};

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

/// Percent-encodes a single path segment (e.g. a Stripe object ID) so it
/// can't inject extra path segments or query strings into the request.
fn encode_id(id: &str) -> String {
    utf8_percent_encode(id, NON_ALPHANUMERIC).to_string()
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

/// Drops null-valued keys from a top-level JSON object (used to clean up
/// `json!({...})` results built from `Option` fields).
fn strip_nulls(value: &mut Value) {
    if let Value::Object(map) = value {
        map.retain(|_, v| !v.is_null());
    }
}

/// Overlays `extra`'s keys onto `base` (a JSON object), with `extra` values
/// winning on conflict. Used so every explicit tool's named fields can be
/// augmented or overridden via its `extra_params` catch-all.
fn merge_params(mut base: Value, extra: Value) -> Value {
    if let (Value::Object(base_map), Value::Object(extra_map)) = (&mut base, extra) {
        for (k, v) in extra_map {
            base_map.insert(k, v);
        }
    }
    base
}

fn empty_extra() -> Value {
    Value::Object(Map::new())
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListCustomersRequest {
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    starting_after: Option<String>,
    /// Any additional Stripe params not covered above, as a nested JSON
    /// object (same shape as the `stripe_api` tool's `params`). Merged
    /// in on top of the named fields, so this can also override them.
    #[serde(default = "empty_extra")]
    extra_params: Value,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CustomerIdRequest {
    /// Stripe customer ID, e.g. "cus_123".
    customer_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CreateCustomerRequest {
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    metadata: Option<Value>,
    /// Any additional Stripe params not covered above, as a nested JSON
    /// object (same shape as the `stripe_api` tool's `params`). Merged
    /// in on top of the named fields, so this can also override them.
    #[serde(default = "empty_extra")]
    extra_params: Value,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListChargesRequest {
    #[serde(default)]
    customer: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    starting_after: Option<String>,
    /// Any additional Stripe params not covered above, as a nested JSON
    /// object (same shape as the `stripe_api` tool's `params`). Merged
    /// in on top of the named fields, so this can also override them.
    #[serde(default = "empty_extra")]
    extra_params: Value,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ChargeIdRequest {
    /// Stripe charge ID, e.g. "ch_123".
    charge_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListPaymentIntentsRequest {
    #[serde(default)]
    customer: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    starting_after: Option<String>,
    /// Any additional Stripe params not covered above, as a nested JSON
    /// object (same shape as the `stripe_api` tool's `params`). Merged
    /// in on top of the named fields, so this can also override them.
    #[serde(default = "empty_extra")]
    extra_params: Value,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CreatePaymentIntentRequest {
    /// Amount in the currency's smallest unit (e.g. cents for USD).
    amount: i64,
    /// Three-letter ISO currency code, e.g. "usd".
    currency: String,
    #[serde(default)]
    customer: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    metadata: Option<Value>,
    /// Defaults to true if omitted, matching Stripe's recommended integration.
    #[serde(default)]
    automatic_payment_methods_enabled: Option<bool>,
    /// Any additional Stripe params not covered above, as a nested JSON
    /// object (same shape as the `stripe_api` tool's `params`). Merged
    /// in on top of the named fields, so this can also override them.
    #[serde(default = "empty_extra")]
    extra_params: Value,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PaymentIntentIdRequest {
    /// Stripe PaymentIntent ID, e.g. "pi_123".
    payment_intent_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListSubscriptionsRequest {
    #[serde(default)]
    customer: Option<String>,
    /// e.g. "active", "canceled", "past_due", "all".
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
    /// Any additional Stripe params not covered above, as a nested JSON
    /// object (same shape as the `stripe_api` tool's `params`). Merged
    /// in on top of the named fields, so this can also override them.
    #[serde(default = "empty_extra")]
    extra_params: Value,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CreateSubscriptionRequest {
    /// Stripe customer ID to subscribe.
    customer: String,
    /// Stripe Price ID for the subscription's first item, e.g. "price_123".
    price: String,
    #[serde(default)]
    quantity: Option<u32>,
    /// Any additional Stripe params not covered above, as a nested JSON
    /// object (same shape as the `stripe_api` tool's `params`). Merged
    /// in on top of the named fields, so this can also override them.
    #[serde(default = "empty_extra")]
    extra_params: Value,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CancelSubscriptionRequest {
    /// Stripe subscription ID, e.g. "sub_123".
    subscription_id: String,
    /// If true, schedules cancellation at the end of the current billing
    /// period instead of cancelling immediately.
    #[serde(default)]
    at_period_end: Option<bool>,
    /// Any additional Stripe params not covered above, as a nested JSON
    /// object (same shape as the `stripe_api` tool's `params`). Merged
    /// in on top of the named fields, so this can also override them.
    #[serde(default = "empty_extra")]
    extra_params: Value,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListInvoicesRequest {
    #[serde(default)]
    customer: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
    /// Any additional Stripe params not covered above, as a nested JSON
    /// object (same shape as the `stripe_api` tool's `params`). Merged
    /// in on top of the named fields, so this can also override them.
    #[serde(default = "empty_extra")]
    extra_params: Value,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct InvoiceIdRequest {
    /// Stripe invoice ID, e.g. "in_123".
    invoice_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CreateRefundRequest {
    /// Charge ID to refund. Provide this or `payment_intent`.
    #[serde(default)]
    charge: Option<String>,
    /// PaymentIntent ID to refund. Provide this or `charge`.
    #[serde(default)]
    payment_intent: Option<String>,
    /// Amount to refund, in the currency's smallest unit. Omit to refund
    /// the full remaining amount.
    #[serde(default)]
    amount: Option<i64>,
    /// e.g. "duplicate", "fraudulent", "requested_by_customer".
    #[serde(default)]
    reason: Option<String>,
    /// Any additional Stripe params not covered above, as a nested JSON
    /// object (same shape as the `stripe_api` tool's `params`). Merged
    /// in on top of the named fields, so this can also override them.
    #[serde(default = "empty_extra")]
    extra_params: Value,
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

    /// Shared request pipeline used by every tool, explicit or generic.
    async fn call(
        &self,
        method: HttpMethod,
        path: String,
        params: Value,
        stripe_account: Option<String>,
        idempotency_key: Option<String>,
    ) -> Result<CallToolResult, McpError> {
        if !path.starts_with('/') {
            return Err(McpError::invalid_params(
                "path must start with '/', e.g. /v1/customers",
                None,
            ));
        }

        let url = format!("{STRIPE_API_BASE}{path}");
        let mut pairs = Vec::new();
        flatten_params("", &params, &mut pairs);

        let mut builder = match method {
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

        if let Some(account) = &stripe_account {
            builder = builder.header("Stripe-Account", account);
        }
        if let Some(key) = &idempotency_key {
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

    // ---- Generic fallback tool -------------------------------------------------

    #[tool(
        description = "Call the Stripe API directly. Use this for anything not covered \
        by the named tools (list_customers, create_payment_intent, etc.) - it works \
        against any Stripe REST endpoint (products, prices, checkout/sessions, \
        payment_links, coupons, disputes, transfers, payouts, files, events, webhook \
        endpoints, and any future Stripe endpoint). Pass the documented Stripe path and \
        a nested JSON object of parameters matching Stripe's API docs; nesting and \
        arrays are automatically converted to Stripe's expected wire format."
    )]
    async fn stripe_api(
        &self,
        Parameters(req): Parameters<StripeApiRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.call(
            req.method,
            req.path,
            req.params,
            req.stripe_account,
            req.idempotency_key,
        )
        .await
    }

    // ---- Customers ---------------------------------------------------------

    #[tool(description = "List Stripe customers, optionally filtered by email.")]
    async fn list_customers(
        &self,
        Parameters(req): Parameters<ListCustomersRequest>,
    ) -> Result<CallToolResult, McpError> {
        let mut params = json!({
            "limit": req.limit,
            "email": req.email,
            "starting_after": req.starting_after,
        });
        strip_nulls(&mut params);
        let params = merge_params(params, req.extra_params);
        self.call(HttpMethod::Get, "/v1/customers".into(), params, None, None)
            .await
    }

    #[tool(description = "Retrieve a single Stripe customer by ID.")]
    async fn get_customer(
        &self,
        Parameters(req): Parameters<CustomerIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/v1/customers/{}", encode_id(&req.customer_id));
        self.call(HttpMethod::Get, path, json!({}), None, None).await
    }

    #[tool(description = "Create a new Stripe customer.")]
    async fn create_customer(
        &self,
        Parameters(req): Parameters<CreateCustomerRequest>,
    ) -> Result<CallToolResult, McpError> {
        let mut params = json!({
            "email": req.email,
            "name": req.name,
            "description": req.description,
            "metadata": req.metadata,
        });
        strip_nulls(&mut params);
        let params = merge_params(params, req.extra_params);
        self.call(HttpMethod::Post, "/v1/customers".into(), params, None, None)
            .await
    }

    // ---- Charges -------------------------------------------------------------

    #[tool(description = "List Stripe charges, optionally filtered by customer.")]
    async fn list_charges(
        &self,
        Parameters(req): Parameters<ListChargesRequest>,
    ) -> Result<CallToolResult, McpError> {
        let mut params = json!({
            "customer": req.customer,
            "limit": req.limit,
            "starting_after": req.starting_after,
        });
        strip_nulls(&mut params);
        let params = merge_params(params, req.extra_params);
        self.call(HttpMethod::Get, "/v1/charges".into(), params, None, None)
            .await
    }

    #[tool(description = "Retrieve a single Stripe charge by ID.")]
    async fn get_charge(
        &self,
        Parameters(req): Parameters<ChargeIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/v1/charges/{}", encode_id(&req.charge_id));
        self.call(HttpMethod::Get, path, json!({}), None, None).await
    }

    // ---- Payment intents -------------------------------------------------------

    #[tool(description = "List Stripe PaymentIntents, optionally filtered by customer.")]
    async fn list_payment_intents(
        &self,
        Parameters(req): Parameters<ListPaymentIntentsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let mut params = json!({
            "customer": req.customer,
            "limit": req.limit,
            "starting_after": req.starting_after,
        });
        strip_nulls(&mut params);
        let params = merge_params(params, req.extra_params);
        self.call(
            HttpMethod::Get,
            "/v1/payment_intents".into(),
            params,
            None,
            None,
        )
        .await
    }

    #[tool(
        description = "Create a Stripe PaymentIntent to collect a payment. \
        automatic_payment_methods is enabled by default unless overridden."
    )]
    async fn create_payment_intent(
        &self,
        Parameters(req): Parameters<CreatePaymentIntentRequest>,
    ) -> Result<CallToolResult, McpError> {
        let automatic = req.automatic_payment_methods_enabled.unwrap_or(true);
        let mut params = json!({
            "amount": req.amount,
            "currency": req.currency,
            "customer": req.customer,
            "description": req.description,
            "metadata": req.metadata,
            "automatic_payment_methods": { "enabled": automatic },
        });
        strip_nulls(&mut params);
        let params = merge_params(params, req.extra_params);
        self.call(
            HttpMethod::Post,
            "/v1/payment_intents".into(),
            params,
            None,
            None,
        )
        .await
    }

    #[tool(description = "Retrieve a single Stripe PaymentIntent by ID.")]
    async fn get_payment_intent(
        &self,
        Parameters(req): Parameters<PaymentIntentIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!(
            "/v1/payment_intents/{}",
            encode_id(&req.payment_intent_id)
        );
        self.call(HttpMethod::Get, path, json!({}), None, None).await
    }

    // ---- Subscriptions ---------------------------------------------------------

    #[tool(description = "List Stripe subscriptions, optionally filtered by customer or status.")]
    async fn list_subscriptions(
        &self,
        Parameters(req): Parameters<ListSubscriptionsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let mut params = json!({
            "customer": req.customer,
            "status": req.status,
            "limit": req.limit,
        });
        strip_nulls(&mut params);
        let params = merge_params(params, req.extra_params);
        self.call(
            HttpMethod::Get,
            "/v1/subscriptions".into(),
            params,
            None,
            None,
        )
        .await
    }

    #[tool(description = "Create a Stripe subscription for a customer on a single price.")]
    async fn create_subscription(
        &self,
        Parameters(req): Parameters<CreateSubscriptionRequest>,
    ) -> Result<CallToolResult, McpError> {
        let mut item = json!({ "price": req.price });
        if let Some(q) = req.quantity {
            item["quantity"] = json!(q);
        }
        let mut params = json!({
            "customer": req.customer,
            "items": [item],
        });
        strip_nulls(&mut params);
        let params = merge_params(params, req.extra_params);
        self.call(
            HttpMethod::Post,
            "/v1/subscriptions".into(),
            params,
            None,
            None,
        )
        .await
    }

    #[tool(
        description = "Cancel a Stripe subscription, either immediately (default) or at \
        the end of the current billing period."
    )]
    async fn cancel_subscription(
        &self,
        Parameters(req): Parameters<CancelSubscriptionRequest>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/v1/subscriptions/{}", encode_id(&req.subscription_id));
        if req.at_period_end.unwrap_or(false) {
            let mut params = json!({ "cancel_at_period_end": true });
            let params = merge_params(std::mem::take(&mut params), req.extra_params);
            self.call(HttpMethod::Post, path, params, None, None).await
        } else {
            let params = merge_params(json!({}), req.extra_params);
            self.call(HttpMethod::Delete, path, params, None, None).await
        }
    }

    // ---- Invoices -----------------------------------------------------------

    #[tool(description = "List Stripe invoices, optionally filtered by customer.")]
    async fn list_invoices(
        &self,
        Parameters(req): Parameters<ListInvoicesRequest>,
    ) -> Result<CallToolResult, McpError> {
        let mut params = json!({
            "customer": req.customer,
            "limit": req.limit,
        });
        strip_nulls(&mut params);
        let params = merge_params(params, req.extra_params);
        self.call(HttpMethod::Get, "/v1/invoices".into(), params, None, None)
            .await
    }

    #[tool(description = "Retrieve a single Stripe invoice by ID.")]
    async fn get_invoice(
        &self,
        Parameters(req): Parameters<InvoiceIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/v1/invoices/{}", encode_id(&req.invoice_id));
        self.call(HttpMethod::Get, path, json!({}), None, None).await
    }

    // ---- Refunds & balance -------------------------------------------------

    #[tool(
        description = "Refund a charge or PaymentIntent, fully or partially. Provide \
        `charge` or `payment_intent` (not both)."
    )]
    async fn create_refund(
        &self,
        Parameters(req): Parameters<CreateRefundRequest>,
    ) -> Result<CallToolResult, McpError> {
        if req.charge.is_none() && req.payment_intent.is_none() {
            return Err(McpError::invalid_params(
                "provide either `charge` or `payment_intent`",
                None,
            ));
        }
        let mut params = json!({
            "charge": req.charge,
            "payment_intent": req.payment_intent,
            "amount": req.amount,
            "reason": req.reason,
        });
        strip_nulls(&mut params);
        let params = merge_params(params, req.extra_params);
        self.call(HttpMethod::Post, "/v1/refunds".into(), params, None, None)
            .await
    }

    #[tool(description = "Retrieve current Stripe account balance.")]
    async fn get_balance(&self) -> Result<CallToolResult, McpError> {
        self.call(HttpMethod::Get, "/v1/balance".into(), json!({}), None, None)
            .await
    }
}

#[tool_handler]
impl ServerHandler for StripeServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "stripe-mcp-server",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "This server exposes Stripe. Prefer the named tools (list_customers, \
                create_customer, list_charges, get_charge, list_payment_intents, \
                create_payment_intent, get_payment_intent, list_subscriptions, \
                create_subscription, cancel_subscription, list_invoices, get_invoice, \
                create_refund, get_balance) for common operations. For anything else, \
                use the generic `stripe_api` tool, which can call any Stripe REST \
                endpoint."
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
