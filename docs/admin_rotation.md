# Admin Rotation and Access Control

## Overview

The Subscription Vault contract uses a single admin address stored in instance
storage under `DataKey::Admin`. Admin rotation allows the current administrator
to transfer all privileges to a new address in a single atomic transaction.
All admin-protected operations read from this single canonical key.

## Admin-Protected Operations

| Operation | Auth pattern | Purpose |
|-----------|-------------|---------|
| `set_min_topup` | explicit admin param | Configure minimum deposit amount |
| `set_grace_period` | explicit admin param | Configure grace period duration |
| `add_accepted_token` | explicit admin param | Add a new accepted payment token |
| `remove_accepted_token` | explicit admin param | Remove an accepted payment token |
| `recover_stranded_funds` | explicit admin param | Recover excess funds in emergency scenarios |
| `rotate_admin` | explicit admin param | Transfer administrative privileges |
| `enable_emergency_stop` | explicit admin param | Pause all critical operations |
| `disable_emergency_stop` | explicit admin param | Resume normal operations |
| `set_protocol_fee` | explicit admin param | Configure protocol fee and treasury |
| `add_to_blocklist` | explicit admin param | Block a subscriber (admin path) |
| `remove_from_blocklist` | explicit admin param | Unblock a subscriber |
| `batch_charge` | stored admin auth | Charge multiple subscriptions |
| `charge_subscription` | stored admin auth | Charge a single subscription |
| `charge_usage` | stored admin auth | Charge usage-based billing |
| `export_contract_snapshot` | explicit admin param | Export audit snapshot |
| `set_billing_retention` | explicit admin param | Configure billing retention |
| `set_oracle_config` | explicit admin param | Configure oracle pricing |

**Auth patterns:**
- **Explicit admin param**: caller passes `admin: Address`; contract calls
  `admin.require_auth()` then checks `admin == stored_admin`.
- **Stored admin auth**: contract loads the stored admin, calls `require_auth()`
  on it. After rotation the stored admin is the new admin, so only the new
  admin's signature satisfies the check.

## Rotation Procedure

### Prerequisites

- You are the **current admin** (the address stored under `DataKey::Admin`).
- You hold the signing keys or multisig authorization for the current admin.
- You know the current nonce for `(current_admin, DOMAIN_ADMIN_ROTATION)`.
  Read it with `get_admin_nonce(current_admin, 1)`.

### Function signature

```rust
pub fn rotate_admin(
    env: Env,
    current_admin: Address,   // must match stored admin; must sign the tx
    new_admin: Address,       // the address that will become admin
    nonce: u64,               // must equal get_admin_nonce(current_admin, 1)
) -> Result<(), Error>
```

### Steps

1. **Query the current nonce**
   ```
   nonce = get_admin_nonce(current_admin, 1)   // domain 1 = DOMAIN_ADMIN_ROTATION
   ```

2. **Call `rotate_admin`**
   - `current_admin`: the currently stored admin address (must sign the tx).
   - `new_admin`: the address that will become admin.
   - `nonce`: the value returned in step 1.

3. **Effect**
   - `DataKey::Admin` is updated to `new_admin` atomically.
   - `current_admin`'s rotation nonce advances to `nonce + 1`.
   - An `admin_rotated` event is emitted (see below).
   - Previous admin loses all privileges instantly; new admin gains them
     in the same transaction.

### Post-Rotation

- Confirm with `get_admin()`.
- Monitor `admin_rotated` events for audit and indexing.

## Event Payload

Every successful rotation emits an `AdminRotatedEvent` under the
`admin_rotated` topic:

```json
{
  "topic": "admin_rotated",
  "old_admin": "<Address>",
  "new_admin": "<Address>",
  "timestamp": <u64>
}
```

Rust type:
```rust
pub struct AdminRotatedEvent {
    pub old_admin: Address,
    pub new_admin: Address,
    pub timestamp: u64,
}
```

A `NonceConsumedEvent` is emitted first (by the replay-protection layer):

