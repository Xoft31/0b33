# Usage-Based Billing

## Overview

Usage-based billing allows merchants to charge subscribers for **metered
consumption** rather than (or in addition to) fixed recurring intervals.
The feature is opt-in per subscription via the `usage_enabled` flag set at
creation time.

When enabled, an off-chain **usage metering service** measures consumption and
calls the `charge_usage` contract entrypoint with the computed amount. The
contract debits the subscriber's prepaid vault accordingly.

## How It Works

```
Off-chain metering service
        │
        │  charge_usage(subscription_id, usage_amount)
        ▼
┌──────────────────────┐
│  SubscriptionVault   │
│                      │
│  1. Validate status  │  (must be Active)
│  2. Check usage_enabled
│  3. Validate amount  │  (> 0)
│  4. Check balance    │  (prepaid_balance ≥ usage_amount)
│  5. Debit balance    │
│  6. Transition if 0  │  → InsufficientBalance
└──────────────────────┘
```

### Entry Point

```rust
pub fn charge_usage(
    env: Env,
    subscription_id: u32,
    usage_amount: i128,
) -> Result<(), Error>;
```

| Parameter          | Type   | Description                                   |
|--------------------|--------|-----------------------------------------------|
| `subscription_id`  | `u32`  | ID returned by `create_subscription`.          |
| `usage_amount`     | `i128` | Amount (in token stroops) to debit.            |

### Pre-conditions

| Check                | Error Returned             | Description                                           |
|----------------------|----------------------------|-------------------------------------------------------|
| Subscription exists  | `NotFound`                 | The given ID must reference a stored subscription.     |
| Status is `Active`   | `NotActive`                | Paused, cancelled, or insufficient-balance subs are rejected. |
| `usage_enabled`      | `UsageNotEnabled`          | The subscription must have been created with usage enabled. |
| Auto-renewal disabled and current interval elapsed | `NotActive` | Usage charges stop at the next interval boundary after auto-renewal is disabled. |
| `usage_amount > 0`   | `InvalidAmount`            | Zero or negative amounts are rejected.                 |
| Balance sufficient   | `InsufficientPrepaidBalance` | `prepaid_balance` must be ≥ `usage_amount`.           |

### Post-conditions

* `prepaid_balance` is reduced by `usage_amount`.
* If `prepaid_balance` reaches **exactly zero**, the subscription transitions
  to `InsufficientBalance`. No further charges (interval **or** usage) can
  proceed until the subscriber calls `deposit_funds` to top up.

## Interaction with Interval-Based Charging

A subscription can use **both** interval and usage billing simultaneously:

* `charge_subscription` (interval-based) debits the fixed `amount` on each
  billing cycle.
* `charge_usage` debits an arbitrary metered amount at any time.

Both draw from the same `prepaid_balance`. If either charge drains the balance
to zero, the subscription moves to `InsufficientBalance`, blocking the other
charge type as well until the subscriber tops up.

## Concurrent Usage Charge Requests

If two `charge_usage` calls for the same subscription ID are submitted
simultaneously (e.g., from parallel billing workers or within the same batch),
the outcome depends on the **reference-based idempotency key**:

### Reference-Based Replay Protection

Each usage charge request **must include a unique `reference` string** that
serves as an idempotency key. The contract enforces:

- **First request** with reference `ref_A` → Charge succeeds, reference stored
- **Duplicate request** with same reference `ref_A` → Returns `UsageChargeResult::Replay`
  (idempotent; no charge executed)
- **New request** with different reference `ref_B` → Charge succeeds if balance available

**Thread safety guarantee:** The reference uniqueness check is atomic. Two concurrent
requests with the **same reference** will deterministically have one succeed and
one fail (no double-charge).

### Example: Parallel Billing Workers

