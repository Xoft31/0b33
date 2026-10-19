# Canonical Event Schema Reference

This document defines all stable event schemas emitted by the subscription vault contract. Events are indexed and emitted exactly once per successful state transition or fund movement.

## Event Emission Strategy

All events follow this pattern:

```rust
env.events().publish(
    (Symbol::new(env, "event_name"), subscription_id_or_resource_id),
    EventStructWithAllFields {
        ...
    },
);
```

**Key Principles:**

- Topic tuple format: `(Symbol, optional_id)` — enables filtering by event type and resource
- Data struct: always a named type (never raw tuples) — provides stable schema for indexers
- Data struct field order is append-only: canonical positional fields stay in place and new fields are appended
- Every event data struct includes the originating `contract_id: Address`; event payloads append this field after all existing fields
- Versioned event data structs include `schema_version: u32`; schema-v4 charge events append `batch_nonce`, `SubscriptionChargedEvent` appends `prepaid_balance` in schema version 5, schema-v5 recovery events append `initiator_role`, schema-v6 dispute resolution events append `resolved_to`, and schema-v7 dispute events append `token`
- Timestamp: included when useful for ordering/metrics
- No sensitive data: all event fields are customer-visible
- One per action: ensures deterministic batch operation ordering

## Schema Versioning Policy

Event versions are tracked independently for each event type. Version `1` is the historical
event payload layout without an explicit schema version field. Version `2` adds
`schema_version: u32` to every event data struct. Version `3` appends
`contract_id: Address`, identifying the contract deployment that emitted the event.
Only an event whose own payload changes advances:

| Event types | Current version | Schema change |
| --- | ---: | --- |
| All other event types | 3 | Append `contract_id: Address` |
| `SubscriptionChargedEvent` | 5 | Append `batch_nonce: Option<u64>` in version 3, `contract_id: Address` in version 3, then `prepaid_balance: i128` |
| `SplitChargeEvent`, `NonceConsumedEvent`, `DiscountAppliedEvent`, `ChargeFailureEvent`, `SubscriptionChargeFailedEvent`, `SubscriptionCancelledEvent`, `GracePeriodEnteredEvent`, `SubscriptionExpiredEvent`, `LifetimeCapReachedEvent`, `ProtocolFeeChargedEvent`, `FeeConvertedEvent`, `SubscriptionAutoPausedEvent` | 4 | Append `batch_nonce: Option<u64>` in version 3, then `contract_id: Address` |
| `RecoveryEvent`, `SubscriberWithdrawalEvent` | 5 | Append `initiator_role: RecoveryInitiatorRole` in version 4, then `contract_id: Address` |
| `DisputeOpenedEvent`, `DisputeRespondedEvent` | 7 | Append `token: Address` in version 6, then `contract_id: Address` |
| `DisputeResolvedEvent` | 7 | Append `resolved_to: ResolutionTarget` in version 5, `token: Address` in version 6, then `contract_id: Address` |

`EVENT_SCHEMA_VERSION` remains as a deprecated compatibility constant for the maximum version
currently used. Consumers must inspect the individual event's `schema_version`, not compare it to
that global maximum. New versioned event types start at version `3`, and subsequent schema changes increment
only that event type's version. Fields remain append-only to preserve existing positional prefixes
for indexers.

Indexers should treat the fields listed for each event below as the canonical positional prefix.
The `contract_id` field is present on every event data struct. Versioned events also include
`schema_version`. These fields are intentionally not repeated in each individual field list.
`contract_id` is the emitting contract's address; indexers consuming events from multiple
deployments should use it to distinguish their origins.
Future schema changes must be additive-only: append new fields after
all existing fields, including schema metadata. Removing a field,
renaming a field, reordering a field, or changing a field type is a breaking schema change that
requires a new migration plan rather than a silent event update.

## Lifecycle Events

### SubscriptionCreatedEvent

**Topic:** `("created", subscription_id)`

Emitted when a subscription is created via `create_subscription()` or `create_subscription_from_plan()`.

**Fields:**

