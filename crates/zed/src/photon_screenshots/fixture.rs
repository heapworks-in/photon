// The fixture project every scene opens: a small billing service with one pull request.

pub const CARGO_TOML: &str = r#"[package]
name = "billing"
version = "0.4.0"
edition = "2024"

[dependencies]
anyhow = "1"
axum = "0.8"
rand = "0.9"
reqwest = { version = "0.12", features = ["json"] }
serde = { version = "1", features = ["derive"] }
sqlx = { version = "0.8", features = ["postgres", "runtime-tokio"] }
thiserror = "2"
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
"#;

pub const README: &str = r#"# billing

Charges invoices, records payments and retries failed charges overnight.

```sh
DATABASE_URL=postgres://localhost/billing GATEWAY_KEY=test_key cargo run
```

See [docs/architecture.md](docs/architecture.md) for how a charge flows through the service.
"#;

pub const ARCHITECTURE: &str = r#"# Billing service

Turns open invoices into payments. It owns the ledger and is the only service that talks to the
card gateway.

## How a charge flows

```mermaid
flowchart LR
    checkout[Checkout] --> api[API]
    api --> gateway{{Card gateway}}
    gateway -->|timeout| retry[Retry]
    retry --> gateway
```

## Retry policy

| Failure         | Retried | Next step               |
| --------------- | ------- | ----------------------- |
| Card declined   | No      | Email the customer      |
| Gateway timeout | Yes     | Nightly job picks it up |
| Gateway 5xx     | Yes     | Nightly job picks it up |

## Backoff

Delays double from 200 ms up to 5 s, with up to 25% jitter:

```rust
let backoff = Backoff {
    initial: Duration::from_millis(200),
    max: Duration::from_secs(5),
    attempts: 4,
};
```

## Running locally

1. Start Postgres and run `sqlx migrate run`.
2. Export `GATEWAY_KEY` with a sandbox key.
3. `cargo run`, then charge a test invoice with `curl -X POST localhost:8080/invoices/42/charge`.
"#;

pub const MAIN_RS: &str = r#"mod api;
mod invoice;
mod jobs;
mod ledger;
mod payments;
mod retry;

use std::{net::SocketAddr, sync::Arc};

use anyhow::Result;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("billing=info")
        .init();

    let config = api::Config::from_env()?;
    let ledger = ledger::Ledger::connect(&config.database_url).await?;
    let gateway = Arc::new(payments::Gateway::new(&config.gateway_key));
    tokio::spawn(jobs::run_nightly(ledger.clone(), gateway.clone()));

    let app = api::router(api::AppState { ledger, gateway });
    let address: SocketAddr = config.listen.parse()?;
    info!(%address, "billing service listening");
    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
"#;

pub const MAIN_RS_BASE: &str = r#"mod api;
mod invoice;
mod jobs;
mod ledger;
mod payments;

use std::{net::SocketAddr, sync::Arc};

use anyhow::Result;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("billing=info")
        .init();

    let config = api::Config::from_env()?;
    let ledger = ledger::Ledger::connect(&config.database_url).await?;
    let gateway = Arc::new(payments::Gateway::new(&config.gateway_key));
    tokio::spawn(jobs::run_nightly(ledger.clone(), gateway.clone()));

    let app = api::router(api::AppState { ledger, gateway });
    let address: SocketAddr = config.listen.parse()?;
    info!(%address, "billing service listening");
    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
"#;

pub const INVOICE_RS: &str = r#"use serde::{Deserialize, Serialize};

