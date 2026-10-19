# Subscriber and Merchant Blocklist

## Overview

The blocklist mechanism allows admins and merchants to prevent specific addresses (subscribers and merchants) from performing sensitive operations. For **subscribers**, the blocklist prevents creating new subscriptions and using subscriber-controlled mutations. For **merchants**, the blocklist prevents fund withdrawal while still allowing charges to accumulate.

This feature is designed for fraud prevention, dispute management, and access control while preserving existing financial obligations, balances, and an auditable admin-only unblock path.

### Blocklist Behavior Summary

| Address Type | Blocked Operations | Allowed Operations | Funds Handling |
|---|---|---|---|
| **Subscriber** | Create subscription, deposit, charge operations | Withdraw prepaid balance (fund return) | Existing balance preserved; new deposits blocked |
| **Merchant** | Withdraw earnings | Receive charges (credits accumulate) | Charges continue; withdrawal frozen until unblocked |

## Key Principles

1. **Preventive, Not Punitive**: The blocklist prevents blocked addresses from performing outbound/access-expansion operations but does not seize or move existing funds.
2. **Existing Obligations Preserved**: Blocklisted addresses retain access to existing balances and subscriptions continue to function (with subscriber charges blocked only).
3. **Dual Authorization**: Both admins (global) and merchants (scoped) can add subscribers to the blocklist.
4. **Admin-Only Removal**: Only admins can remove addresses from the blocklist.
5. **Stable Audit Trail**: Re-adding an already blocked subscriber is rejected so the original entry and reason are not overwritten.

## Authorization Model

### Admin Authorization
- **Scope**: Global - can blocklist any subscriber address
- **Operations**: Add to blocklist, remove from blocklist
- **Use Cases**: Platform-wide fraud prevention, regulatory compliance, terms of service violations

### Merchant Authorization
- **Scope**: Limited - can only blocklist subscribers with whom they have an active subscription relationship
- **Operations**: Add to blocklist only (cannot remove)
- **Use Cases**: Payment disputes, chargebacks, merchant-specific fraud prevention

## Blocklist Enforcement

### Blocked Operations

When a subscriber address is blocklisted, the following operations are **blocked**:

**Subscriber-authored flows:**
1. **`create_subscription`**: Cannot create new subscriptions
2. **`create_subscription_with_token`**: Cannot create token-specific subscriptions
3. **`create_subscription_from_plan`**: Cannot create subscriptions from plan templates
4. **`deposit_funds`**: Cannot deposit additional funds into existing subscriptions
5. **`pause_subscription`** (as subscriber): Cannot self-pause a subscription
6. **`cancel_subscription`** (as subscriber): Cannot self-cancel a subscription
7. **`resume_subscription`** (as subscriber): Cannot self-resume a subscription
8. **`migrate_subscription_to_plan`**: Cannot migrate an existing subscription to a new plan version

**Charge operations:**
9. **`charge_subscription`** / **`batch_charge`** / **`operator_batch_charge`**: Billing charges against a blocklisted subscriber's subscriptions are rejected with `Error::SubscriberBlocklisted`. The subscription remains open but cannot be billed until the subscriber is unblocked or the merchant/admin cancels it.

**Merchant withdrawal (when the merchant address is blocklisted):**
10. **`withdraw_merchant_funds`** / **`withdraw_merchant_funds_for_token`**: A blocklisted merchant address cannot withdraw earnings. Accumulated balances are preserved and released upon unblocking.

All blocked operations return `Error::SubscriberBlocklisted`.

### Allowed Operations

Blocklisted subscribers retain the following rights:

1. **`withdraw_subscriber_funds`**: Can withdraw remaining prepaid balance after cancellation. The blocklist is preventive, not punitive — existing funds are never seized.

Merchant- or admin-authorized flows against a subscription belonging to a blocklisted subscriber remain fully available. The blocklist specifically rejects blocked subscriber-authored mutation paths and prevents billing while blocked.

