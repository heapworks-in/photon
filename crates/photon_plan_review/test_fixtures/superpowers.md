# Retry declined card charges with backoff

> **For agentic workers:** Use superpowers:executing-plans to implement this plan task by task.

**Goal:** Charges that fail for a passing reason (a timeout, a 5xx) are retried with backoff
instead of waiting for the nightly job.

**Spec:** `docs/specs/2026-10-20-retry-charges-design.md`

**Constraints:** No new dependencies. Retries never block the request for more than 10 seconds.

**Decision:** Declines are never retried.

---

### Task 1: Backoff type

**Files:**
- Create: `src/retry.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn delays_double_up_to_the_cap() {
    let backoff = Backoff::default();
    assert_eq!(backoff.delay(3), Duration::from_millis(800));
}
```

- [ ] **Step 2: Add the type**

```rust
pub struct Backoff {
    pub base: Duration,
    pub cap: Duration,
}

impl Backoff {
    pub fn delay(&self, attempt: u32) -> Duration {
        self.base.saturating_mul(1 << attempt).min(self.cap)
    }
}
```

Add `mod retry;` to `src/main.rs`. `Backoff` starts at 100 ms and caps at 2 s.

### Task 2: retry_with_backoff()

**Files:**
- Modify: `src/retry.rs`
- Modify: `Cargo.toml`

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn gives_up_after_max_attempts() {
    let calls = AtomicU32::new(0);
    let result = retry_with_backoff(Backoff::default(), |_| true, |_| async {
        calls.fetch_add(1, Ordering::SeqCst);
        Err::<(), _>("down")
    })
    .await;
    assert!(result.is_err());
}
```

- [ ] **Step 2: Implement it**

```rust
pub async fn retry_with_backoff<T, E, F, Fut>(
    backoff: Backoff,
    retryable: impl Fn(&E) -> bool,
    mut call: F,
) -> Result<T, E>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    todo!()
}
```

Sleep for `Backoff::delay` between attempts, plus up to 100 ms of jitter.

**Decision:** Jitter is added after the cap, on purpose.

Run: `cargo test gives_up_after_max_attempts`. Expected: PASS.

### Task 3: Retry in Gateway::charge

**Files:** Modify `src/payments.rs:48-62` · Modify `src/gateway.rs` · Test `src/payments.rs`

**Produces:** `Gateway::charge(&self, invoice: &Invoice) -> Result<Receipt, ChargeError>`

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn retries_gateway_timeouts() {
    let gateway = Gateway::with_responses([Timeout, Timeout, Ok(receipt())]);
    let receipt = gateway.charge(&invoice()).await.unwrap();
    assert_eq!(gateway.attempts(), 3);
}
```

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test retries_gateway_timeouts`. Expected: FAIL, `attempts() == 1`.

- [ ] **Step 3: Wrap the call**

Replace the direct HTTP call with `retry_with_backoff(self.backoff, is_retryable, |attempt| self.send(&request))`, from `src/retry.rs`. Timeouts and 5xx are retryable; declines are not.

- [ ] **Step 4: Run the tests**

Run: `cargo test`. Expected: PASS.

### Task 4: Idempotency key

**Files:**
- Modify: `src/ledger.rs`

- [ ] **Step 1: Keep one key per invoice**

Every attempt sends the same key, so a retried charge is never taken twice. `record_payment` stores the key with the receipt.

Run: `cargo test idempotency`. Expected: PASS.

### Task 5: Nightly job picks up retries

**Files:**
- Modify: `src/jobs.rs`
- Test: `tests/nightly.rs`

- [ ] **Step 1: Write the tests**

```rust
#[tokio::test]
async fn retries_failed_invoices_nightly() {}

#[tokio::test]
async fn skips_declined_invoices() {}
```

- [ ] **Step 2: Skip declines**

`retry_failed_invoices` skips invoices whose last error was a decline; `failed_invoices` returns the error with each invoice.

### Task 6: Metrics

**Files:**
- Create: `src/metrics.rs`

Metrics counter for retries, so the dashboard shows how often charges are retried.

```rust
pub fn record_retry(attempt: u32) {
    RETRIES.with_label_values(&[&attempt.to_string()]).inc();
}
```

Call `record_retry` from `retry_with_backoff` before each sleep.

### Task 7: Docs

**Files:**
- Modify: `docs/architecture.md`

Add a "Retries" section: what is retried, how often, and that declines never are.