- `subscription_id`: u32 — unique subscription identifier
- `subscriber`: Address — subscriber account
- `merchant`: Address — merchant account receiving payments
- `amount`: i128 — recurring charge amount per billing interval
- `interval_seconds`: u64 — billing interval in seconds
- `lifetime_cap`: Option<i128> — optional maximum total charge limit
- `expires_at`: Option<u64> — optional expiration timestamp

**Example Use Cases:**

- Build subscription graph in explorer
- Display user's active subscriptions
- Calculate subscription creation metrics

---

### FundsDepositedEvent

**Topic:** `("deposited", subscription_id)`

Emitted when a subscriber deposits funds into a subscription vault via `deposit_funds()`.

**Fields:**

- `subscription_id`: u32
- `subscriber`: Address
- `amount`: i128 — amount deposited (in token base units)
- `prepaid_balance`: i128 — total prepaid balance after deposit

**Example Use Cases:**

- Display deposit history in subscriber UI
- Monitor prepaid balance trends
- Trigger low-balance alerts

---

## Charging Events

Charge-related events emitted during `batch_charge()` carry
`batch_nonce: Some(nonce)`, matching the nonce consumed by that batch. This
includes `NonceConsumedEvent`, successful and failed charge events, discount,
fee, split, expiration, cancellation, grace-period, auto-pause, and lifetime-cap
events. The same event types emitted outside a batch carry `batch_nonce: None`.

### SubscriptionChargedEvent

**Topic:** `("charged",)` _(no subscription ID in topic)_

Emitted when an interval-based charge succeeds via `charge_subscription()` or `batch_charge()`.

**Fields:**

- `subscription_id`: u32
- `merchant`: Address — merchant receiving the payment
- `amount`: i128 — total charged amount
- `lifetime_charged`: i128 — cumulative total charged over subscription lifetime
- `prepaid_balance`: i128 — remaining prepaid balance after this charge; appended after `contract_id` in schema version 5
- `batch_nonce`: Option<u64> — present when emitted by a batch charge

**Example Use Cases:**

- Generate merchant revenue reports
- Track subscription payment history
- Aggregate merchant earnings by token

---

### SubscriptionChargeFailedEvent

**Topic:** `("charge_failed", subscription_id)`

Emitted when a charge attempt fails due to insufficient balance. Subscription transitions to `InsufficientBalance` or `GracePeriod` status.

**Fields:**

- `subscription_id`: u32
- `merchant`: Address
- `required_amount`: i128 — amount that was attempted to be charged
- `available_balance`: i128 — prepaid balance at time of charge
- `shortfall`: i128 — required_amount - available_balance
- `resulting_status`: SubscriptionStatus — status after failure (GracePeriod or InsufficientBalance)
- `timestamp`: u64
- `batch_nonce`: Option<u64> — present when emitted by a batch charge

**Example Use Cases:**

- Send notifications when subscription enters non-active states
- Track recovery candidates for re-engagement
- Monitor subscription health metrics

---

### OneOffChargedEvent

**Topic:** `("one_off_charged", subscription_id)`

Emitted when a merchant initiates an off-interval charge via `charge_one_off()`.

**Fields:**

- `subscription_id`: u32
- `merchant`: Address
- `amount`: i128

---

### ProtocolFeeChargedEvent

**Topic:** `("protocol_fee_charged", subscription_id)`

Emitted when protocol fees are extracted and routed to treasury during `charge_subscription()` batch operations.

**Fields:**

- `subscription_id`: u32
- `treasury`: Address — fee recipient
- `fee_amount`: i128 — amount routed to treasury
- `merchant_amount`: i128 — amount retained by merchant after fee
- `timestamp`: u64
- `batch_nonce`: Option<u64> — present when emitted by a batch charge

---

### LifetimeCapReachedEvent

**Topic:** `("lifetime_cap_reached", subscription_id)`

Emitted when a subscription's lifetime charge cap is exhausted. Subscription is automatically cancelled.

**Fields:**

- `subscription_id`: u32
- `lifetime_cap`: i128 — configured maximum total charges
- `lifetime_charged`: i128 — total charged at time of cap hit
- `timestamp`: u64
- `batch_nonce`: Option<u64> — present when emitted by a batch charge

