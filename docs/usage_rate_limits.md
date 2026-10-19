# Usage Rate Limits & Burst Protection

Usage charge calls now support per-subscription rate limiting and abuse protection to ensure the billing system cannot be spammed or exploited.

## Configuration

These limits are configured by the merchant via the `configure_usage_limits` entry point:

- `rate_limit_max_calls: Option<u32>`: Maximum allowed usage charges within the rate window.
- `rate_window_secs: u64`: Duration of the rate limit window in seconds.
- `burst_min_interval_secs: u64`: Minimum required time (in seconds) between any two consecutive usage charges. `0` disables burst protection.
- `usage_cap_units: Option<i128>`: Maximum usage amount allowed per billing period.

## Enforcement Mechanisms

### 1. Burst Protection
- The contract records `last_usage_timestamp` for each subscription.
- If a new charge is attempted where `now - last_usage_timestamp < burst_min_interval_secs`, it is rejected with `UsageChargeResult::BurstLimitExceeded`.
- A configured interval of `0` disables this check, so consecutive usage charges can occur at the same timestamp (other limits and replay protection still apply).
- **Purpose**: Prevents rapid repeated calls in the same ledger or across very short intervals.

### 2. Approximate Time-Window Rate Limits
- A single counter (`window_call_count`) is stored with `window_start_timestamp`. The window starts with the first accepted usage charge and resets lazily on the first accepted charge at or after `window_start_timestamp + rate_window_secs`.
- When the counter reaches `rate_limit_max_calls`, further calls in that window are rejected with `UsageChargeResult::RateLimitExceeded`.
- This is not a true sliding-window limit or a clock-aligned fixed window. Calls can cluster across a reset: for example, with a limit of 5 per 60 seconds, 5 calls near the end of one window and 5 just after its boundary may all be accepted within a short period. Thus, nearly twice the configured count can be accepted in a brief boundary-crossing span.
- Treat this setting as approximate abuse control, not an exact quota or billing-accuracy guarantee. If strict request pacing is required, enforce it in the metering service with a sliding-window limiter and leave margin below the on-chain limit. Use `usage_cap_units` for the exact per-billing-period amount cap.

### 3. Replay Protection
- Every usage charge must include a unique `reference: String` parameter.
- The contract stores the last processed reference per subscription.
- If a subsequent call provides the exact same reference, it is rejected with `UsageChargeResult::Replay`.
- **Purpose**: Ensures the same off-chain usage event cannot be double-processed if the metering service retries a delayed transaction.

## Indexer-visible enforcement outcomes

Usage charge enforcement is designed to be observable by indexers without relying on reverted transactions.

- On success, `charge_usage_with_reference` completes normally and emits `usage_charged`.
- On enforcement outcomes (replay / burst / rate limit / per-period cap), `charge_usage_with_reference` completes normally and emits `usage_charge_rejected`.
- When a usage charge would exceed the subscription's `lifetime_cap`, the subscription is cancelled and `lifetime_cap_reached` is emitted (this outcome is not emitted as `usage_charge_rejected`).

`usage_charge_rejected` payload is `UsageChargeRejectedEvent` and includes:

- `subscription_id`
- `merchant`
- `token`
- `usage_amount`
- `timestamp`
- `reference`
- `result: UsageChargeResult`

`configure_usage_limits` emits `usage_limits_configured` with the final stored parameters.

## Storage Footprint
The state is highly bounded:
- **Limits**: Stored under `DataKey::UsageLimits(subscription_id)`
- **State**: Stored under `DataKey::UsageState(subscription_id)` (contains timestamps, counters, and period usage)
- **Reference**: Stored under `DataKey::UsageReference(subscription_id)`

## Example: Valid Usage vs Rejected Usage
- **Valid Usage**:
  - `charge_usage_with_reference(sub_id, 1_000_000, "txn_123")` succeeds.
  - 5 seconds later: `charge_usage_with_reference(sub_id, 2_000_000, "txn_124")` succeeds (burst interval > 2s).
- **Rejected Usage**:
  - `charge_usage_with_reference(sub_id, 1_000_000, "txn_123")` immediately fails (Replay).
  - Calling 10 times in 1 minute when max_calls=5 fails on the 6th call (RateLimitExceeded).
