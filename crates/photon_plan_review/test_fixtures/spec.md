# Retry declined card charges

## Retry timeouts and server errors

A charge that times out or gets a 5xx is retried with exponential backoff.

## Never retry declines

A 402 is final.

## Nightly job

The nightly job skips invoices that were declined.

## Metrics

Retries are counted per attempt.

## Refund partial captures

Partial captures are refunded the same day.