**Example Use Cases:**

- Trigger subscription renewal flows
- Track cap-reached metrics
- Archive completed subscriptions

---

### UsageStatementEvent

**Topic:** `("usage_statement", subscription_id)`

Emitted when usage-based charges are processed via `charge_usage()`.

**Fields:**

- `subscription_id`: u32
- `merchant`: Address
- `usage_amount`: i128 — metered charge amount
- `token`: Address — settlement token
- `timestamp`: u64
- `reference`: String — idempotency reference key

---

### UsageCapResetEvent

**Topic:** `("usage_cap_reset", subscription_id)`

Emitted when a usage-enabled subscription with a configured per-period usage cap enters a later billing period and its accumulated usage is reset.

**Fields:**

- `subscription_id`: u32
- `merchant`: Address
- `token`: Address — settlement token
- `previous_period_index`: u64
- `current_period_index`: u64
- `previous_period_usage_units`: i128 — usage accumulated before the reset
- `usage_cap_units`: i128 — configured per-period cap
- `timestamp`: u64
- `schema_version`: u32

---

## Subscription Status Events

### SubscriptionPausedEvent

**Topic:** `("sub_paused", subscription_id)`

Emitted when a subscription is paused via `pause_subscription()`. No charges processed while paused.

**Fields:**

- `subscription_id`: u32
- `authorizer`: Address — subscriber or merchant who initiated pause

**Example Use Cases:**

- Display subscription pause status
- Analytics on pause/resume cycles
- Update merchant UI

---

### SubscriptionResumedEvent

**Topic:** `("sub_resumed", subscription_id)`

Emitted when a subscription is resumed from Paused/GracePeriod/InsufficientBalance via `resume_subscription()` OR automatically after a successful deposit rebalances an underfunded subscription.

**Fields:**

- `subscription_id`: u32
- `authorizer`: Address — subscriber or merchant who initiated resume

**Example Use Cases:**

- Update subscription status in UI
- Track recovery metrics
- Trigger billing resumption workflows

---

### SubscriptionCancelledEvent

**Topic:** `("subscription_cancelled", subscription_id)`

Emitted when a subscription is cancelled via `cancel_subscription()`. Terminal state.

**Fields:**

- `subscription_id`: u32
- `authorizer`: Address — subscriber or merchant who cancelled
- `refund_amount`: i128 — prepaid balance available for refund

**Example Use Cases:**

- Process refunds to subscribers
- Calculate churn metrics
- Archive cancelled subscriptions

---

### SubscriptionExpiredEvent

**Topic:** `("subscription_expired", subscription_id)`

Emitted when a subscription is automatically expired due to `expires_at` timestamp being reached.

**Fields:**

- `subscription_id`: u32
- `timestamp`: u64

---

### SubscriptionRecoveryReadyEvent

**Topic:** `("recovery_ready", subscription_id)`

Emitted after a deposit when a previously underfunded subscription (`InsufficientBalance` or `GracePeriod`) is restored to sufficient balance and automatically transitions to `Active`.

**Fields:**

- `subscription_id`: u32
- `subscriber`: Address
- `prepaid_balance`: i128
- `required_amount`: i128
- `timestamp`: u64

---

## Withdrawal Events

### MerchantWithdrawalEvent

**Topic:** `("withdrawn", merchant, token)`

Emitted when a merchant withdraws earned funds via `withdraw_merchant_funds()` or `withdraw_merchant_token_funds()`.

**Fields:**

- `merchant`: Address
- `token`: Address — token being withdrawn
- `amount`: i128 — amount withdrawn
- `remaining_balance`: i128 — balance after withdrawal

**Example Use Cases:**

- Track merchant payout history
- Display earnings ledger
- Reconcile merchant balances

---

### PartialRefundEvent

**Topic:** `("partial_refund", subscription_id)`

Emitted when a cancelled subscription has its refund amount transferred back to subscriber.

**Fields:**

- `subscription_id`: u32
- `subscriber`: Address
- `amount`: i128
- `timestamp`: u64

---