| Operation | Blocked subscriber as caller | Merchant/admin acting on their subscription |
|-----------|------------------------------|---------------------------------------------|
| `pause_subscription` | Blocked | Allowed |
| `cancel_subscription` | Blocked | Allowed |
| `resume_subscription` | Blocked | Allowed |
| `charge_subscription` | Blocked (any caller) | Blocked (any caller) |
| `withdraw_subscriber_funds` | Allowed (fund return) | N/A |

## Storage Schema

### Blocklist Entry

```rust
pub struct BlocklistEntry {
    pub subscriber: Address,
    pub added_by: Address,
    pub added_at: u64,
    pub reason: Option<String>,
}
```

### Storage Key

Blocklist entries are stored with the key pattern:
```
(Symbol("blocklist"), subscriber_address) -> BlocklistEntry
```

This allows O(1) lookup during subscription creation, deposit, resume, and migration checks.

## Events

### BlocklistAddedEvent

Emitted when a subscriber is added to the blocklist.

```rust
pub struct BlocklistAddedEvent {
    pub subscriber: Address,
    pub added_by: Address,
    pub timestamp: u64,
    pub reason: Option<String>,
}
```

**Topic**: `("blocklist_added",)`

### BlocklistRemovedEvent

Emitted when a subscriber is removed from the blocklist.

```rust
pub struct BlocklistRemovedEvent {
    pub subscriber: Address,
    pub removed_by: Address,
    pub timestamp: u64,
}
```

**Topic**: `("blocklist_removed",)`

## API Reference

### `add_to_blocklist`

Add a subscriber to the blocklist.

```rust
pub fn add_to_blocklist(
    env: Env,
    authorizer: Address,
    subscriber: Address,
    reason: Option<String>,
) -> Result<(), Error>
```

**Authorization**:
- Admin: Can blocklist any subscriber
- Merchant: Can only blocklist subscribers they have subscriptions with

**Errors**:
- `Error::Forbidden`: Merchant attempting to blocklist unrelated subscriber
- `Error::Unauthorized`: Invalid authorization
- `Error::InvalidInput`: Subscriber is already blocklisted

### `remove_from_blocklist`

Remove a subscriber from the blocklist. Admin only.

```rust
pub fn remove_from_blocklist(
    env: Env,
    admin: Address,
    subscriber: Address,
) -> Result<(), Error>
```

**Authorization**: Admin only

**Errors**:
- `Error::Unauthorized`: Caller is not admin
- `Error::NotFound`: Subscriber is not blocklisted

### `is_blocklisted`

Check if a subscriber is blocklisted.

```rust
pub fn is_blocklisted(
    env: Env,
    subscriber: Address,
) -> bool
```

**Returns**: `true` if subscriber is blocklisted, `false` otherwise

### `get_blocklist_entry`

Get blocklist entry details for a subscriber.

```rust
pub fn get_blocklist_entry(
    env: Env,
    subscriber: Address,
) -> Result<BlocklistEntry, Error>
```

**Errors**:
- `Error::NotFound`: Subscriber is not blocklisted

## Use Cases

### 1. Fraud Prevention (Admin)

An admin detects fraudulent activity from a subscriber address:

```rust
client.add_to_blocklist(
    &admin,
    &fraudulent_subscriber,
    &Some(String::from_str(&env, "Fraudulent chargebacks detected"))
);

let result = client.try_create_subscription(
    &fraudulent_subscriber,
    &merchant,
    &amount,
    &interval,
    &false,
    &None
);
assert_eq!(result, Err(Ok(Error::SubscriberBlocklisted)));
```

### 2. Payment Disputes (Merchant)

A merchant experiences repeated payment disputes with a subscriber:

```rust
client.add_to_blocklist(
    &merchant,
    &subscriber,
    &Some(String::from_str(&env, "Repeated payment disputes"))
);

let result = client.try_deposit_funds(&sub_id, &subscriber, &amount);
assert_eq!(result, Err(Ok(Error::SubscriberBlocklisted)));
```

### 3. Regulatory Compliance (Admin)

An admin needs to restrict access for regulatory reasons:

