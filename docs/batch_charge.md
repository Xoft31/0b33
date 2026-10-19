# `batch_charge`

Charges multiple subscriptions in one transaction. The entrypoint is
admin-only and is guarded by the emergency stop flag.

## Signature
```rust
pub fn batch_charge(
	env: Env,
	subscription_ids: Vec<u32>,
	idem_keys: Option<Vec<BytesN<32>>>,
	nonce: u64,
) -> Result<Vec<BatchChargeResult>, Error>
```

Call `batch_charge_detailed` when operators need the offending ID in the
response. It returns `BatchChargeResponse`, whose `duplicate_id` is `Some(id)`
and whose `results` is empty when a duplicate rejects the batch. The original
`batch_charge` remains available and continues to return `Error::InvalidInput`
for duplicate IDs.

## Maximum batch size

**`BATCH_MAX_SIZE` = 100 ids per call.** The bound is a compile-time constant
(`types::BATCH_MAX_SIZE`, re-exported at the crate root as
`subscription_vault::BATCH_MAX_SIZE`) and is enforced in `do_batch_charge`
before any id is processed.

| `subscription_ids.len()` | Behaviour |
|--------------------------|-----------|
| `0` | No-op. Returns an empty vector; the nonce is **not** consumed. |
| `1 ..= 50` | Processed normally if all IDs are unique. |
| `51 ..= 100` | Processed normally if all IDs are unique and emits a `batch_size_warning` event. |
| `> 100` | Rejected wholesale with `Error::InvalidInput` (code `3002`). No id is charged, no result vector is returned, and the nonce is **not** consumed. |

Duplicate IDs are rejected wholesale before the nonce is consumed. The
`batch_charge` entrypoint returns `Error::InvalidInput`; the detailed entrypoint
returns the duplicate ID in `BatchChargeResponse.duplicate_id`.

The warning event payload includes `batch_size`, `recommended_max_size` (50),
`schema_version`, and `contract_id`. It is emitted only for a valid batch that
passes input validation and the nonce check; it does not change the result
shape or reject the batch.

### Why the cap exists

`batch_charge` charges ids **sequentially inside one transaction**. Each id
costs a bounded but non-trivial number of instructions (subscription read, fee
maths, token transfer, storage writes, event emission). Soroban's
per-transaction instruction budget is a *network* limit, not a contract limit,
so without a contract-level cap an oversized batch does not fail with a
contract error — it aborts with a generic execution error that a caller cannot
distinguish from genuine out-of-gas and cannot handle programmatically.

The cap converts that ambiguous, unrecoverable failure into a deterministic,
catchable `InvalidInput` **before** any state is touched, so the caller can
split the batch and retry with the same nonce.

### Sizing guidance

- **Hard ceiling:** 100 ids.
- **Recommended working size:** 50 ids or fewer. Per-item cost is not uniform
  — a cold subscription read is more expensive than a warm one — so a batch
  near the ceiling is more likely to approach the network instruction budget.
- **Batching pattern:** chunk a larger billing run into ceil(N / 50) calls of
  at most 50 ids each. Supply the current nonce for each chunk; it advances
  only when that chunk has at least one successful result.
- Treat 100 as a rejection threshold to stay away from, not a target.

### Oversized-batch retry

Both argument-shape checks — batch length and duplicate ids — run **before**
the nonce is consumed. A rejected batch therefore leaves no trace:

```
attempt 1: batch_charge([...101 ids...], None, nonce=7)
  → Err(InvalidInput)         // oversized
  // nonce 7 is still unused

attempt 2: batch_charge([...50 ids...], None, nonce=7)   // chunk 1
  → Ok([...])
attempt 3: batch_charge([...51 ids...], None, nonce=8)   // chunk 2
  → Ok([...])
```