```json
{
  "topic": "nonce_consumed",
  "signer": "<current_admin Address>",
  "domain": 1,
  "nonce": <u64>,
  "timestamp": <u64>
}
```

## Nonce Protection

`rotate_admin` is protected by a per-`(signer, domain)` monotonic counter
stored in persistent storage.

- **Domain**: `DOMAIN_ADMIN_ROTATION = 1` (separate from `DOMAIN_BATCH_CHARGE = 0`).
- **Starting value**: `0` for every new address.
- **Acceptance rule**: `provided_nonce == stored_nonce` exactly; any other value
  returns `Error::NonceAlreadyUsed`.
- **Advance rule**: on success the counter is incremented to `stored + 1` before
  the event is emitted (effects-before-interactions).
- **Per-address independence**: each admin address has its own counter. A newly
  appointed admin always starts at `0`, independent of any predecessor.

This prevents:
1. **Replay attacks** — resubmitting a captured transaction after a key compromise.
2. **Cross-domain collisions** — a batch-charge nonce cannot be reused for rotation.
3. **Out-of-order execution** — skipping nonces is rejected, preserving sequencing.

## Risks

### Irreversibility

Rotation is **irreversible** without the new admin's cooperation. If the new
admin's keys are lost, privileged operations cannot be performed.

### No Grace Period

The change takes effect in the same transaction. There is no delay or
confirmation step. Verify `new_admin` carefully before submitting.

### Self-rotation and Contract Locking

Both are explicitly rejected:
- `new_admin == current_admin` → `Error::SelfRotation`
- `new_admin == env.current_contract_address()` → `Error::InvalidNewAdmin`

## Two-Step Rotation and Rollback

`rotate_admin` (above) is the **instant** path: one transaction, no
confirmation step, no way back. For anything routine — key rotation, moving to
a multisig, a planned handover — use the two-step flow instead, which puts a
verifiable window between "I propose you" and "you become admin".

### The two-step flow

```rust
// Step 1 — current admin proposes. Nonce-free; see "Nonce Protection".
pub fn propose_admin(env: Env, current_admin: Address, new_admin: Address)
    -> Result<(), Error>;

// Step 2a — the proposed admin claims, becoming the stored admin.
pub fn claim_admin_role(env: Env, claimant: Address) -> Result<(), Error>;

// Step 2b — the current admin cancels the proposal instead.
pub fn cancel_admin_proposal(env: Env, admin: Address) -> Result<(), Error>;

// Read the pending proposal (or `None`).
pub fn get_admin_proposal(env: Env) -> Option<AdminProposal>;
```

`propose_admin` writes an `AdminProposal { new_admin, proposed_at, expires_at }`
to **instance** storage with
`expires_at = proposed_at + 7 days` (`PROPOSAL_WINDOW_SECS`). It emits
`admin_proposal_created`.

`claim_admin_role` can only succeed for the exact address in the proposal, and
only after a 24-hour cooldown has elapsed since the proposal was created
(`ADMIN_PROPOSAL_COOLDOWN_SECS`).

| Step | Rejects with |
|------|---------------|
| `propose_admin` | `Unauthorized` (caller is not the stored admin), `InvalidNewAdmin` (`new_admin == contract address`), `ProposalAlreadyExists` (a proposal is already pending *and still inside its window* — an expired one is replaced) |
| `claim_admin_role` | `ProposalNotFound`, `ProposalCooldownActive` (within 24 h of the proposal), `ProposalExpired` (also clears the stale proposal), `InvalidClaimant` (not the proposed address) |
| `cancel_admin_proposal` | `Unauthorized` (caller is not the stored admin), `NoActiveProposal` (nothing pending) |

Only one *claimable* proposal can exist at a time — a second `propose_admin`
fails with `ProposalAlreadyExists` until the first is claimed, cancelled, or
expires. An expired proposal is dead: it can never be claimed again, so
`propose_admin` treats it as absent and overwrites it with a fresh proposal
(and a fresh 7-day window) rather than returning `ProposalAlreadyExists`
forever and forcing the admin to cancel first.

