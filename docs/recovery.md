# Admin Recovery of Stranded Funds

## Overview

The SubscriptionVault contract includes a tightly scoped administrative recovery mechanism for handling funds that become inaccessible through normal contract operations. This document outlines when recovery is appropriate, how it works, and the security controls in place.

## Purpose

Despite careful contract design, funds can become stranded in several scenarios:

- **Accidental transfers**: Users send tokens directly to the contract address by mistake
- **Deprecated flows**: Contract upgrades or bug fixes leave funds in an inaccessible state
- **Unreachable addresses**: A subscriber may be unable to authorize their own
   withdrawal after cancellation; this does not make an accounted balance
   eligible for admin recovery.

The recovery mechanism provides a last-resort option to prevent permanent fund loss while maintaining strong security guarantees.

## Operational Procedures

There are two distinct recovery paths. Admin recovery transfers only unaccounted
surplus under governance controls; subscriber withdrawal returns funds still
owed to that subscriber. Admin recovery cannot substitute for a subscriber
withdrawal or redirect an accounted balance to a new address.

### Admin-initiated stranded-fund recovery

1. **Establish eligibility.** Identify the token, exact amount, recipient, and
   applicable `RecoveryReason`. Confirm that the funds are not represented by
   subscriber prepaid balances, merchant liabilities, or cancellation escrow,
   and that no normal contract operation can release them. For `ExpiredEscrow`,
   also document that the escrow amount is no longer included in
   `total_accounted`; an elapsed escrow window or an unreachable subscriber is
   not enough by itself.
2. **Reconcile and prepare approval.** Call the public,
   unauthenticated `get_token_reconciliation(token)` query. Confirm
   `is_balanced == true` and preserve the returned values as evidence. The
   recovery is capped at `recoverable_amount`, which excludes prepaid balances
   and other accounted liabilities. Where guardians are configured, create a
   `RecoverStrandedFunds` governance proposal matching the recipient, token,
   and amount expected to be transferred after that cap; it must pass quorum
   and its timelock before execution. Without configured guardians, the
   contract retains its single-admin compatibility path.
3. **Submit the recovery.** The configured admin calls
   `recover_stranded_funds(admin, token, recipient, amount, recovery_id,
   reason)` and authorizes the transaction with the admin key. Use a unique
   `recovery_id`; the contract rejects reuse. The result reports
   `recovered_amount` and `surplus_amount` (the requested amount that exceeded
   the available recoverable balance). When guardians are configured, the
   proposal must match the capped transfer amount and be approved before the
   call.
4. **Verify the transaction and event.** Confirm the transaction succeeded and
   inspect its `recovery` event. Check the admin, recipient, token, amount,
   reason, and timestamp against the proposal and incident record. The token
   transfer and event are part of the same transaction, so a failed transfer
   must not be treated as a completed recovery.
5. **Verify balances and close the record.** Confirm the recipient's token
   balance increased by the recovered amount, then query
   `get_token_reconciliation(token)` again and retain the result and transaction
   hash. Record the `recovery_id`, proposal, evidence, event, and post-recovery
   reconciliation in the recovery log.

### Subscriber-initiated withdrawal

1. **Identify the subscriber's funds.** Read the subscription and determine
   whether the amount remains in `prepaid_balance` or is held in its
   cancellation escrow. Withdrawal is allowed when the subscription is
   `Cancelled`, `Expired`, or `Archived`, or when its expiration condition has
   been reached. The caller must be the subscriber recorded on the subscription
   (and on the escrow, when withdrawing escrowed funds).
2. **Check escrow conditions, if applicable.** For cancellation escrow, wait
   until its `released_at` timestamp and resolve any open dispute first. An
   early attempt fails with `EscrowNotReleased`; an open dispute fails with
   `DisputeAlreadyOpen`. These conditions are enforced on-chain.