### MerchantRefundEvent

**Topic:** `("merchant_refund", merchant)`

Emitted when a merchant refunds a subscriber's balance via `merchant_refund()`.

**Fields:**

- `merchant`: Address
- `subscriber`: Address
- `token`: Address
- `amount`: i128

---

### SubscriberWithdrawalEvent

**Topic:** `("subscriber_withdrawal", subscription_id)`

Emitted when a subscriber withdraws funds from a cancelled subscription.

**Fields:**

- `subscription_id`: u32
- `subscriber`: Address
- `amount`: i128
- `initiator_role`: RecoveryInitiatorRole — always `Subscriber` (schema-v4)

**Indexer note:** `initiator_role` is the discriminator that separates the two
recovery paths when they are consumed as one stream. See
[`RecoveryEvent`](#recoveryevent) below — `recovery` always reports `Admin`,
`sub_withdrawn` always reports `Subscriber`. The values are pinned by
authorization, not by caller input, so an indexer can rely on them.

---

## Admin & Config Events

### EmergencyStopEvent

**Topic:** `("emergency_stop_enabled",)` or `("emergency_stop_disabled",)`

Emitted when the admin toggles emergency stop via `enable_emergency_stop()` or `disable_emergency_stop()`.

**Fields:**

- `admin`: Address
- `state`: bool — `true` when emergency stop is enabled, `false` when it is disabled
- `timestamp`: u64
- `schema_version`: u32

This shared schema allows a single decoder to handle both emergency-stop events.

---

### AdminRotatedEvent

**Topic:** `("admin_rotated",)`

Emitted when admin is rotated to a new address via `rotate_admin()`.

**Fields:**

- `old_admin`: Address
- `new_admin`: Address
- `timestamp`: u64

---

### RecoveryEvent

**Topic:** `("recovery",)`

Emitted when admin recovers stranded funds via `recover_stranded_funds()`.

**Fields:**

- `admin`: Address
- `recipient`: Address
- `token`: Address
- `amount`: i128
- `reason`: RecoveryReason enum
- `timestamp`: u64
- `initiator_role`: RecoveryInitiatorRole — always `Admin` (schema-v4)

**Recovery-path discrimination (schema-v4, #437):** the vault has two
structurally separate recovery paths, and `initiator_role` labels which one
released the funds:

| `initiator_role` | Emitted by | Event | Gate |
|---|---|---|---|
| `Admin` | `recover_stranded_funds` | `("recovery", admin)` | `require_admin_auth` + multi-sig/governance approval |
| `Subscriber` | `withdraw_subscriber_funds`, `finalize_emergency_withdraw` | `("sub_withdrawn", subscription_id)` | `subscriber.require_auth()` + subscriber must own the record |

An auditor consuming a unified "funds were released" stream can branch on this
one field instead of cross-referencing `DataKey::Admin` on-chain. Because the
paths are gated by different signers, the value cannot be spoofed: it is
emitted by the contract and never accepted as a parameter.

---

### BillingCompactedEvent

**Topic:** `("billing_compacted",)`

Emitted when billing statement compaction is run via `compact_statements()`.

**Fields:**

- `admin`: Address
- `subscription_id`: u32
- `pruned_count`: u32
- `kept_count`: u32
- `total_pruned_amount`: i128
- `timestamp`: u64
- `aggregate_pruned_count`: u32 — total pruned across all historical compactions
- `aggregate_total_amount`: i128 — total pruned amount
- `aggregate_oldest_period_start`: Option<u64>
- `aggregate_newest_period_end`: Option<u64>

---

### ⚠️ Merchant Multi-Sig Configuration — No Event Currently Emitted

`set_merchant_multisig()` (`merchant.rs`) lets an admin configure a per-merchant
`MerchantMultiSigConfig { signers: Vec<Address>, threshold: u32 }`, which then
requires `threshold` of the listed `signers` to co-authorize
`withdraw_merchant_funds()`/`withdraw_merchant_funds_for_token()`.

**Gap:** this function currently does **not** call `env.events().publish(...)` —
changing a merchant's multisig configuration is invisible to indexers today. This is
flagged here rather than silently documented as a working event, since the whole
point of this wave-5 update is aligning indexers with contract reality before
mainnet. **Recommend a follow-up PR** adding a `MerchantMultiSigConfiguredEvent`
(topic `("multisig_configured", merchant)`, fields: `merchant: Address`,
`signers: Vec<Address>`, `threshold: u32`, `timestamp: u64`,
`schema_version: u32`) before mainnet launch, since indexers otherwise cannot
alert on or audit multisig policy changes for a merchant's withdrawal authority.


## Merchant Configuration Events

### MerchantPausedEvent

**Topic:** `("merchant_paused", merchant)`

Emitted when a merchant enables blanket pause via `pause_merchant()`.

**Fields:**

- `merchant`: Address
- `timestamp`: u64

**Effect:** All subscriptions for this merchant become non-chargeable.

---

### MerchantUnpausedEvent

**Topic:** `("merchant_unpaused", merchant)`

Emitted when a merchant disables blanket pause via `unpause_merchant()`.

**Fields:**

- `merchant`: Address
- `timestamp`: u64

---

### PlanTemplateUpdatedEvent

**Topic:** `("plan_updated",)`

Emitted when a plan template is updated to a new version.

**Fields:**

- `template_key`: u32 — logical template group shared by all versions
- `old_plan_id`: u32
- `new_plan_id`: u32
- `version`: u32
- `merchant`: Address
- `timestamp`: u64

---

### PlanMaxActiveUpdatedEvent

**Topic:** `("plan_max_active_updated",)`

Emitted when a plan's maximum active subscriptions limit is configured.

**Fields:**

- `plan_template_id`: u32
- `merchant`: Address
- `max_active`: u32 — (`0` = no limit)
- `timestamp`: u64

---

### SubscriptionMigratedEvent

**Topic:** `("subscription_migrated", subscription_id)`

Emitted when a subscription is migrated from one plan template version to another.

**Fields:**

- `subscription_id`: u32
- `template_key`: u32
- `from_plan_id`: u32
- `to_plan_id`: u32
- `merchant`: Address
- `subscriber`: Address
- `timestamp`: u64

---

## Metadata Events

### MetadataSetEvent

**Topic:** `("metadata_set", subscription_id)`

Emitted when subscription metadata is set or updated.

**Fields:**

- `subscription_id`: u32
- `key`: String
- `authorizer`: Address

---

### MetadataDeletedEvent

**Topic:** `("metadata_deleted", subscription_id)`

Emitted when subscription metadata is deleted.

**Fields:**

- `subscription_id`: u32
- `key`: String
- `authorizer`: Address

---

## Dispute & Escrow Events

Chargeback-style workflow (`dispute.rs`): a subscriber opens a dispute, the disputed
amount moves from the merchant's balance into an escrow bucket
(`DataKey::DisputeEscrow(dispute_id)`), an admin may respond with evidence, and an
admin resolves the dispute by routing the escrowed funds to either party.

**Escrow invariant** (see `dispute.rs` module docs): `sum(escrow balances) + merchant_balance == original_merchant_balance` at all times between a dispute opening and its resolution. Indexers reconciling merchant balances should treat an open dispute's escrowed amount as temporarily removed from that merchant's spendable balance, not lost.

### DisputeOpenedEvent

**Topic:** `("dispute_opened", dispute_id)`

Emitted when a subscriber opens a dispute via `open_dispute()`. Debits the disputed amount from the merchant's balance into escrow.

**Fields:**

- `dispute_id`: u64 — unique dispute identifier
- `subscription_id`: u32
- `subscriber`: Address — the disputing party (must match the subscription's subscriber)
- `merchant`: Address
- `amount`: i128 — amount moved into escrow
- `evidence_hash`: Option<BytesN<32>> — optional subscriber-supplied evidence hash
- `timestamp`: u64
- `schema_version`: u32
- `token`: Address — token denomination of the escrowed amount

**Example Use Cases:**

- Alert merchant of a new dispute
- Reconcile merchant available balance vs. escrowed balance
- Track dispute volume/rate per merchant

---

### DisputeRespondedEvent

**Topic:** `("dispute_responded", dispute_id)`

Emitted when an admin responds to a dispute via `respond_dispute()`, transitioning it from `Open` to `Responded`. Does not move funds.

**Fields:**

- `dispute_id`: u64
- `subscription_id`: u32
- `admin_evidence_hash`: Option<BytesN<32>> — optional admin-supplied evidence hash
- `timestamp`: u64
- `schema_version`: u32
- `token`: Address — token denomination of the disputed amount

---

### DisputeResolvedEvent

**Topic:** `("dispute_resolved", dispute_id)`

Emitted when an admin resolves a dispute via `resolve_dispute()`. Clears the escrow and routes the funds to the party identified by `resolved_to`.

**Fields:**

- `dispute_id`: u64
- `subscription_id`: u32
- `resolution`: DisputeStatus — the final dispute status
- `timestamp`: u64
- `schema_version`: u32
- `resolved_to`: ResolutionTarget — `Subscriber` or `Merchant`, identifying who received the escrowed funds
- `token`: Address — token denomination of the disputed amount

**Security note:** `open_dispute` requires the caller to be the subscription's subscriber; `respond_dispute`/`resolve_dispute` are admin-only. Double-open is prevented via `DataKey::SubscriptionDispute(subscription_id)` — indexers should not expect more than one open dispute per subscription at a time.

### DisputeWindowUpdatedEvent

**Topic:** `("dispute_window_updated",)`

Emitted when an admin or approved governance proposal changes the global dispute
response window.

**Fields:**

- `old_window_secs`: u64
- `new_window_secs`: u64
- `timestamp`: u64
- `schema_version`: u32

---

## Governance & Timelock Events

Quorum-gated proposal system (`governance.rs`): guardians vote on proposals with a
weighted-quorum threshold; every proposal carries an `eta` (execution timestamp) that
must have passed before it can be executed — this is the timelock. Currently supports
rotating the contract admin and updating the protocol fee/treasury address
(`ProposalKind::SetProtocolFee`, which is what actually changes the treasury address
behind the timelock).

### ProposalSubmittedEvent

**Topic:** `("proposal_submitted",)`

Emitted when a proposal is submitted via `submit_proposal()`.

**Fields:**

- `proposal_id`: u64
- `kind`: ProposalKind — `RotateAdmin` (0), `SetProtocolFee` (1), or `UpgradeContract` (2, reserved/unused — execution currently always fails for this kind)
- `target`: Address — primary target (new admin for `RotateAdmin`; unused placeholder for `SetProtocolFee`, see `ProposalExecutedEvent`/on-chain `Proposal.target2`/`target3` for the actual fee/treasury values, which are not repeated in this event)
- `quorum_bps`: u32 — required quorum in basis points (max 10,000)
- `eta`: u64 — earliest execution timestamp (the timelock)
- `timestamp`: u64 — submission timestamp

**Indexer note:** this event does not include `target2`/`target3` (the proposed treasury address / fee bps for a `SetProtocolFee` proposal) — an indexer needing those values must read them via `get_proposal(proposal_id)` rather than from this event alone.

---

### ProposalVotedEvent

**Topic:** `("proposal_voted",)`

Emitted on each guardian vote via `vote_proposal()`. Double-voting isn't blocked — a guardian may change their vote before execution; each call re-emits this event with their latest choice.

**Fields:**

- `proposal_id`: u64
- `guardian`: Address
- `voted_yes`: bool
- `guardian_weight`: u32 — the guardian's voting weight **at the time of this vote** (may differ from their current weight if later changed/removed)
- `timestamp`: u64

---

### ProposalExecutedEvent

**Topic:** `("proposal_executed",)`

Emitted when a proposal successfully executes via `execute_proposal()`, after both quorum and the ETA timelock have been satisfied.

**Fields:**

- `proposal_id`: u64
- `kind`: ProposalKind
- `votes_for`: u32 — total weight voting yes, **re-validated against current guardians** at execution time (a since-removed guardian's earlier vote is excluded)
- `votes_against`: u32
- `total_weight`: u32 — total voting weight across all current guardians at execution time
- `timestamp`: u64

**Security note:** guardian removal mid-vote invalidates that guardian's prior vote for quorum-counting purposes — `votes_for`/`votes_against` reflect only currently-valid guardians, which may differ from the raw vote tally recorded on `ProposalVotedEvent`.

---

### ProposalCancelledEvent

**Topic:** `("proposal_cancelled",)`

Emitted when the admin cancels a pending, not-yet-executed proposal via `cancel_proposal()`.

**Fields:**

- `proposal_id`: u64
- `reason`: String — free-form cancellation reason
- `timestamp`: u64

---

## Initialization & Migration Events

### MigrationExportEvent

**Topic:** `("export_subscriptions",)`

Emitted when subscriptions are exported via `export_subscription_summaries()`.

**Fields:**

- `admin`: Address
- `start_id`: u32
- `limit`: u32
- `exported`: u32 — actual count exported
- `timestamp`: u64

---

### SchemaMigratedEvent

**Topic:** `("schema_migrated",)`

Emitted when the contract schema is upgraded via `migrate(admin, dry_run = false)`.

**Fields:**

- `from`: u32 — previous stored schema version
- `to`: u32 — new schema version
- `admin`: Address
- `timestamp`: u64

---

## Security Considerations

### No Sensitive Data in Events

Events are **public blockchain data**. The following are **intentionally NOT included**:

- ✗ Private keys or account secrets
- ✗ Optional metadata values (only keys are emitted)
- ✗ Subscriber email or PII (only Address)
- ✗ Merchant fee percentages or routing details (only transactions recorded)
- ✗ Grace period configuration (only status transition events)

### Failure Events Are Explicit

Subscriptions that fail charging emit `SubscriptionChargeFailedEvent` — never emit a "success" event. This ensures indexers can distinguish:

- ✓ Successful charge → `SubscriptionChargedEvent`
- ✗ Failed charge → `SubscriptionChargeFailedEvent`
- ✗ Charge not attempted (wrong status) → no event

### Deterministic Batch Ordering

`batch_charge()` emits exactly one event per successfully charged subscription, in the order subscriptions were processed. Failed charges do not emit events. This ensures indexers can reconstruct execution order without re-querying the contract.

### Authorization Events Include Authorizer

When an action requires explicit authorization (`pause`, `resume`, `cancel`, `refund`), the `authorizer` field in the event identifies who approved it:

- Subscriber initiated
- Merchant initiated
- Admin initiated (for recovery)

This enables audit trails for compliance.

---

## Indexing Recommendations

| Event                         | Index By                               | Aggregate By                   |
| ----------------------------- | -------------------------------------- | ------------------------------ |
| SubscriptionCreatedEvent      | subscription_id, subscriber, merchant  | time (metrics)                 |
| FundsDepositedEvent           | subscription_id, subscriber            | balance progression            |
| SubscriptionChargedEvent      | subscription_id, merchant, token       | merchant earnings, pvt revenue |
| SubscriptionChargeFailedEvent | subscription_id, subscriber            | recovery metrics               |
| MerchantWithdrawalEvent       | merchant, token, timestamp             | payout history                 |
| LifetimeCapReachedEvent       | subscription_id, merchant, timestamp   | completion metrics             |
| SubscriptionCancelledEvent    | subscription_id, subscriber, timestamp | churn metrics                  |
| MerchantPausedEvent           | merchant, timestamp                    | merchant downtime              |
| DisputeOpenedEvent            | dispute_id, subscription_id, merchant  | dispute volume, escrow reconciliation |
| DisputeResolvedEvent          | dispute_id, subscription_id            | resolution outcome metrics     |
| ProposalSubmittedEvent        | proposal_id, kind                      | governance activity tracking   |
| ProposalExecutedEvent         | proposal_id, kind                      | timelock/quorum audit trail    |

---

## Timestamp Precision

All timestamps are **ledger timestamps in seconds** (Unix epoch). Resolution is not nanosecond-accurate; use for relative ordering and metrics, not high-frequency reconciliation.
