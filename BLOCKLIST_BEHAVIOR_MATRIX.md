# Blocklist Behavior Matrix — Quick Reference

## Overview

This matrix documents the behavior of blocklisted addresses (subscribers and merchants) across all contract operations.

---

## Subscriber Blocklisting

| Operation | Blocklisted Subscriber | Non-blocklisted Subscriber | Error (if blocked) |
|-----------|------------------------|--------------------------|-------------------|
| **Mutation: Create subscription** | ❌ Blocked | ✅ Allowed | `SubscriberBlocklisted` |
| **Mutation: Deposit funds** | ❌ Blocked | ✅ Allowed | `SubscriberBlocklisted` |
| **Mutation: Pause subscription** | ❌ Blocked | ✅ Allowed | `SubscriberBlocklisted` |
| **Mutation: Resume subscription** | ❌ Blocked | ✅ Allowed | `SubscriberBlocklisted` |
| **Mutation: Cancel subscription** | ❌ Blocked | ✅ Allowed | `SubscriberBlocklisted` |
| **Mutation: Migrate to plan** | ❌ Blocked | ✅ Allowed | `SubscriberBlocklisted` |
| **Charge: Interval charge** | ❌ Blocked | ✅ Allowed | `SubscriberBlocklisted` |
| **Charge: Usage charge** | ❌ Blocked | ✅ Allowed | `SubscriberBlocklisted` |
| **Withdrawal: Withdraw prepaid funds** | ✅ Allowed* | ✅ Allowed | — |

**\* Fund return path only (after subscription is cancelled)**

---

## Merchant Blocklisting

| Operation | Blocklisted Merchant | Non-blocklisted Merchant | Error (if blocked) |
|-----------|---------------------|-------------------------|-------------------|
| **Receive: Charge credits** | ✅ Allowed (accumulates) | ✅ Allowed | — |
| **Withdrawal: Withdraw merchant funds** | ❌ Blocked | ✅ Allowed | `SubscriberBlocklisted` |
| **Withdrawal: Withdraw by token** | ❌ Blocked | ✅ Allowed | `SubscriberBlocklisted` |

---

## Cross-Address Interactions

### When Subscriber is Blocklisted

| Actor | Operation | Result |
|-------|-----------|--------|
| **Merchant** | Pause blocklisted subscriber's subscription | ✅ Allowed |
| **Merchant** | Cancel blocklisted subscriber's subscription | ✅ Allowed |
| **Merchant** | Resume blocklisted subscriber's subscription | ✅ Allowed |
| **Admin** | Pause blocklisted subscriber's subscription | ✅ Allowed |
| **Admin** | Cancel blocklisted subscriber's subscription | ✅ Allowed |
| **Admin** | Resume blocklisted subscriber's subscription | ✅ Allowed |

### When Merchant is Blocklisted

| Actor | Operation | Result |
|-------|-----------|--------|
| **Subscriber** | Charge initiated for subscription with blocklisted merchant | ❌ Blocked at next charge |
| **Admin** | Charge initiated for subscription with blocklisted merchant | ❌ Blocked at next charge |
| **Split payee** (if blocklisted) | Charge attempted | ❌ Blocked |

---

## Timing Semantics

### Subscriber Blocklisting

- **Entry check**: Blocklist is checked at the start of operations like `create_subscription`, `deposit_funds`, `charge_subscription`
- **Timing**: If a subscriber is blocklisted **after** a charge completes but **before** the next charge, the next charge will be rejected
- **No retroactive**: Charges that completed before blocklisting are NOT reversed

### Merchant Blocklisting

- **Entry check**: Blocklist is checked at charge entry for the merchant and any split payees
- **Charge continuity**: Merchants blocklisted **after** a charge are prevented from **subsequent** charges (not the completed one)
- **Withdrawal freeze**: Merchant withdrawal is blocked immediately when blocklisted, regardless of when earnings were accumulated
- **Soft freeze**: Credits continue to accumulate (charges succeed) but extraction is blocked

---

## Blocklist Lifecycle

### Adding to Blocklist

```
subscriber / merchant address
    ↓
(admin or merchant) calls add_to_blocklist
    ↓
Blocklist entry created with reason
    ↓
BlocklistAddedEvent emitted
    ↓
Address blocked for sensitive operations
```

### Removing from Blocklist

```
blocklisted subscriber / merchant
    ↓
(admin only) calls remove_from_blocklist
    ↓
Check: no open disputes (subscribers only)
    ↓
Blocklist entry removed
    ↓
BlocklistRemovedEvent emitted
    ↓
Address restored to full access
```

---

## Error Responses

All blocklist violations return the same error code for security consistency:

- **Error Code**: `1003` (`SubscriberBlocklisted`)
- **Category**: Auth
- **Retry Strategy**: Do not retry; escalate to support/admin flow

---

## Testing Checklist

- [ ] Subscriber can't create subscription when blocklisted
- [ ] Subscriber can't deposit funds when blocklisted
- [ ] Subscriber can't pause/resume/cancel when blocklisted
- [ ] Subscriber can't be charged when blocklisted
- [ ] Merchant/admin can still manage blocklisted subscriber's subscriptions
- [ ] Subscriber can withdraw prepaid balance when blocklisted (fund return)
- [ ] Merchant can be blocklisted by admin
- [ ] Merchant blocklisted still receives charges (soft freeze)
- [ ] Merchant can't withdraw when blocklisted
- [ ] Merchant can withdraw after unblocking
- [ ] Duplicate add-to-blocklist is rejected
- [ ] Can't remove from blocklist if open disputes exist
- [ ] Add/remove events are emitted with correct metadata

---

## Operational Notes

### For Admins

1. Blocklisting a merchant does NOT halt subscriber charges
2. Blocklisting a subscriber DOES halt all subscriber-driven mutations and charges
3. Use reasons to document why each blocklist entry was added
4. Review entries periodically for removal eligibility
5. Unblocking is the only way to restore access; no automatic expiry

### For Merchants

1. You can blocklist subscribers who have disputes or payment issues
2. You cannot unblock subscribers; only admin can
3. Admin can still manage your subscriptions even if you're blocklisted
4. Your earnings accumulate while blocklisted but can't be extracted
5. Escalate serious issues to admin for platform-wide handling

### For Subscribers

1. If blocklisted, you can't create new subscriptions
2. Existing subscriptions won't be charged while blocklisted
3. You can still withdraw your prepaid balance (fund return)
4. Appeal blocklisting through the admin (only admins can remove)
5. Your balance and subscription status are preserved during blocklist