3. **Authorize and withdraw.** The subscriber calls
   `withdraw_subscriber_funds(subscription_id, subscriber)` and signs with the
   subscriber key. No admin signature or recovery proposal is used. The call
   transfers the full eligible prepaid balance or escrow amount to that
   subscriber and clears the corresponding contract record. If the subscriber
   has lost the key, an admin cannot use this entrypoint on their behalf.
4. **Verify the withdrawal.** Confirm the transaction succeeded and inspect the
   `sub_withdrawn` event for the subscription id, subscriber, token, and amount.
   For an escrow withdrawal, also confirm the
   `cancellation_escrow_released` event. Verify the subscriber's token balance
   increased and, for a prepaid-balance withdrawal, confirm the balance is now
   zero. Retain the transaction hash and event data.

### Identifying stranded subscriptions (`list_recoverable_subscriptions`)

There is no secondary index of cancelled subscriptions — `apply_cancellation`
removes the id from `MerchantSubs`, `TokenSubs` and `SubscriberSubs` — so
finding them requires an id-range scan. The contract exposes that scan as a
view:

```rust
list_recoverable_subscriptions(
    cursor: Option<u32>,
    limit: u32,
) -> RecoverableSubscriptionsPage

pub struct RecoverableSubscriptionsPage {
    pub subscriptions: Vec<RecoverableSubscription>, // id, subscriber, merchant, token, prepaid_balance
    pub next_cursor: Option<u32>,
}
```

A subscription is reported when **`status == Cancelled` and
`prepaid_balance > 0`**. `cursor` is an inclusive lower bound on the
subscription id (`None` starts at `0`); `limit` must be in
`1..=MAX_QUERY_RESULTS` or the call fails with `Error::InvalidExportLimit`.
Results are ascending by id, and each call inspects at most `MAX_SCAN_DEPTH`
ids. Continue from `next_cursor` while it is `Some`; when it is `None` the scan
reached the end of the id space, so an empty page with `None` means nothing is
stranded.

Note the distinction from `list_subscriptions_by_subscriber`: that one reads a
per-subscriber index and cannot see a cancelled subscription at all, because
the id has already been removed from the index.

**What a hit means.** In the normal flow `cancel_subscription` zeroes
`prepaid_balance` and moves the amount into `DataKey::CancellationEscrow`, so a
match indicates money that reached a cancelled record outside that path:

- a **delegated-payer deposit** (`deposit_funds_on_behalf`) that lands after
  cancellation, since that path does not gate on `Cancelled`;
- a record restored from a snapshot export whose escrow accounting was not
  reproduced by the restore;
- state migrated from an older storage schema.

**What a hit does not mean.** This query is a reporting and forensics aid. It
confers no right to move the funds. `recover_stranded_funds` remains
structurally incapable of touching an accounted balance, and the legitimate
route for a cancelled subscription's money is still the
subscriber-authenticated `withdraw_subscriber_funds`. A cancelled subscription
has no `uncancel` path either — `Cancelled` may only transition to `Archived`.

---

## Recovery Scenarios

### Valid Use Cases

The contract defines four specific recovery reasons, each representing a well-documented scenario:

#### 1. UserOverpayment

**When to use**: Tokens sent directly to the contract address by mistake, not associated with any subscription.

**Example**: A user copies the contract address instead of their merchant address and sends 100 USDC directly to the contract.

**Verification steps**:

- Check transaction history to confirm the transfer
- Verify no subscription exists for the sending address
- Confirm the funds are not part of any subscription balance

#### 2. FailedTransfer

**When to use**: Funds stranded due to transfer failures or stalled in an unexpected state.

**Example**: A contract upgrade changes the withdrawal flow, or an indexer missed a step, leaving some funds in an old storage pattern that's no longer accessible.

**Verification steps**:

- Document the specific bug or upgrade that caused the issue
- Identify the exact amount and location of stranded funds
- Confirm the funds cannot be recovered through normal contract operations

#### 3. ExpiredEscrow

**When to use**: A cancellation escrow or subscription balance became
unaccounted because a bug, migration, or state restore dropped its accounting
record, and the funds are therefore included in the token's recoverable
surplus. This is not a path for an escrow that remains properly accounted.