pub type InvoiceId = i64;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LineItem {
    pub description: String,
    pub quantity: u32,
    pub unit_price_cents: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Invoice {
    pub id: InvoiceId,
    pub customer_id: String,
    pub currency: String,
    pub lines: Vec<LineItem>,
    pub discount_cents: i64,
}

impl Invoice {
    pub fn total_cents(&self) -> i64 {
        let subtotal: i64 = self
            .lines
            .iter()
            .map(|line| line.unit_price_cents * i64::from(line.quantity))
            .sum();
        (subtotal - self.discount_cents).max(0)
    }
}
"#;

pub const LEDGER_RS: &str = r#"use anyhow::{Context as _, Result};
use sqlx::PgPool;

use crate::{
    invoice::{Invoice, InvoiceId},
    payments::Receipt,
};

#[derive(Clone)]
pub struct Ledger {
    pool: PgPool,
}

impl Ledger {
    pub async fn connect(database_url: &str) -> Result<Self> {
        let pool = PgPool::connect(database_url)
            .await
            .context("connecting to the ledger database")?;
        Ok(Self { pool })
    }

    pub async fn open_invoice(&self, id: InvoiceId) -> Result<Invoice> {
        let row = sqlx::query_scalar::<_, sqlx::types::Json<Invoice>>(
            "SELECT body FROM invoices WHERE id = $1 AND paid_at IS NULL",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    pub async fn failed_invoices(&self) -> Result<Vec<Invoice>> {
        let rows = sqlx::query_scalar::<_, sqlx::types::Json<Invoice>>(
            "SELECT body FROM invoices WHERE last_charge_failed AND paid_at IS NULL",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|row| row.0).collect())
    }

    pub async fn record_payment(&self, invoice: &Invoice, receipt: &Receipt) -> Result<()> {
        sqlx::query("UPDATE invoices SET paid_at = now(), receipt_id = $2 WHERE id = $1")
            .bind(invoice.id)
            .bind(&receipt.id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
"#;

pub const JOBS_RS: &str = r#"use std::{sync::Arc, time::Duration};

use tracing::{info, warn};

use crate::{ledger::Ledger, payments::Gateway};

pub async fn run_nightly(ledger: Ledger, gateway: Arc<Gateway>) {
    let mut interval = tokio::time::interval(Duration::from_secs(24 * 60 * 60));
    loop {
        interval.tick().await;
        retry_failed_invoices(&ledger, &gateway).await;
    }
}

async fn retry_failed_invoices(ledger: &Ledger, gateway: &Gateway) {
    let invoices = match ledger.failed_invoices().await {
        Ok(invoices) => invoices,
        Err(error) => return warn!(%error, "couldn't list failed invoices"),
    };
    info!(count = invoices.len(), "retrying failed invoices");
    for invoice in invoices {
        match gateway.charge(&invoice).await {
            Ok(receipt) => {
                if let Err(error) = ledger.record_payment(&invoice, &receipt).await {
                    warn!(invoice = invoice.id, %error, "charged but not recorded");
                }
            }
            Err(error) => warn!(invoice = invoice.id, %error, "charge failed again"),
        }
    }
}
"#;

pub const API_RS_BASE: &str = r#"use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::post,
};
use serde::Serialize;

use crate::{
    invoice::InvoiceId,
    ledger::Ledger,
    payments::{ChargeError, Gateway},
};

pub struct Config {
    pub listen: String,
    pub database_url: String,
    pub gateway_key: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            listen: std::env::var("BILLING_LISTEN").unwrap_or_else(|_| "0.0.0.0:8080".into()),
            database_url: std::env::var("DATABASE_URL")?,
            gateway_key: std::env::var("GATEWAY_KEY")?,
        })
    }
}

#[derive(Clone)]
pub struct AppState {
    pub ledger: Ledger,
    pub gateway: Arc<Gateway>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/invoices/{id}/charge", post(charge_invoice))
        .with_state(state)
}

#[derive(Serialize)]
pub struct ChargeResponse {
    receipt_id: String,
    amount_cents: i64,
}

pub async fn charge_invoice(
    State(state): State<AppState>,
    Path(id): Path<InvoiceId>,
) -> Result<Json<ChargeResponse>, (StatusCode, String)> {
    let invoice = state.ledger.open_invoice(id).await.map_err(not_found)?;
    let receipt = state.gateway.charge(&invoice).await.map_err(charge_failed)?;
    state
        .ledger
        .record_payment(&invoice, &receipt)
        .await
        .map_err(internal)?;
    Ok(Json(ChargeResponse {
        receipt_id: receipt.id,
        amount_cents: receipt.amount_cents,
    }))
}

fn charge_failed(error: ChargeError) -> (StatusCode, String) {
    match error {
        ChargeError::Declined(reason) => (StatusCode::PAYMENT_REQUIRED, reason),
        ChargeError::Unavailable(_) => (
            StatusCode::BAD_GATEWAY,
            "payment gateway unavailable".into(),
        ),
    }
}

fn not_found(error: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::NOT_FOUND, error.to_string())
}

