# Merchant Blocklist Timing Issue — Resolution

## Issue Summary

**Area**: Merchant management / Blocklist  
**Problem**: If a merchant is added to the blocklist between a `charge_subscription` call entering and the credit-routing step, there is no check at the credit-routing point. The sub-account could still receive funds.

## Analysis

### Technical Context

In Soroban's execution model, all operations within a single transaction are **atomic**. This means:

1. A merchant cannot be blocklisted **during** the execution of a single `charge_subscription` call.
2. If a merchant is blocklisted after a charge completes but before the next charge is initiated, the subsequent charge will check the blocklist at entry and be rejected.

### Design Decision: Documentation (Option 2)

After analysis, we confirmed that the current behavior is **intentional design**, not a bug. The blocklist implements a **"soft freeze"** strategy:

- **For subscribers**: Blocklist blocks ALL outbound operations (create, deposit, charges)
- **For merchants**: Blocklist blocks only OUTBOUND operations (withdrawal), but allows INBOUND operations (charges continue)

This is explicitly stated in the code and is consistent with the documented principle: **"Preventive, Not Punitive"**.

### Rationale

The soft-freeze design preserves **subscription continuity** while preventing **capital extraction**:

1. **Subscribers aren't harmed**: Ongoing charges continue to accumulate and they retain their prepaid balance
2. **Merchants can't extract funds**: Earnings freeze in the vault until the blocklist is lifted
3. **Admin has control**: Only admins can unblock, ensuring a governance path

This is preferable to a hard block (blocking all charges) because:
- Hard blocking would abruptly halt billing for all subscribers
- Subscribers didn't violate policy; only the merchant did
- The soft freeze achieves compliance without disrupting active subscriptions

## Resolution

### 1. Documentation Update (`docs/blocklist.md`)

#### Title and Overview
- Renamed from "Subscriber Blocklist" to "**Subscriber and Merchant Blocklist**"
- Added a behavior summary table showing blocked/allowed operations per address type

#### Section 5: Charging is Blocked for Blocklisted Subscribers (Not Merchants)
- Explicitly clarified that **subscriber charges are blocked**
- Added note: "If a merchant is blocklisted, charges continue to succeed and credits accumulate in the vault. The merchant simply cannot withdraw those earnings until unblocked."

#### Section 7: Merchant Withdrawal and Crediting Blocked When Merchant is Blocklisted
- Expanded from brief to detailed explanation
- Documented the three behaviors:
  - Merchant cannot withdraw accumulated earnings
  - Funds remain in the vault's accounting ledger
  - **Charging does not block** — subscription charges continue to succeed
- Explained the soft-freeze rationale

#### Section 6 (New): Security Considerations → Merchant Blocklist Timing and Charge Continuity
- Added security-focused explanation of the timing semantics
- Clarified that blocklist prevents **withdrawal** (outbound), not **crediting** (inbound)
- Stated: "This design preserves subscription continuity: blocklisting a merchant does not halt ongoing subscriber charges; it only freezes the merchant's ability to extract those funds."

### 2. Test Implementation (`contracts/subscription_vault/src/test.rs`)

Added `test_merchant_blocklist_charge_continuity_soft_freeze`:

```rust
#[test]
fn test_merchant_blocklist_charge_continuity_soft_freeze() {
    // Verify that blocklisting a merchant does NOT block subsequent charges.
    // The merchant simply cannot withdraw. This tests the soft-freeze semantics
    // documented in docs/blocklist.md section 7.
```

**Test flow**:
1. Create subscription and deposit funds
2. First charge succeeds, merchant balance increases
3. Blocklist the merchant
4. Second charge STILL SUCCEEDS (not blocked for merchant blocklist)
5. Merchant earnings continue to accumulate
6. Merchant withdrawal is BLOCKED with `SubscriberBlocklisted`
7. After unblocking, merchant can withdraw successfully

This test directly validates the documented behavior and serves as a specification for the soft-freeze semantics.

### 3. Testing Coverage Update

Updated `docs/blocklist.md` Testing Coverage section to explicitly reference the new test:
- `test_merchant_blocklist_charge_continuity_soft_freeze`: Explicitly verifies that blocklisting a merchant does not prevent charges — charges succeed and credits accumulate, but the merchant cannot withdraw until unblocked (soft-freeze semantics)

## Key Takeaways

| Aspect | Behavior | Rationale |
|--------|----------|-----------|
| **Merchant blocklist vs charges** | Charges continue; withdrawal blocked | Preserves subscriber continuity; prevents capital extraction |
| **Timing window** | No race condition within transaction | Soroban atomic execution model |
| **Cross-charge semantics** | New charges check blocklist at entry | Rejection happens at charge initiation, not mid-execution |
| **Sub-account credits** | Not retroactively blocked | Reflects soft-freeze: blocklist prevents outbound, not inbound |
| **Admin control** | Admin-only unblock | Ensures governance over blocklist lifecycle |

## Files Modified

- `docs/blocklist.md` — Comprehensive documentation clarifications
- `contracts/subscription_vault/src/test.rs` — Test implementation for soft-freeze semantics

## Verification

The issue is resolved by:
1. ✅ Clarifying that the current behavior is intentional (soft freeze)
2. ✅ Documenting that charges do NOT block for merchant blocklist
3. ✅ Explaining the rationale (preventive, not punitive; preserves continuity)
4. ✅ Adding test coverage to validate the documented behavior
5. ✅ Updating Security Considerations to address timing concerns

No code changes to the charge core logic were needed; only documentation and testing were required to clarify and validate the intentional design.