```rust
client.add_to_blocklist(
    &admin,
    &restricted_subscriber,
    &Some(String::from_str(&env, "Regulatory compliance - sanctioned address"))
);

client.remove_from_blocklist(&admin, &restricted_subscriber);

let new_sub_id = client.create_subscription(
    &restricted_subscriber,
    &merchant,
    &amount,
    &interval,
    &false,
    &None
);
```

## Edge Cases and Limitations

### 1. Existing Subscriptions

**Behavior**: Blocklisting does not cancel or confiscate existing subscriptions. Existing subscriptions continue to function for charging and unwind flows, but blocked subscribers cannot use subscriber-authored mutation paths that would create, restore, or reconfigure access.

**Rationale**: Preserves financial obligations and prevents unilateral fund seizure while still cutting off subscriber-controlled access expansion.

### 2. Multiple Merchants

**Behavior**: If a subscriber is blocklisted by one merchant, they cannot create subscriptions with any merchant.

**Rationale**: The blocklist is global at the contract level. Merchant authorization is for adding entries, not for scoping enforcement.

### 3. Deposit Restrictions

**Behavior**: Blocklisted subscribers cannot deposit funds into existing subscriptions.

**Rationale**: Prevents blocklisted users from extending their access through top-ups.

### 4. Resume and Migration Restrictions

**Behavior**: Blocklisted subscribers cannot resume a paused or recoverable subscription and cannot migrate a subscription to a newer plan version until an admin removes the blocklist entry.

**Rationale**: Resuming or migrating materially changes subscription access and commercial terms, so these flows are treated the same as creation and top-up paths.

### 5. Charging is Blocked for Blocklisted Subscribers (Not Merchants)

**Behavior**: Interval charges and usage charges are rejected for any subscription whose **subscriber** is blocklisted. The subscription remains open in its current state but cannot be billed until the subscriber is unblocked or the subscription is cancelled by the merchant or admin.

**Note on merchant blocklisting**: If a **merchant** is blocklisted, charges continue to succeed and credits accumulate in the vault (see section 7). The merchant simply cannot withdraw those earnings until unblocked.

**Rationale**: Charging a blocklisted subscriber against their explicit blocked status is a conflict of intent. Merchants who want to conclude a relationship with a blocked subscriber should cancel the subscription via their own authorization.

### 6. Pause and Cancel Are Blocked for Subscriber (Not Merchant/Admin)

**Behavior**: A blocklisted subscriber cannot self-pause, self-cancel, or self-resume their subscription. Merchants and admins retain full pause/cancel/resume authority over those subscriptions.

**Rationale**: Preventing subscriber-initiated mutation stops a blocked subscriber from hiding from charges (pausing) or racing to cancel before a dispute resolves. The merchant and admin can still manage the subscription lifecycle on behalf of both parties.

### 7. Merchant Withdrawal and Crediting Blocked When Merchant is Blocklisted

**Behavior**: If a merchant address is itself blocklisted (e.g., for fraud):
- The merchant cannot withdraw accumulated earnings via `withdraw_merchant_funds` or `withdraw_merchant_funds_for_token`.
- The funds remain in the vault's accounting ledger and are released when the admin removes the block.
- **Charging does not block** — subscription charges continue to succeed and credits accumulate in the vault, but the blocklisted merchant cannot retrieve them until unblocked.

**Rationale**: Unified enforcement using the same `require_not_blocklisted` guard. The blocklist enforces **outbound fund flows** (withdrawal), creating a soft freeze that preserves subscription continuity while preventing capital extraction. This allows:
- Subscriber-billed charges to complete and accumulate earnings
- Merchant earnings to be preserved during the block period
- Admin to unblock and restore merchant access without disrupting subscriptions

This is by design: the blocklist is preventive (blocks access to fund extraction), not punitive (does not seize or halt billing).

### 8. Withdrawal Rights for Subscribers

**Behavior**: Blocklisted subscribers can always call `withdraw_subscriber_funds` to reclaim their prepaid balance from a cancelled subscription.