fn internal(error: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}
"#;

pub const API_RS_HEAD: &str = r#"use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::post,
};
use serde::Serialize;

use crate::{
    invoice::InvoiceId,
    ledger::Ledger,
    payments::{ChargeError, Gateway},
};

pub struct Config {
    pub listen: String,
    pub database_url: String,
    pub gateway_key: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            listen: std::env::var("BILLING_LISTEN").unwrap_or_else(|_| "0.0.0.0:8080".into()),
            database_url: std::env::var("DATABASE_URL")?,
            gateway_key: std::env::var("GATEWAY_KEY")?,
        })
    }
}

#[derive(Clone)]
pub struct AppState {
    pub ledger: Ledger,
    pub gateway: Arc<Gateway>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/invoices/{id}/charge", post(charge_invoice))
        .with_state(state)
}

#[derive(Serialize)]
pub struct ChargeResponse {
    receipt_id: String,
    amount_cents: i64,
}

pub async fn charge_invoice(
    State(state): State<AppState>,
    Path(id): Path<InvoiceId>,
) -> Result<Json<ChargeResponse>, (StatusCode, String)> {
    let invoice = state.ledger.open_invoice(id).await.map_err(not_found)?;
    let receipt = state.gateway.charge(&invoice).await.map_err(charge_failed)?;
    state
        .ledger
        .record_payment(&invoice, &receipt)
        .await
        .map_err(internal)?;
    Ok(Json(ChargeResponse {
        receipt_id: receipt.id,
        amount_cents: receipt.amount_cents,
    }))
}

fn charge_failed(error: ChargeError) -> (StatusCode, String) {
    match error {
        ChargeError::Declined(reason) => (StatusCode::PAYMENT_REQUIRED, reason),
        ChargeError::Unavailable(error) => {
            tracing::warn!(%error, "gateway unavailable after retries");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "payment gateway unavailable, try again shortly".into(),
            )
        }
    }
}

fn not_found(error: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::NOT_FOUND, error.to_string())
}

fn internal(error: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}
"#;

pub const PAYMENTS_RS_BASE: &str = r#"use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::invoice::Invoice;

const CHARGES_URL: &str = "https://gateway.example.com/v1/charges";

pub struct Gateway {
    client: reqwest::Client,
    api_key: String,
}

#[derive(Debug, Serialize)]
struct ChargeRequest<'a> {
    amount_cents: i64,
    currency: &'a str,
    customer: &'a str,
    idempotency_key: String,
}

#[derive(Debug, Deserialize)]
pub struct Receipt {
    pub id: String,
    pub amount_cents: i64,
}

#[derive(Debug, Error)]
pub enum ChargeError {
    #[error("card declined: {0}")]
    Declined(String),
    #[error("gateway unavailable")]
    Unavailable(#[from] reqwest::Error),
}

impl Gateway {
    pub fn new(api_key: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.to_string(),
        }
    }

    pub async fn charge(&self, invoice: &Invoice) -> Result<Receipt, ChargeError> {
        let request = ChargeRequest {
            amount_cents: invoice.total_cents(),
            currency: &invoice.currency,
            customer: &invoice.customer_id,
            idempotency_key: format!("invoice-{}", invoice.id),
        };
        let response = self
            .client
            .post(CHARGES_URL)
            .bearer_auth(&self.api_key)
            .json(&request)
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::PAYMENT_REQUIRED {
            return Err(ChargeError::Declined(response.text().await?));
        }
        Ok(response.error_for_status()?.json().await?)
    }
}
"#;

pub const PAYMENTS_RS_HEAD: &str = r#"use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    invoice::Invoice,
    retry::{Backoff, retry_with_backoff},
};

const CHARGES_URL: &str = "https://gateway.example.com/v1/charges";

pub struct Gateway {
    client: reqwest::Client,
    api_key: String,
    backoff: Backoff,
}

#[derive(Debug, Serialize)]
struct ChargeRequest<'a> {
    amount_cents: i64,
    currency: &'a str,
    customer: &'a str,
    idempotency_key: String,
}

#[derive(Debug, Deserialize)]
pub struct Receipt {
    pub id: String,
    pub amount_cents: i64,
}