The boundary is inclusive on the claim side: a proposal whose
`expires_at == ledger.timestamp()` is still claimable, and it is only replaced
once `ledger.timestamp() > expires_at`.

### Rollback: the new admin key is compromised *before* acceptance

**A rollback path is available. `cancel_admin_proposal` exists and is
callable by the current admin at any time before the proposal is claimed.**

This is the scenario the issue asks about, and it is fully recoverable:

```
1. propose_admin(current_admin, new_admin)        → admin_proposal_created
   ... new_admin's key is compromised, or was mistyped,
       or was never actually controlled by the intended party ...

2. cancel_admin_proposal(current_admin)           → admin_proposal_cancelled
   → the pending proposal is removed. Nothing else changes.
   → current_admin remains the stored admin, unchanged.

3. Re-propose correctly, or use rotate_admin if the window is not needed.
```

Properties that matter for an incident:

- **No grace period, no race.** `claim_admin_role` requires a 24-hour cooldown
  after the proposal was created. Cancelling well inside that window means the
  new admin provably cannot have claimed, so the rollback is race-free. Verify
  by reading `get_admin_proposal` before acting.
- **Total no-op on everything else.** `cancel_admin_proposal` removes exactly
  one instance-storage key and emits one event. It does not touch
  `DataKey::Admin`, subscriptions, balances, nonces, or any other config.
- **Admin authority is never lost.** Only the *stored* admin may cancel
  (`Unauthorized` otherwise), and cancelling does not change who that is. The
  current admin retains full authority throughout.
- **Repeatable.** After cancelling, `propose_admin` succeeds again immediately
  — there is no cooldown on the propose/cancel pair.
- **No nonce consumed.** Unlike `rotate_admin`, neither `propose_admin` nor
  `cancel_admin_proposal` uses the admin nonce (domain 1), so cancelling
  cannot be blocked by a stale nonce and does not consume one.

