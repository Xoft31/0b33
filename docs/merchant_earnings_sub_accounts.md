# Merchant Earnings with Sub-Accounts

> **Status**: The sub-account earnings queries are backward compatible. No breaking changes to existing APIs. This document describes new query endpoints that expose the complete earnings picture including sub-account balances.

## Problem

Previously, when a merchant used sub-accounts (isolated per-department ledgers), off-chain reporters faced a confusing situation:

1. **`get_merchant_balance_by_token(merchant, token)`** — Returned only the **primary** merchant balance
   - When charges routed to sub-accounts, this balance would be *reduced* to maintain invariants
   - Did not show the funds in sub-accounts

2. **`get_merchant_total_earnings(merchant)`** — Returned only the parent's `TokenEarnings` ledger
   - This recorded the **full accrual** (primary + sub-accounts) for roll-up reporting
   - But did not show the split between primary and sub-account storage

This split between "earnings recording" and "balance storage" made it impossible for off-chain reporters to understand:
- How much of the reported earnings was actually in the primary account (withdrawable immediately)
- How much was isolated in sub-accounts (requires per-sub-account withdrawal)
- What the total merchant control truly was

## Solution: Three New Query Endpoints

### 1. `get_merchant_token_earnings_with_sub_accounts(merchant, token)`

**Returns:** `MerchantTokenEarningsWithSubAccounts` with:
- `primary_earnings: TokenEarnings` — Full accrual history (interval, usage, one-off, withdrawals, refunds)
- `primary_balance: i128` — Funds in the primary account
- `sub_account_balances: Vec<SubAccountBalance>` — Funds in each sub-account (label + balance)
- `total_balance: i128` — Sum of primary + all sub-accounts

**Use case:** When you need the complete breakdown for one token-merchant pair. This is the recommended query for off-chain reporters and dashboards.

**Example:**

```
earnings_with_subs = get_merchant_token_earnings_with_sub_accounts(merchant, usdc_token)

// Off-chain reporter can now see:
primary_available = earnings_with_subs.primary_balance
sub_account_balances = earnings_with_subs.sub_account_balances  // [("sales", 1000), ("support", 500)]
total_available = earnings_with_subs.total_balance  // 1500

// And verify earnings history:
total_accrued = earnings_with_subs.primary_earnings.accruals.interval 
              + earnings_with_subs.primary_earnings.accruals.usage
              + earnings_with_subs.primary_earnings.accruals.one_off
// total_accrued should equal total_available if no withdrawals/refunds
```

### 2. `get_merchant_total_earnings_with_sub_accounts(merchant)`

**Returns:** `Vec<MerchantTokenEarningsWithSubAccounts>` — Same breakdown as above, but for **all tokens** the merchant has earned from.

**Use case:** Full merchant earnings dashboard across all tokens/currencies.

**Example:**

```
all_earnings = get_merchant_total_earnings_with_sub_accounts(merchant)

// Iterate over all tokens
for token_breakdown in all_earnings {
    println!("Token: {}", token_breakdown.token);
    println!("  Primary: {}", token_breakdown.primary_balance);
    println!("  Sub-accounts:");
    for sub in token_breakdown.sub_account_balances {
        println!("    {}: {}", sub.label, sub.balance);
    }
    println!("  Total: {}", token_breakdown.total_balance);
}
```

### 3. `get_merchant_balance_aggregated(merchant, token)`

**Returns:** `i128` — The total balance (primary + all sub-accounts) for one token.

**Use case:** Quick aggregation query when you only need the total, not the breakdown.

**Example:**

```
total = get_merchant_balance_aggregated(merchant, usdc_token)  // 1500
// Equivalent to: get_merchant_token_earnings_with_sub_accounts(...).total_balance
```

## Backward Compatibility

The existing queries remain unchanged:

- `get_merchant_balance_by_token(merchant, token)` — Still returns only the **primary** balance
- `get_merchant_total_earnings(merchant)` — Still returns `Vec<(Address, TokenEarnings)>` (no sub-account info)

**Migration path for off-chain reporters:**

If you were using `get_merchant_total_earnings` and need to account for sub-accounts:

**Before:**
```rust
let earnings = get_merchant_total_earnings(merchant);
for (token, token_earnings) in earnings {
    let computed = token_earnings.accruals.interval 
                 + token_earnings.accruals.usage
                 + token_earnings.accruals.one_off
                 - token_earnings.withdrawals
                 - token_earnings.refunds;
    // computed != actual merchant balance if sub-accounts exist!
}
```