**Example**: A migration defect leaves the escrow tokens in the contract but
drops the record that included them in `total_accounted`. If the escrow remains
accounted, only the subscriber can withdraw it through
`withdraw_subscriber_funds`.

**Verification steps**:

- Confirm the subscription is in Cancelled status
- Document evidence the subscriber has lost key access (community request, time elapsed, etc.)
- Verify the subscriber's identity through alternative means if possible

#### 4. SystemCorrection

**When to use**: System or logic corrections that require manual intervention by the admin.

**Example**: A bug caused an incorrect amount of tokens to be deducted from the total accounted balance, requiring an admin to recover the excess tokens.

**Verification steps**:

- Verify the specific system error and calculate the exact delta.
- Document the root cause and ensure a patch is deployed.

### Invalid Use Cases

Recovery should **NOT** be used for:

- ❌ Active subscriptions with accessible subscribers
- ❌ Merchant balances that can be normally withdrawn
- ❌ Disputes between subscribers and merchants
- ❌ Regular contract operations or maintenance
- ❌ "Borrowing" funds temporarily with intent to return
- ❌ **Funds owed to a cancelled subscriber** — see
  [Recovery after subscription cancellation](#recovery-after-subscription-cancellation)

---

## Recovery after subscription cancellation

**Rule: `recover_stranded_funds` is *not* a recovery path for a cancelled
subscription. A cancelled subscription's funds are returned to the subscriber
through `withdraw_subscriber_funds`, and this entrypoint is structurally
incapable of taking them.**

This section is normative: the behaviour below is what the implementation does
today, and integrators and support staff should plan around it rather than
discovering it at incident time.

### Why there is no status gate

`recover_stranded_funds` does not read a subscription's status. It is not a
per-subscription entrypoint at all — it takes an `amount` and a `recipient`,
and has no `subscription_id` parameter. The only balance gate is:

```rust
recoverable = contract_balance - total_accounted(token)
recovered_amount = min(amount, max(recoverable, 0))
surplus_amount = amount - recovered_amount
```

`total_accounted` aggregates every subscriber prepaid balance, every merchant
balance, and every open cancellation escrow. So the exclusion of a cancelled
subscription's funds is **accounting-based, not status-based**: as long as the
money is still attributed to someone, it is subtracted out of
`recoverable` and cannot be recovered, regardless of the subscription's
lifecycle status.

There is therefore no code path in which "the subscription is `Cancelled`"
changes the outcome of a recovery attempt.

### Lifecycle of a cancelled subscription's funds

| Stage | Where the funds are | In `total_accounted`? | Recoverable? | Correct action |
|-------|---------------------|-----------------------|--------------|----------------|
| 1. `Active` with a prepaid balance | `Subscription.prepaid_balance` | Yes | **No** | Normal billing. |
| 2. `cancel_subscription` succeeds | Zeroed on the record, moved to `DataKey::CancellationEscrow(subscription_id)` | **Yes** (escrowed, not yet transferred) | **No** | Wait out the dispute window. Nothing to recover. |
| 3. Escrow window elapsed, subscriber withdraws | Transferred to the subscriber; `sub_total_accounted` decremented | No | n/a — balance left the contract | Done. |
| 4. Escrow window elapsed, subscriber never withdraws | Still in `CancellationEscrow` | **Yes** | **No** | Subscriber calls `withdraw_subscriber_funds` or `claim_cancellation_escrow`. The subscriber-authenticated claim refreshes the escrow TTL. Admin cannot claim it. |

Note stage 3: the escrow release decrements `total_accounted` at *withdrawal*
time, not at cancellation time. So cancelling a subscription does not change
the recoverable surplus at all — contract balance and accounted move down
together when the refund is finally paid out. Cancellation never creates
recoverable funds.

### The one legitimate post-cancellation case: `ExpiredEscrow`

The `ExpiredEscrow` reason (§3 above) is the **only** sanctioned way to touch
money connected to a cancelled subscription, and it applies to a narrower
situation than it first appears: funds that are already *unaccounted* because
the record that accounted for them no longer exists.

Concretely, this is for the case where an escrow (or a subscription balance)
became permanently unreachable through a bug, a migration, or a state-export /
state-restore that dropped the accounting entry — the amount is still in the
contract but `total_accounted` no longer reserves it, so it legitimately shows
up in `recoverable`.

It is **not** a way to shortcut an escrow that is still properly accounted for.
Before filing an `ExpiredEscrow` proposal you must show, with on-chain
evidence, that:

1. The subscription is `Cancelled` and the escrow window has fully elapsed.
2. The subscriber cannot be reached or has provably abandoned the refund.
3. **The escrowed amount is not present in `total_accounted`** — i.e. the
   attempt is not being blocked because someone else is still owed the money.
4. An open dispute does not exist for that subscription (a dispute blocks
   withdrawal and must be resolved first).

If step 3 cannot be demonstrated, the correct path is to let the subscriber
withdraw. A recovery request cannot draw on accounted funds; if no unaccounted
balance exists, it returns `recovered_amount: 0` and reports the request as
`surplus_amount`.

### What an admin sees when they try

| Attempt | Result |
|---------|--------|
| Recover the refund of a `Cancelled` subscription whose escrow is still accounted | Transfers no escrowed funds; reports `recovered_amount: 0` if no unrelated recoverable balance exists |
| Recover the refund of a `Cancelled` subscription after the subscriber already withdrew | Transfers no funds if the contract has no unaccounted balance |
| Request more than an unrelated surplus | Succeeds only for the available surplus and reports the excess request as `surplus_amount`. The escrowed portion is never included. Do not net the two: net recoveries look like theft in the audit trail. |
| Recover a genuinely orphaned, unaccounted escrow after an upgrade/migration bug | Succeeds with `RecoveryReason::ExpiredEscrow` and the evidence above |

### Support checklist for a cancelled subscription

1. Read the status. `Cancelled` is terminal — there is no `uncancel`
   entrypoint, so the subscription will never bill again.
2. Check the escrow. If `CancellationEscrowOpenedEvent` exists for the id and
   no `CancellationEscrowReleasedEvent` follows it, the refund is still
   escrowed and **not** admin-recoverable.
3. Check `now < released_at`. Before the window elapses, withdrawal fails with
   `Error::EscrowNotReleased`; the wait is enforced on-chain, not administrative.
4. Check for an open dispute. `withdraw_subscriber_funds` returns
   `Error::DisputeAlreadyOpen` while a dispute is live; resolve the dispute
   first.
5. If the window has elapsed and no dispute is open, the subscriber should call
   `withdraw_subscriber_funds`. If the subscriber is unreachable, escalate to
   an `ExpiredEscrow` governance proposal **only** after evidencing that the
   amount is no longer in `total_accounted`.
6. Use `get_token_reconciliation` to confirm the numbers before filing any
   proposal — it reports the contract balance, `total_accounted`, and the
   recoverable surplus per token, which is the same arithmetic
   `recover_stranded_funds` gates on. For the operational procedure around
   reconciling balances, see [`reconciliation_strategy.md`](reconciliation_strategy.md).

---

## Technical Implementation

### Function Signature

```rust
pub fn recover_stranded_funds(
    env: Env,
    admin: Address,
    token: Address,
    recipient: Address,
    amount: i128,
    recovery_id: String,
    reason: RecoveryReason,
) -> Result<(), Error>
```

### Security Controls

#### 1. Admin Authorization

- Only the configured admin address can invoke recovery
- Requires cryptographic signature from the admin key
- Admin key should be a multi-signature wallet or hardware wallet

```rust
admin.require_auth();
let stored_admin = env.storage().instance().get(&Symbol::new(&env, "admin"))?;
if admin != stored_admin {
    return Err(Error::Unauthorized);
}
```

#### 2. Amount Validation

- Amount must be positive (> 0)
- Prevents accidental calls with zero or negative values
- Recovery is capped at the available recoverable balance
- Any excess request is reported as `surplus_amount` in `RecoveryResult`
- Recoverable balance is calculated as `actual_token_balance - total_accounted_balance` to ensure user and merchant active funds are strictly protected.

```rust
if amount <= 0 {
    return Err(Error::InvalidRecoveryAmount);
}

let recoverable = contract_balance.checked_sub(accounted_balance)?.max(0);
let recovered_amount = amount.min(recoverable);
let surplus_amount = amount - recovered_amount;
```

#### 2.1 Replay Protection

- Every recovery requires a unique `recovery_id`
- Prevents duplicate execution of the same recovery operation

```rust
let recovery_key = DataKey::Recovery(recovery_id.clone());
if env.storage().persistent().has(&recovery_key) {
    return Err(Error::Replay);
}
env.storage().persistent().set(&recovery_key, &true);
```

#### 3. Audit Trail

Every recovery operation emits a `RecoveryEvent` containing:

- Admin address (who authorized)
- Token address
- Recipient address (where funds went)
- Amount recovered
- Recovery reason (why it was done)
- Timestamp (when it occurred)

**Backend Indexer Guidance**:
Indexers should listen for the `recovery` topic. The event payload is a `RecoveryEvent` struct. Indexers should log the `recovery_id` (from transaction parameters) alongside the event to ensure a complete audit trail and match it with governance proposals.

```rust
let recovery_event = RecoveryEvent {
    admin: admin.clone(),
    recipient: recipient.clone(),
    token: token.clone(),
    amount,
    reason: reason.clone(),
    timestamp: env.ledger().timestamp(),
};

env.events().publish((Symbol::new(&env, "recovery"), admin.clone()), recovery_event);
```

#### 4. State Protection

- Recovery does not modify subscription state
- Active subscriptions remain unaffected
- Merchant balances remain intact

## Governance Process

### Before Recovery

1. **Documentation**: Create detailed documentation of the stranded fund situation
   - How the funds became stranded
   - Amount and location
   - Evidence supporting the recovery reason

2. **Community Review**: Submit the recovery proposal for community review
   - Public forum post or governance proposal
   - Minimum review period (e.g., 7 days)
   - Allow for objections or alternative solutions

3. **Verification**: Multiple parties verify the claim
   - Technical team confirms funds are truly stranded
   - Community members validate the evidence
   - Legal review if necessary

4. **Authorization**: Where guardians are configured, the recovery proposal
   passes quorum and its timelock before the admin executes it. Otherwise, the
   configured admin authorizes the call under the single-admin compatibility
   path. Record the approval decision.

### During Recovery

1. **Execution**: Admin invokes `recover_stranded_funds`
   - Provide all required parameters
   - Ensure recipient address is correct
   - Execute transaction

2. **Event Monitoring**: Verify the recovery event is emitted
   - Check on-chain events
   - Confirm details match the proposal

### After Recovery

1. **Reporting**: Publish post-recovery report
   - Confirm successful execution
   - Link to transaction hash
   - Update community on outcome

2. **Reconciliation**: Verify recipient received funds
   - Check recipient balance
   - Confirm amount matches

3. **Documentation**: Update records
   - Add to historical recovery log
   - Note lessons learned
   - Update procedures if needed

## Security Considerations

### Threat Model

#### Compromised Admin Key

**Risk**: If the admin key is compromised, an attacker could recover legitimate funds.

**Mitigations**:

- Use multi-signature wallet for admin (requires multiple keys)
- Implement time-locks (recovery requires waiting period)
- Monitor recovery events in real-time
- Have emergency pause mechanism

#### Collusion

**Risk**: Admin and recipient collude to steal funds claiming they're stranded.

**Mitigations**:

- Transparent governance process
- Community oversight and review
- On-chain audit trail
- Social accountability

#### Human Error

**Risk**: Admin accidentally recovers wrong amount or to wrong address.

**Mitigations**:

- Multiple verification steps
- Dry-run simulations before execution
- Clear documentation and checklists
- Peer review of parameters

### Residual Risks

Even with controls, some risks remain:

1. **Admin Trust**: System relies on admin integrity and competence
2. **Governance Quality**: Decisions depend on community engagement and diligence
3. **Technical Mistakes**: Complex scenarios may be misjudged
4. **Time Pressure**: Emergency situations may bypass normal processes

These risks are accepted trade-offs for the ability to recover genuinely stranded funds.

## Monitoring and Auditing

### Real-time Monitoring

Set up alerts for recovery events:

```javascript
// Pseudocode for monitoring
contract.events.on("recovery", (event) => {
  alert({
    admin: event.admin,
    recipient: event.recipient,
    amount: event.amount,
    reason: event.reason,
    timestamp: event.timestamp,
  });
});
```

### Historical Audit

Maintain a public log of all recoveries:

| Date       | Admin     | Token    | Recipient | Amount   | Reason             | Tx Hash  |
| ---------- | --------- | -------- | --------- | -------- | ------------------ | -------- |
| 2026-03-28 | admin.xlm | USDC     | user.xlm  | 100 USDC | UserOverpayment    | 0xabc... |

### Regular Review

- Quarterly review of all recovery operations
- Annual security audit including recovery functionality
- Continuous improvement of governance processes

## Testing

The recovery feature includes comprehensive test coverage:

- ✅ Successful recovery with all reason types
- ✅ Unauthorized caller rejection
- ✅ Zero and negative amount rejection
- ✅ Event emission verification
- ✅ Large and small amount handling
- ✅ Multiple sequential recoveries
- ✅ Different recipient types
- ✅ Interaction with active subscriptions
- ✅ Interaction with cancelled subscriptions
- ✅ Edge cases (max values, idempotency)

Total: 17 dedicated test cases achieving >95% coverage of recovery logic.

## Best Practices

### For Administrators

1. **Always verify** fund status before recovery
2. **Document everything** - assumptions, evidence, decisions
3. **Follow the process** - no shortcuts, even for "obvious" cases
4. **Communicate clearly** - keep community informed
5. **Learn from incidents** - improve procedures after each recovery

### For Community Members

1. **Stay engaged** - review recovery proposals
2. **Ask questions** - challenge assumptions respectfully
3. **Provide evidence** - help verify claims
4. **Report issues** - flag potential misuse early
5. **Participate in governance** - your voice matters

### For Integrators

1. **Monitor events** - watch for unexpected recoveries
2. **Validate assumptions** - don't assume admin is always right
3. **Build safeguards** - add your own monitoring layer
4. **Report anomalies** - help identify suspicious activity
5. **Have contingencies** - plan for admin key compromise

## Examples

### Example 1: Accidental Transfer Recovery

**Scenario**: User accidentally sends 50 USDC to contract address.

**Process**:

1. User reports the mistake via community forum
2. Admin verifies the transaction on-chain
3. Confirms no subscription exists for that address
4. Creates recovery proposal with evidence
5. Community reviews for 7 days
6. Admin executes recovery back to user's correct address

**Code**:

```rust
client.recover_stranded_funds(
    &admin,
    &usdc_token_address,
    &user_correct_address,
    &50_000000, // 50 USDC with 6 decimals
    &String::from_str(&env, "rec_2026_03_28_01"),
    &RecoveryReason::UserOverpayment
);
```

## Snapshot export and restore (forensic recovery)

The contract exposes paginated snapshot endpoints to capture and restore full state pages
for incident response. These are admin-only operations.

- `export_full_snapshot_page(admin, start_id, size) -> FullSnapshotPage` — returns a page
   of `SubscriptionSummary` entries and `MerchantBalanceEntry` records. Use `next_start_id`
   to iterate pages until none remain.

- `restore_snapshot_page(admin, start_id, subscriptions, balances, next_start_id)` — restores
   the provided subscription and balance records into the target contract instance. This
   function requires the emergency stop to be active to prevent interleaving with live traffic.

Usage notes:

- Limit `size` to a reasonable value (<= 100) to avoid long-running calls.
- Always enable emergency stop on the target deployment before running `restore_snapshot_page`.
- Verify exported pages (checksums, counts) off-chain before restore.
- Each export and restore emits `snapshot_exported` and `snapshot_restored` events respectively.


### Example 2: Orphaned Escrow Recovery

**Scenario**: A migration defect leaves an escrow's tokens in the contract but
drops the record that counted the escrow in `total_accounted`. The original
subscriber is unreachable, and support has verified the recipient's new
address. If the escrow remains accounted, do not use this procedure; the
subscriber must use `withdraw_subscriber_funds`.

**Process**:

1. Admin documents the migration defect and verifies the original escrow data.
2. Query `get_token_reconciliation(token)` and verify that the amount is
   unaccounted and included in `recoverable_amount`.
3. Record evidence that the escrow window elapsed, the subscriber is
   unreachable, no dispute is open, and the new recipient address is verified.
4. Create a `RecoverStrandedFunds` proposal with `ExpiredEscrow` rationale;
   obtain quorum and wait for the timelock if guardians are configured.
5. Admin submits the recovery with a unique `recovery_id` and the approved
   recipient, token, and amount.
6. Verify the `recovery` event and recipient balance, then re-run reconciliation
   and publish the transaction and evidence.

**Code**:

```rust
client.recover_stranded_funds(
    &admin,
    &token_address,
    &subscriber_new_address,
    &remaining_balance,
    &String::from_str(&env, "rec_2026_03_28_02"),
    &RecoveryReason::ExpiredEscrow
);
```

## Conclusion

The admin recovery mechanism is a safety net for exceptional circumstances. It's designed with multiple layers of security and transparency, but ultimately relies on:

- **Technical safeguards**: Authorization, validation, audit trails
- **Social safeguards**: Community oversight, governance processes
- **Procedural safeguards**: Documentation, verification, review periods

Used responsibly with strong governance, recovery can save genuinely stranded funds. Misused or poorly governed, it could undermine trust in the system. The contract provides the tools; the community provides the judgment.

## References

- Contract source: `contracts/subscription_vault/src/lib.rs`
- Test suite: `contracts/subscription_vault/src/test.rs`
- Cancellation/escrow behaviour: `contracts/subscription_vault/src/subscription.rs`
  (`apply_cancellation`, `do_withdraw_subscriber_funds`)
- Accounting gate: `contracts/subscription_vault/src/accounting.rs`
  (`get_total_accounted`)
- State machine documentation: `docs/subscription_state_machine.md`
- Cancellation semantics: `docs/cancellation.md`
- Reconciling balances: `docs/reconciliation_strategy.md`
- Admin recovery function: `recover_stranded_funds()`
- Stranded-subscription query: `list_recoverable_subscriptions()`
- Recovery event type: `RecoveryEvent`
- Recovery initiator label: `RecoveryInitiatorRole` enum
- Recovery reasons: `RecoveryReason` enum

## Changelog

- 2026-10-16: Added `list_recoverable_subscriptions` (#438) — surfaces
  subscriptions stranded in `Cancelled` status with a non-zero
  `prepaid_balance`, paginated by cursor. Reporting/forensics only; it grants
  no right to move funds.
- 2026-10-16: Added `initiator_role` to `RecoveryEvent` and
  `SubscriberWithdrawalEvent` (#437) — event schema v4. Lets auditors tell the
  admin recovery path from the subscriber withdrawal path when reading a single
  stream of recovery events. Values are pinned by authorization, not input.
- 2026-02-21: Initial documentation for admin recovery feature
- 2026-09-25: Added **Recovery after subscription cancellation** — normative
  statement that a `Cancelled` subscription's funds are not admin-recoverable,
  the escrow accounting lifecycle, the `ExpiredEscrow` boundary conditions,
  observed error codes, and a support checklist (#245).