#[derive(Debug, Error)]
pub enum ChargeError {
    #[error("card declined: {0}")]
    Declined(String),
    #[error("gateway unavailable")]
    Unavailable(#[from] reqwest::Error),
}

impl Gateway {
    pub fn new(api_key: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.to_string(),
            backoff: Backoff::default(),
        }
    }

    /// Charges the invoice's card. The idempotency key stays the same on every attempt, so a
    /// retry after a timeout can't charge the customer twice.
    pub async fn charge(&self, invoice: &Invoice) -> Result<Receipt, ChargeError> {
        let request = ChargeRequest {
            amount_cents: invoice.total_cents(),
            currency: &invoice.currency,
            customer: &invoice.customer_id,
            idempotency_key: format!("invoice-{}", invoice.id),
        };
        retry_with_backoff(self.backoff, is_retryable, |attempt| {
            tracing::debug!(invoice = invoice.id, attempt, "charging card");
            self.send_charge(&request)
        })
        .await
    }

    async fn send_charge(&self, request: &ChargeRequest<'_>) -> Result<Receipt, ChargeError> {
        let response = self
            .client
            .post(CHARGES_URL)
            .bearer_auth(&self.api_key)
            .json(request)
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::PAYMENT_REQUIRED {
            return Err(ChargeError::Declined(response.text().await?));
        }
        Ok(response.error_for_status()?.json().await?)
    }
}

/// Declines are final; only network errors and gateway 5xx responses are worth another try.
fn is_retryable(error: &ChargeError) -> bool {
    match error {
        ChargeError::Declined(_) => false,
        ChargeError::Unavailable(error) => {
            error.is_timeout()
                || error.is_connect()
                || error.status().is_some_and(|status| status.is_server_error())
        }
    }
}
"#;

pub const RETRY_RS: &str = r#"use std::{future::Future, time::Duration};

use rand::Rng as _;

/// How long to wait between attempts: doubling from `initial` up to `max`, plus jitter so
/// retries from many invoices don't reach the gateway at the same moment.
#[derive(Clone, Copy, Debug)]
pub struct Backoff {
    pub initial: Duration,
    pub max: Duration,
    pub attempts: u32,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(200),
            max: Duration::from_secs(5),
            attempts: 4,
        }
    }
}

impl Backoff {
    pub fn delay(&self, attempt: u32) -> Duration {
        let exponential = self.initial.saturating_mul(2u32.saturating_pow(attempt));
        let capped = exponential.min(self.max);
        let jitter = rand::rng().random_range(0..=capped.as_millis() as u64 / 4);
        capped + Duration::from_millis(jitter)
    }
}

/// Runs `operation` until it succeeds, `should_retry` says the error is final, or the
/// attempts run out. Returns the last error.
pub async fn retry_with_backoff<T, E, F, Fut>(
    backoff: Backoff,
    should_retry: impl Fn(&E) -> bool,
    mut operation: F,
) -> Result<T, E>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut attempt = 0;
    loop {
        match operation(attempt).await {
            Ok(value) => return Ok(value),
            Err(error) if attempt + 1 < backoff.attempts && should_retry(&error) => {
                tokio::time::sleep(backoff.delay(attempt)).await;
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_double_up_to_the_cap() {
        let backoff = Backoff::default();
        let first = backoff.delay(0);
        assert!(first >= Duration::from_millis(200) && first <= Duration::from_millis(250));
        assert!(backoff.delay(10) <= Duration::from_millis(6250));
    }
}
"#;

/// A first cut of `retry.rs`, pushed before the review and changed after it.
pub const RETRY_RS_FIRST: &str = r#"use std::{future::Future, time::Duration};

#[derive(Clone, Copy, Debug)]
pub struct Backoff {
    pub initial: Duration,
    pub max: Duration,
    pub attempts: u32,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(200),
            max: Duration::from_secs(5),
            attempts: 4,
        }
    }
}

impl Backoff {
    pub fn delay(&self, attempt: u32) -> Duration {
        self.initial
            .saturating_mul(2u32.saturating_pow(attempt))
            .min(self.max)
    }
}