**Rationale**: Prevents fund seizure. The blocklist is preventive, not punitive.

## Security Considerations

### 1. Authorization Checks

- All blocklist operations require proper authorization (admin or merchant)
- Merchant authorization is scoped to subscribers they have relationships with
- Removal is admin-only to prevent unauthorized unblocking

### 2. Storage Efficiency

- Blocklist uses O(1) lookup via direct address key
- No iteration required during subscription creation, deposit, resume, or migration
- Minimal gas overhead for blocklist checks

### 3. Event Transparency

- All blocklist additions and removals emit events
- Events include reason metadata for audit trails
- Off-chain systems can monitor blocklist changes

### 4. Duplicate Entry Protection

- Adding an already blocklisted subscriber is rejected with `Error::InvalidInput`
- The original `BlocklistEntry` remains unchanged, preserving `added_by`, `added_at`, and `reason`
- This prevents accidental audit-trail rewrites before an explicit admin unblock

### 5. No Retroactive Enforcement

- Blocklist does not cancel or modify existing subscriptions
- Prevents unexpected state changes for active subscriptions
- Maintains contract predictability

### 6. Merchant Blocklist Timing and Charge Continuity

- Merchant blocklist status is checked at charge entry time (for `charge_one`), preventing charges *initiated* by blocklisted merchants or their split payees.
- If a merchant is blocklisted **after** a charge completes its validation phase but **before** a subsequent charge, the new charge is rejected with `SubscriberBlocklisted`.
- Credits to blocklisted merchant sub-accounts are **not retroactively blocked** — charges that completed before blocklisting took effect have already routed their credits to sub-accounts or merchant balances. This reflects the soft-freeze semantics: blocklisting prevents **withdrawal** (outbound), not **crediting** (inbound from completed charges).
- This design preserves subscription continuity: blocklisting a merchant does not halt ongoing subscriber charges; it only freezes the merchant's ability to extract those funds.

## Testing Coverage

The blocklist implementation includes tests covering:

**Core blocklist management (`test.rs` / `test_governance.rs`):**
- Admin add and admin-only removal
- Duplicate add rejection without entry mutation
- Missing-entry unblock rejection
- Add/remove event emission and payload verification
- `None` and empty-string reason variants

**Enforcement and continuity (`test.rs`):**
- Blocked: create subscription, deposit funds
- Blocked: interval charge and usage charge while subscriber is blocklisted
- Blocked: subscriber self-pause, self-cancel, self-resume
- Allowed: merchant pause/cancel/resume on blocked subscriber's subscription
- Allowed: subscriber `withdraw_subscriber_funds` (fund-return path)
- Blocked: merchant `withdraw_merchant_funds` when the merchant address is blocklisted
- Block-after-creation: subscription balance and status preserved, subsequent deposits blocked
- Unblock restores full subscriber access (create, deposit, pause)
- Blocked merchant earnings preserved; withdrawal unlocked after admin unblocks merchant
- **`test_merchant_blocklist_charge_continuity_soft_freeze`**: Explicitly verifies that blocklisting a merchant does **not** prevent charges — charges succeed and credits accumulate, but the merchant cannot withdraw until unblocked (soft-freeze semantics)

## Operational Guidance

### For Admins

1. Document blocklist reasons whenever possible.
2. Review entries periodically and remove them only after policy review.
3. Monitor add/remove events to keep an audit trail.

### For Merchants

1. Use the blocklist as a last resort after dispute-resolution steps.
2. Escalate serious fraud cases to the admin for removal governance and platform-wide tracking.
3. Remember that merchant-added entries are enforced globally by this contract.

### For Subscribers

1. Appeal blocklist status through the contract admin.
2. Existing balances are not confiscated, but new subscriber-driven mutations stay blocked until removal.

## Future Enhancements

Potential future improvements:

1. Merchant-scoped enforcement instead of global enforcement
2. Temporary blocklist entries with expiry
3. Categorized blocklist reasons for differentiated policy
4. Batch add/remove operations
5. Export tooling for compliance reporting