```
Worker 1: charge_usage(sub_id=42, usage_amount=100, reference="usage_20260925_batch_1_worker_a")
Worker 2: charge_usage(sub_id=42, usage_amount=150, reference="usage_20260925_batch_1_worker_b")
Result:   Both succeed independently (different references, different amounts)

Worker 1: charge_usage(sub_id=42, usage_amount=100, reference="usage_20260925_batch_1_worker_a")
Worker 1: charge_usage(sub_id=42, usage_amount=100, reference="usage_20260925_batch_1_worker_a") [retry]
Result:   First succeeds with Charged, retry returns Replay
```

### Best Practices

1. **Generate unique references per billing period/window:**
   ```
   reference = f"usage_{subscription_id}_{period_timestamp}_{worker_id}"
   ```

2. **Include worker or batch ID to distinguish parallel workers:**
   ```
   reference = f"billing_run_{batch_id}_{worker_id}_{sequence_number}"
   ```

3. **Treat `Replay` as success in idempotent contexts:**
   ```
   match charge_usage(id, amount, ref) {
       UsageChargeResult::Charged => log("charge succeeded"),
       UsageChargeResult::Replay => log("idempotent duplicate, ignoring"),
       other => log("real error", other),
   }
   ```

4. **Use subscription ID + timestamp + sequence as reference base:**
   ```
   reference = f"sub_{id}_ts_{now}_seq_{sequence}"
   ```

### Interval-Based Billing (Different Behavior)

Interval-based charges use a different replay protection mechanism:
- They track the **billing period index** (derived from elapsed time)
- A charge cannot fire twice for the same period
- No explicit reference is required (period index serves this purpose automatically)

## Integration Guide for Off-Chain Services

1. **Create a subscription** with `usage_enabled = true`.
2. **Top up** the vault via `deposit_funds` so there is sufficient
   `prepaid_balance`.
3. **Meter usage** off-chain (e.g. API calls, compute time, data transfer).
4. **Call `charge_usage`** periodically (e.g. every hour or daily) with the
   accumulated `usage_amount`.
5. **Monitor** the subscription status. When it transitions to
   `InsufficientBalance`, notify the subscriber to top up.
6. After the subscriber tops up and the status returns to `Active`, resume
   metering.

### Best Practices

* **Batch small charges**: accumulate usage off-chain and submit a single
  `charge_usage` call per period to minimise transaction fees.
* **Check balance first**: use `get_subscription` to read `prepaid_balance`
  before submitting a charge to avoid unnecessary failed transactions.
* **Use `estimate_topup_for_intervals`** alongside usage estimates to advise
  subscribers on how much to deposit.

## Error Codes

| Variant                    | Code  | Meaning                                      |
|----------------------------|-------|----------------------------------------------|
| `NotFound`                 | 404   | Subscription does not exist.                 |
| `NotActive`                | 1002  | Subscription is not in `Active` status.      |
| `UsageNotEnabled`          | 1004  | `usage_enabled` is `false` on subscription.  |
| `InvalidAmount`            | 1006  | `usage_amount` ≤ 0.                          |
| `InsufficientPrepaidBalance` | 1005 | Prepaid balance cannot cover the charge.     |

## Testing Concurrent Usage Charges

The test suite includes scenarios for concurrent usage charge handling:

| Test | Scenario | Expected Outcome |
|------|----------|------------------|
| `test_concurrent_same_reference` | Two workers submit identical charge with same reference | First succeeds with `Charged`, second returns `Replay` |
| `test_concurrent_different_references` | Two workers submit different references simultaneously | Both succeed if balance permits |
| `test_concurrent_reference_uniqueness` | Verify reference atomicity across ledger ledgers | No double-charge on same reference |
| `test_usage_after_insufficient_balance` | Concurrent charges that exhaust balance | First succeeds, second fails with `InsufficientPrepaidBalance` |
| `test_reference_replay_idempotency` | Retry same reference multiple times | All retries after first return `Replay` |

To run tests:

```bash
cargo test -p subscription_vault test_concurrent_usage
```

**Invariant maintained:** For any subscription and reference, at most one charge
executes successfully. All subsequent calls with the same reference return
`UsageChargeResult::Replay` without modifying state.