pub async fn retry_with_backoff<T, E, F, Fut>(
    backoff: Backoff,
    should_retry: impl Fn(&E) -> bool,
    mut operation: F,
) -> Result<T, E>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut attempt = 0;
    loop {
        match operation(attempt).await {
            Ok(value) => return Ok(value),
            Err(error) if attempt + 1 < backoff.attempts && should_retry(&error) => {
                tokio::time::sleep(backoff.delay(attempt)).await;
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}
"#;

pub const JAVA_CLIENT: &str = r#"package dev.billing.client;

import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.time.Duration;

/** Charges invoices through the billing service. */
public final class BillingClient {
    private final HttpClient http;
    private final URI baseUri;

    public BillingClient(URI baseUri) {
        this.baseUri = baseUri;
        this.http = HttpClient.newBuilder()
                .connectTimeout(Duration.ofSeconds(5))
                .build();
    }

    public ChargeResult charge(long invoiceId) throws Exception {
        HttpRequest request = HttpRequest.newBuilder()
                .uri(baseUri.resolve("/invoices/" + invoiceId + "/charge"))
                .POST(HttpRequest.BodyPublishers.noBody())
                .build();
        HttpResponse<String> response = http.send(request, HttpResponse.BodyHandlers.ofString());
        return switch (response.statusCode()) {
            case 200 -> ChargeResult.paid(response.body());
            case 402 -> ChargeResult.declined(response.body());
            default -> ChargeResult.retryLater(response.statusCode());
        };
    }
}
"#;

pub const JAVA_ERROR: &str = "failed to initialize jdtls: Server reset the connection\n\
    -- stderr --\n\
    Error: LinkageError occurred while loading main class org.eclipse.jdt.ls.core.internal.JavaLanguageServerPlugin\n\
    java.lang.UnsupportedClassVersionError: This version of the Java Runtime only recognizes class file versions up to 61.0";

/// Files on `main`, before the pull request.
pub fn base_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Cargo.toml", CARGO_TOML),
        ("README.md", README),
        ("docs/architecture.md", ARCHITECTURE),
        ("src/main.rs", MAIN_RS_BASE),
        ("src/api.rs", API_RS_BASE),
        ("src/invoice.rs", INVOICE_RS),
        ("src/jobs.rs", JOBS_RS),
        ("src/ledger.rs", LEDGER_RS),
        ("src/payments.rs", PAYMENTS_RS_BASE),
        (
            "clients/java/src/main/java/dev/billing/client/BillingClient.java",
            JAVA_CLIENT,
        ),
    ]
}

/// The pull request's first push, then its second.
pub fn first_push() -> Vec<(&'static str, &'static str)> {
    vec![
        ("src/main.rs", MAIN_RS),
        ("src/payments.rs", PAYMENTS_RS_HEAD),
        ("src/retry.rs", RETRY_RS_FIRST),
    ]
}

pub fn second_push() -> Vec<(&'static str, &'static str)> {
    vec![("src/api.rs", API_RS_HEAD), ("src/retry.rs", RETRY_RS)]
}

/// Functions the fake language server knows, and who calls whom.
pub const FUNCTIONS: &[(&str, &str)] = &[
    ("charge_invoice", "src/api.rs"),
    ("charge_failed", "src/api.rs"),
    ("charge", "src/payments.rs"),
    ("send_charge", "src/payments.rs"),
    ("is_retryable", "src/payments.rs"),
    ("retry_with_backoff", "src/retry.rs"),
    ("delay", "src/retry.rs"),
    ("open_invoice", "src/ledger.rs"),
    ("record_payment", "src/ledger.rs"),
    ("failed_invoices", "src/ledger.rs"),
    ("total_cents", "src/invoice.rs"),
    ("run_nightly", "src/jobs.rs"),
    ("retry_failed_invoices", "src/jobs.rs"),
    ("main", "src/main.rs"),
];

pub const CALLS: &[(&str, &str)] = &[
    ("main", "run_nightly"),
    ("run_nightly", "retry_failed_invoices"),
    ("retry_failed_invoices", "failed_invoices"),
    ("retry_failed_invoices", "charge"),
    ("retry_failed_invoices", "record_payment"),
    ("charge_invoice", "open_invoice"),
    ("charge_invoice", "charge"),
    ("charge_invoice", "charge_failed"),
    ("charge_invoice", "record_payment"),
    ("charge", "total_cents"),
    ("charge", "retry_with_backoff"),
    ("charge", "send_charge"),
    ("charge", "is_retryable"),
    ("retry_with_backoff", "delay"),
];