⚠ **The window is not infinite.** If the 24-hour cooldown has already elapsed
*and* the new admin has already called `claim_admin_role`, the rollback is
gone — see [Rollback after acceptance](#rollback-after-acceptance) below.
**Cancel early.** Treat the cooldown as the deadline, not the 7-day expiry.

### Emergency stop interaction

All admin-transfer paths (`rotate_admin`, `propose_admin`, and
`claim_admin_role`) return `Error::EmergencyStopActive` while the emergency
stop is active. This prevents a pending or newly created transfer from
changing administrative control during an incident. The current admin may
still call `cancel_admin_proposal` to revoke a pending transfer.

After the incident is resolved and the stop has been disabled, use
`rotate_admin` when an immediate handoff is necessary or the stored admin is a
contract/governance address that cannot use the two-step flow.

`rotate_admin` **is** nonce-gated (`DOMAIN_ADMIN_ROTATION`, read with
`get_admin_nonce(current_admin, 1)`), so query the nonce immediately before
signing.

### Rollback after acceptance

Once `claim_admin_role` has succeeded there is **no rollback**. The stored
admin is now the new admin, and the old admin's address returns
`Error::Unauthorized` on every admin-only entrypoint. The only recovery is the
new admin voluntarily proposing/rotating back.

This is the irreducible risk of the two-step flow, and it is why:

- The new admin should be a **multisig or governance address**.
- The old admin should independently confirm the new admin can actually sign
  (and can actually sign a *rejection*) before proposing.
- Cancellation should be treated as a normal, cheap operation — not an
  admission of failure.

For the full operational procedure, including the per-step CLI invocations and
error-code table, see [`runbooks/admin_rotation.md`](runbooks/admin_rotation.md)
→ *Step 3B: Cancel* and *§4 Rollback Path*.

## Access Control Matrix

| Operation | Current Admin | Previous Admin | Non-Admin |
|-----------|:---:|:---:|:---:|
| `rotate_admin` | ✓ | ✗ `Unauthorized` | ✗ `Unauthorized` |
| `propose_admin` | ✓ | ✗ `Unauthorized` | ✗ `Unauthorized` |
| `cancel_admin_proposal` | ✓ | ✗ `Unauthorized` | ✗ `Unauthorized` |
| `claim_admin_role` | ✗ (must be the *proposed* address) | ✗ `InvalidClaimant` | ✗ `InvalidClaimant` |
| `get_admin_proposal` | ✓ (read-only, no auth) | ✓ | ✓ |
| `set_min_topup` | ✓ | ✗ `Unauthorized` | ✗ `Unauthorized` |
| `set_grace_period` | ✓ | ✗ `Unauthorized` | ✗ `Unauthorized` |
| `add_accepted_token` | ✓ | ✗ `Unauthorized` | ✗ `Unauthorized` |
| `remove_accepted_token` | ✓ | ✗ `Unauthorized` | ✗ `Unauthorized` |
| `recover_stranded_funds` | ✓ | ✗ `Unauthorized` | ✗ `Unauthorized` |
| `enable_emergency_stop` | ✓ | ✗ `Unauthorized` | ✗ `Unauthorized` |
| `disable_emergency_stop` | ✓ | ✗ `Unauthorized` | ✗ `Unauthorized` |
| `set_protocol_fee` | ✓ | ✗ `Unauthorized` | ✗ `Unauthorized` |
| `batch_charge` | ✓ (via stored auth) | ✗ | ✗ |
| `charge_subscription` | ✓ (via stored auth) | ✗ | ✗ |
| `charge_usage` | ✓ (via stored auth) | ✗ | ✗ |

## Security Notes

### Single Canonical Key

There is exactly one admin stored at `DataKey::Admin`. There is no secondary
admin path, cached admin, or fallback. Every admin-protected operation reads
from this key.

### No Backdoor for Previous Admins

Once rotation completes, the previous admin's address returns
`Error::Unauthorized` on every admin-only entrypoint. There is no grace period,
delayed revocation, or override.

### Rotation Cannot Brick the Contract

Two guards prevent permanently locking admin access:
1. Self-rotation is rejected (`Error::SelfRotation`).
2. Rotation to the contract address is rejected (`Error::InvalidNewAdmin`),
   since the contract cannot produce Soroban auth signatures.

### Subscription and Balance Isolation

Rotation does not touch subscription storage, prepaid balances, or merchant
balances. Active subscriptions continue normally; pending charges remain
chargeable by the new admin via `batch_charge`.

## Best Practices

1. **Use a multisig or governance address** for the production admin.
2. **Query the nonce** with `get_admin_nonce(admin, 1)` before every rotation.
3. **Verify `new_admin`** controls the private key before signing.
4. **Monitor `admin_rotated` events** for real-time audit and alerting.
5. **Rotate during low-activity periods** to minimize operational risk.
6. **Maintain an off-chain record** of all rotations and admin addresses.

## Test Coverage

Admin rotation is covered by three test files:

| File | Focus |
|------|-------|
| `test_governance.rs` (module `admin_rotation_invariants`) | Invariant checks, event payload, emergency-stop integration, pending charges |
| `test_replay_protection.rs` | Nonce mechanics, domain separation, sequential nonces, event emission |
| `test.rs` (section `-- Admin tests --`) | Access control matrix, multi-rotation chains, subscription isolation |

Run all tests:

```bash
cargo test -p subscription_vault
```

Measure coverage:

```bash
cargo install cargo-tarpaulin
cargo tarpaulin -p subscription_vault --out Stdout
```

## Related Documentation

- [Admin Rotation Tests](./admin_rotation_tests.md) — Test coverage details.
- [Recovery](./recovery.md) — Admin recovery of stranded funds.
- [Events](./events.md) — Full event schema reference.
- [Admin Authorization Matrix](./admin_authorization_matrix.md) — Per-endpoint auth details.