Reusing `nonce = 7` after an `InvalidInput` rejection is safe and is the
recommended way to recover. In contrast, a batch that *ran* always consumes
its nonce, so a retry of a partially-successful batch needs a fresh one (see
[Retry guidance](#retry-guidance)).

For idempotency-key semantics and why double-charging is prevented, see
[`replay_protection.md`](replay_protection.md).

## Per-subscription idempotency keys

The optional `idem_keys` parameter accepts a `Vec<BytesN<32>>` that is
**parallel to `subscription_ids`** — same length, same order. When supplied,
each subscription is charged through the same idempotency-key machinery used
by the single-charge `charge_subscription` entrypoint.

### How it works

- Before `charge_one` runs for position *i*, the contract checks whether
  `idem_keys[i]` has already been recorded for `subscription_ids[i]` within
  the active TTL window (default: 7 days, configurable via `set_idempotency_ttl`).
- **Key already seen:** the charge is silently skipped. The result at position
  *i* is `success = true, error_code = 0` — an idempotent no-op.
- **Key is fresh:** if the charge succeeds, the key is recorded. On failure the
  key is not recorded (a failed charge can be retried with the same key).

Keys are scoped per `(subscription_id, domain)`, so the same 32-byte value can
be reused safely across different subscriptions or different entrypoints without
collision.

### When to use idem_keys

The primary use case is **timeout-uncertain retries**: an off-chain billing
engine submits a batch and receives no response (network timeout). It cannot
tell whether the transaction landed. With `idem_keys`:

```
// First attempt (outcome unknown — timeout):
batch_charge(ids=[10, 11, 12], idem_keys=[k10, k11, k12], nonce=5)
  → timeout, no response

// Retry with a fresh nonce; same idem_keys:
batch_charge(ids=[10, 11, 12], idem_keys=[k10, k11, k12], nonce=6)
  → Ok([{success:true,..}, {success:true,..}, {success:true,..}])
  // IDs that were already charged on attempt 1 are silently skipped.
  // IDs that were not charged are charged now.
```

Without `idem_keys`, the caller must either use off-chain event indexing to
determine which IDs were charged, or risk double-charging by blindly
resubmitting all IDs.

### Length validation

When `idem_keys` is `Some`, its length **must equal** `subscription_ids.len()`.
A mismatch is rejected with `Error::InvalidInput` before the nonce is consumed,
so the caller can correct the input and retry with the same nonce.

When `idem_keys` is `None`, behaviour is identical to batches submitted before
this parameter was introduced: no per-ID replay protection beyond the batch nonce.

### Key generation guidance

- Use a **cryptographically random 32-byte value** per `(subscription_id, billing_cycle)`.
- A simple construction: `SHA-256(scheduler_run_id || subscription_id || period_start_timestamp)`.
- Do **not** reuse the same key across different billing periods for the same
  subscription — the TTL window (default 7 days) protects within a period, but
  deliberate reuse across periods would silently skip legitimate charges.
- Store keys alongside the billing job record so they survive the scheduler
  restart that necessitates a retry.

## Partial-success model

Admin authentication and batch nonce validation happen once at the batch
boundary. Each ID is then processed independently through the shared
`charge_one` path. An item failure does not roll back successful items or abort
the remaining items. For a valid (duplicate-free) batch, the call returns
exactly one result for each input ID, including IDs that are missing.

## `BatchChargeResult`

The result vector has the same order and length as `subscription_ids`:

| Field | Type | Values |
|-------|------|--------|
| `subscription_id` | `u32` | The input subscription ID this result describes |
| `success` | `bool` | `true` when that item completed without an item-level error |
| `error_code` | `u32` | `0` on success; otherwise the corresponding `Error` code |

Successful interval charges mutate only their own subscription and accounting
state. Failed items return their error code and retain the single-charge
semantics, including any per-item lifecycle transition such as entering
`GracePeriod` or `InsufficientBalance`.

## Ordering guarantees

Results are appended while iterating `subscription_ids`, and each result also
contains its `subscription_id`, so callers can associate outcomes without
relying only on vector position. Duplicate IDs reject the entire batch before
processing begins; use `batch_charge_detailed` to identify the repeated ID.

## Skip conditions
- Subscription not found
- Status is Paused, Cancelled, or InsufficientBalance
- Billing interval has not elapsed
- Insufficient prepaid balance (also applies the lifecycle grace rule)

## Retry guidance

The batch nonce is consumed only when at least one result has `success = true`.
If every result is an individual failure, the nonce remains available, so a
corrected retry may reuse it. Item-level failures can still emit diagnostics
or apply lifecycle transitions (for example, marking an expired subscription
as expired); nonce reuse does not roll back those per-item effects. A batch
with any successful result consumes its nonce, and a retry must use a fresh
one. Prefer retrying only failed IDs after correcting their cause:
top up and explicitly resume an underfunded subscription, wait for an interval
that has not elapsed, or remove/replace an ID that no longer exists. Do not
blindly retry successful IDs, because their next result may be
`IntervalNotElapsed` or another state-dependent outcome.

**Timeout-uncertain retries with `idem_keys`:** if the original transaction's
outcome is unknown (network timeout), pass the same `idem_keys` with a fresh
nonce. Already-charged subscriptions are silently skipped; uncharged ones are
processed normally. See [Per-subscription idempotency keys](#per-subscription-idempotency-keys)
for key generation guidance.

The outer `Result` is an error rather than a result vector when the emergency
stop is active, authentication fails, or the batch nonce is invalid. In
particular, enabling the emergency stop prevents any item from being charged;
it does not produce partial per-item results. Pause, resume, cancel, and query
operations remain available according to the emergency-stop policy.