**After:**
```rust
let earnings_with_subs = get_merchant_total_earnings_with_sub_accounts(merchant);
for breakdown in earnings_with_subs {
    let computed = breakdown.primary_earnings.accruals.interval 
                 + breakdown.primary_earnings.accruals.usage
                 + breakdown.primary_earnings.accruals.one_off
                 - breakdown.primary_earnings.withdrawals
                 - breakdown.primary_earnings.refunds;
    // computed == breakdown.total_balance (always!)
    // You now have the split:
    //   - breakdown.primary_balance (primary account)
    //   - breakdown.sub_account_balances (per-department)
}
```

## Data Structure Reference

### `SubAccountBalance`
```
pub struct SubAccountBalance {
    pub label: Symbol,      // e.g., "sales", "support"
    pub balance: i128,      // Amount in this sub-account
}
```

### `MerchantTokenEarningsWithSubAccounts`
```
pub struct MerchantTokenEarningsWithSubAccounts {
    pub token: Address,                                      // Token address
    pub primary_earnings: TokenEarnings,                    // Full accrual history
    pub primary_balance: i128,                              // Primary account balance
    pub sub_account_balances: Vec<SubAccountBalance>,       // All sub-accounts
    pub total_balance: i128,                                // primary_balance + sum(sub_account_balances)
}
```

where `TokenEarnings` is:
```
pub struct TokenEarnings {
    pub accruals: AccruedTotals {
        pub interval: i128,     // Sum of interval charges
        pub usage: i128,        // Sum of usage charges
        pub one_off: i128,      // Sum of one-off charges
    },
    pub withdrawals: i128,      // Sum of withdrawals
    pub refunds: i128,          // Sum of refunds
}
```

## Accounting Invariant

For any merchant-token pair with sub-accounts, the following must always hold:

```
primary_earnings.accruals.interval
+ primary_earnings.accruals.usage
+ primary_earnings.accruals.one_off
- primary_earnings.withdrawals
- primary_earnings.refunds
== primary_balance + sum(sub_account_balances)
```

Or more concisely:
```
computed_from_accruals == total_balance
```

Use this invariant to validate contract state in integration tests or off-chain audits.

## Charge Routing and Sub-Accounts

When a subscription charges to a sub-account:

1. The `amount` is **deducted** from `subscription.prepaid_balance`
2. The **merchant amount** (after fees) is **credited** to the sub-account balance
3. The **parent's `MerchantEarnings`** are credited (for roll-up reporting)
4. The **parent's `MerchantBalance`** is **reduced** by the merchant amount (to keep total constant)

This means:
- Parent `TokenEarnings` reflect **all** earnings (for compliance/audit)
- Parent `MerchantBalance` excludes sub-account funds (to avoid double-counting)
- Sub-account `balance` (i128) is the only record for each sub-account

The new queries expose all three pieces together, so reporters can reconcile correctly.

## Migration Checklist for Indexers

- [ ] Identify queries using `get_merchant_balance_by_token` that need the full picture
- [ ] Replace with `get_merchant_token_earnings_with_sub_accounts` to get the breakdown
- [ ] Update dashboard displays to show both primary and sub-account balances
- [ ] Use the `total_balance` field for reconciliation/audit reports
- [ ] Validate using the accounting invariant above
- [ ] Test with merchants that have:
  - [ ] No sub-accounts (should have empty `sub_account_balances`)
  - [ ] Multiple sub-accounts
  - [ ] Charges to different sub-accounts in same token
  - [ ] Withdrawals from sub-accounts

## Querying Sub-Account Details Separately

To get details on a single sub-account without the full breakdown:

- `get_sub_account_list(merchant)` — List all registered sub-account labels
- `get_sub_account_balance(merchant, label)` — Balance of one sub-account

These existed before and remain the most efficient for single-sub-account queries.

## Security & Authorization

All new query endpoints are **read-only** and **unauthenticated**, like other balance queries:

- `get_merchant_token_earnings_with_sub_accounts` — Anyone can call
- `get_merchant_total_earnings_with_sub_accounts` — Anyone can call
- `get_merchant_balance_aggregated` — Anyone can call

No new authorization checks are introduced; these queries surface data that was already public via the existing endpoints (just reorganized).

## Testing

Key scenarios to test:

1. **No sub-accounts**: `sub_account_balances` should be empty, `total_balance == primary_balance`
2. **One charge to sub-account**: Primary balance reduced, sub-account balance increased, parent earnings increased
3. **Multiple charges to different sub-accounts**: Each sub-account updated independently
4. **Withdrawal from sub-account**: Sub-account balance reduced, parent earnings withdrawals increased
5. **Multi-token merchant**: Each token has independent primary + sub-account breakdown
6. **Accounting invariant** across all scenarios

See `contracts/subscription_vault/src/test_merchant_sub_accounts.rs` for existing tests.
