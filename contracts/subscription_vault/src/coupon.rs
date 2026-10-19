//! Merchant-managed coupon and discount system (Issue #474).
//!
//! # Lifecycle
//!
//! 1. **Merchant** calls `create_coupon` → stored under `DataKey::Coupon(code)`.
//! 2. **Subscriber** calls `apply_coupon` → binds code to subscription via
//!    `DataKey::SubCoupon(subscription_id)`; marks redemption via
//!    `DataKey::SubCouponRedeemed(subscription_id, code)`; increments
//!    `DataKey::CouponRedemptions(code)`.
//! 3. `charge_one` calls `resolve_coupon_for_charge` / `validate_coupon_for_charge` /
//!    `compute_discount` to apply the discount before fee splitting.
//! 4. **Merchant** may call `revoke_coupon` at any time. Already-bound coupons that have
//!    been revoked are skipped silently at charge time (to avoid billing outages).
//!
//! # Per-Subscription Redemption
//!
//! Once a coupon is applied to a specific subscription, it cannot be reapplied to that same
//! subscription. A second attempt to apply the same coupon to the same subscription returns
//! the `Replay` error (code 4005).
//!
//! # Discount Ordering
//!
//! Percentage discount is applied first, then fixed-amount:
//!
//! ```text
//! after_pct   = gross * (10_000 - percent_off_bps) / 10_000   (integer floor)
//! after_fixed = max(after_pct - fixed_off, 0)                  (clamped to zero)
//! discount    = gross - after_fixed
//! ```
//!
//! The payable amount never goes below zero.
//!
//! # Storage
//!
//! All keys are **persistent** and get TTL extended on every write:
//!
//! | Key | Type | Purpose |
//! |---|---|---|
//! | `DataKey::Coupon(code)` | `Coupon` | Coupon record |
//! | `DataKey::CouponRedemptions(code)` | `u32` | Global bind count |
//! | `DataKey::SubCoupon(subscription_id)` | `Symbol` | Subscription → coupon code |
//! | `DataKey::SubCouponRedeemed(subscription_id, code)` | `bool` | Per-subscription redemption flag |

#![allow(dead_code)]

use crate::types::{
    Coupon, CouponAppliedEvent, CouponCreatedEvent, CouponRevokedEvent, DataKey,
    DiscountAppliedEvent, Error, BASE_EVENT_SCHEMA_VERSION,
    DISCOUNT_APPLIED_EVENT_SCHEMA_VERSION, SUB_TTL_EXTEND_TO, SUB_TTL_THRESHOLD,
};
use soroban_sdk::{Address, Env, Symbol};

// ─────────────────────────────────────────────────────────────────────────────
// Storage helpers
// ─────────────────────────────────────────────────────────────────────────────

fn write_coupon(env: &Env, coupon: &Coupon) {
    let key = DataKey::Coupon(coupon.code.clone());
    env.storage().persistent().set(&key, coupon);
    crate::subscription::maybe_extend_ttl(env, &key, SUB_TTL_THRESHOLD, SUB_TTL_EXTEND_TO);
}

fn read_coupon(env: &Env, code: &Symbol) -> Option<Coupon> {
    env.storage()
        .persistent()
        .get(&DataKey::Coupon(code.clone()))
}

fn read_redemptions(env: &Env, code: &Symbol) -> u32 {
    env.storage()
        .persistent()
        .get::<_, u32>(&DataKey::CouponRedemptions(code.clone()))
        .unwrap_or(0)
}

fn write_redemptions(env: &Env, code: &Symbol, count: u32) {
    let key = DataKey::CouponRedemptions(code.clone());
    env.storage().persistent().set(&key, &count);
    crate::subscription::maybe_extend_ttl(env, &key, SUB_TTL_THRESHOLD, SUB_TTL_EXTEND_TO);
}

fn is_coupon_redeemed(env: &Env, subscription_id: u32, code: &Symbol) -> bool {
    env.storage()
        .persistent()
        .has(&DataKey::SubCouponRedeemed(subscription_id, code.clone()))
}

fn mark_coupon_redeemed(env: &Env, subscription_id: u32, code: &Symbol) {
    let key = DataKey::SubCouponRedeemed(subscription_id, code.clone());
    env.storage().persistent().set(&key, &true);
    crate::subscription::maybe_extend_ttl(env, &key, SUB_TTL_THRESHOLD, SUB_TTL_EXTEND_TO);
}

// ─────────────────────────────────────────────────────────────────────────────
// Public API
// ─────────────────────────────────────────────────────────────────────────────

/// Create a new coupon. Callable by the owning merchant only.
///
/// # Validation
/// - `percent_off_bps` must be in `0..=10_000`.
/// - `fixed_off` must be `>= 0`.
/// - If `expires_at > 0`, it must be strictly in the future.
/// - The coupon code must not already exist.
pub fn create_coupon(
    env: &Env,
    merchant: Address,
    code: Symbol,
    token: Address,
    percent_off_bps: u32,
    fixed_off: i128,
    max_redemptions: u32,
    expires_at: u64,
) -> Result<(), Error> {
    merchant.require_auth();

    if percent_off_bps > 10_000 {
        return Err(Error::InvalidInput);
    }
    if fixed_off <= 0 {
        return Err(Error::InvalidAmount);
    }
    let now = env.ledger().timestamp();
    if expires_at > 0 && expires_at <= now {
        return Err(Error::InvalidInput);
    }

    // Reject duplicate codes.
    if env
        .storage()
        .persistent()
        .has(&DataKey::Coupon(code.clone()))
    {
        return Err(Error::CouponAlreadyExists);
    }

    let coupon = Coupon {
        code: code.clone(),
        merchant: merchant.clone(),
        token: token.clone(),
        percent_off_bps,
        fixed_off,
        max_redemptions,
        expires_at,
        revoked: false,
    };
    write_coupon(env, &coupon);

    env.events().publish(
        (Symbol::new(env, "coupon_created"), merchant.clone()),
        CouponCreatedEvent {
            merchant,
            code,
            token,
            percent_off_bps,
            fixed_off,
            max_redemptions,
            expires_at,
            timestamp: now,
            schema_version: BASE_EVENT_SCHEMA_VERSION,
            contract_id: env.current_contract_address(),
        },
    );
    Ok(())
}

/// Revoke an existing coupon. Only the owning merchant may revoke.
///
/// Revoked coupons cannot be applied by new subscribers. Already-bound
/// coupons are skipped silently at charge time.
pub fn revoke_coupon(env: &Env, merchant: Address, code: Symbol) -> Result<(), Error> {
    merchant.require_auth();

    let mut coupon = read_coupon(env, &code).ok_or(Error::CouponNotFound)?;
    if coupon.merchant != merchant {
        return Err(Error::Unauthorized);
    }

    coupon.revoked = true;
    write_coupon(env, &coupon);

    env.events().publish(
        (Symbol::new(env, "coupon_revoked"), merchant.clone()),
        CouponRevokedEvent {
            merchant,
            code,
            timestamp: env.ledger().timestamp(),
            schema_version: BASE_EVENT_SCHEMA_VERSION,
            contract_id: env.current_contract_address(),
        },
    );
    Ok(())
}

/// Bind a coupon to a subscription. Callable by the subscription's subscriber only.
///
/// Increments the global redemption counter at bind time. A subscription can
/// hold at most one coupon (`CouponAlreadyApplied` if already bound).
/// Once a coupon has been applied to a subscription, it cannot be reapplied
/// (`Replay` error on second attempt).
pub fn apply_coupon(
    env: &Env,
    subscriber: Address,
    subscription_id: u32,
    code: Symbol,
) -> Result<(), Error> {
    subscriber.require_auth();

    // Load subscription and verify ownership.
    let sub = crate::queries::get_subscription(env, subscription_id)?;
    if sub.subscriber != subscriber {
        return Err(Error::Unauthorized);
    }

    // Check if this specific (subscription_id, code) pair has already been redeemed.
    if is_coupon_redeemed(env, subscription_id, &code) {
        return Err(Error::Replay);
    }

    // One coupon per subscription.
    if env
        .storage()
        .persistent()
        .has(&DataKey::SubCoupon(subscription_id))
    {
        return Err(Error::CouponAlreadyApplied);
    }

    let coupon = read_coupon(env, &code).ok_or(Error::CouponNotFound)?;

    if coupon.revoked {
        return Err(Error::CouponRevoked);
    }

    let now = env.ledger().timestamp();
    if coupon.expires_at > 0 && now >= coupon.expires_at {
        return Err(Error::CouponExpired);
    }

    let observed_redemptions = read_redemptions(env, &code);
    if coupon.max_redemptions > 0 && observed_redemptions >= coupon.max_redemptions {
        return Err(Error::CouponRedemptionLimitReached);
    }

    // Token must match the subscription's settlement token.
    if coupon.token != sub.token {
        return Err(Error::CouponTokenMismatch);
    }

    // Atomically claim a redemption slot (CAS against the value we validated).
    // Done before any other write so a failure leaves no partial state.
    cas_increment_redemptions(env, &code, observed_redemptions, coupon.max_redemptions)?;

    // Bind coupon to subscription.
    let sub_key = DataKey::SubCoupon(subscription_id);
    env.storage().persistent().set(&sub_key, &code);
    crate::subscription::maybe_extend_ttl(env, &sub_key, SUB_TTL_THRESHOLD, SUB_TTL_EXTEND_TO);

    // Mark this (subscription_id, code) pair as redeemed.
    mark_coupon_redeemed(env, subscription_id, &code);

    env.events().publish(
        (Symbol::new(env, "coupon_applied"), subscription_id),
        CouponAppliedEvent {
            subscription_id,
            subscriber,
            code,
            timestamp: now,
            schema_version: BASE_EVENT_SCHEMA_VERSION,
            contract_id: env.current_contract_address(),
        },
    );
    Ok(())
}

/// Return the coupon bound to this subscription, if any.
pub fn resolve_coupon_for_charge(env: &Env, subscription_id: u32) -> Option<Coupon> {
    let code: Symbol = env
        .storage()
        .persistent()
        .get(&DataKey::SubCoupon(subscription_id))?;
    read_coupon(env, &code)
}

/// Return a coupon by code, or `None` if it does not exist.
pub fn get_coupon(env: &Env, code: Symbol) -> Option<Coupon> {
    read_coupon(env, &code)
}

/// Validate that a coupon is still usable at charge time.
///
/// Returns `Ok(())` if the coupon should be applied, or an error if it should
/// be skipped. Callers in `charge_one` catch these errors and skip the discount
/// without failing the charge, to avoid billing outages.
pub fn validate_coupon_for_charge(
    env: &Env,
    now: u64,
    sub_token: &Address,
    coupon: &Coupon,
) -> Result<(), Error> {
    if coupon.revoked {
        return Err(Error::CouponRevoked);
    }
    if coupon.expires_at > 0 && now >= coupon.expires_at {
        return Err(Error::CouponExpired);
    }
    if coupon.token != *sub_token {
        return Err(Error::CouponTokenMismatch);
    }
    // Note: redemption limit is NOT re-checked here — the limit was already
    // enforced at apply_coupon time. Re-checking at charge time would wrongly
    // block subscribers who legitimately bound the coupon before the limit was
    // reached.
    Ok(())
}

/// Compute the discount amount for a given gross charge and coupon.
///
/// Returns the discount (non-negative). The caller subtracts this from gross.
///
/// # Discount ordering
///
/// 1. Percentage discount applied first (reduces gross by `percent_off_bps`).
/// 2. Fixed discount applied to the result of step 1.
/// 3. Payable amount is clamped to `[0, gross]`.
///
/// # Coupon vs. fee ordering (Issue #475)
///
/// The discount returned here is computed on the **gross** charge amount,
/// *before* the protocol fee is deducted. The caller (`charge_one`) applies
/// the discount first, then computes the fee on the resulting payable amount.
/// This ensures the merchant — not the treasury — absorbs the coupon
/// discount, and the invariant `Gross = Discount + Merchant Net + Treasury
/// Fee` holds. See the comment block at the call site in `charge_core.rs`
/// for a worked example.
pub fn compute_discount(gross: i128, coupon: &Coupon) -> i128 {
    // Step 1 — percentage
    let after_pct = if coupon.percent_off_bps > 0 {
        let remaining_bps = 10_000i128 - coupon.percent_off_bps as i128;
        // Integer floor division: mirrors the protocol-fee pattern.
        gross * remaining_bps / 10_000i128
    } else {
        gross
    };

    // Step 2 — fixed (saturating subtraction, clamp to zero)
    let after_fixed = after_pct.saturating_sub(coupon.fixed_off).max(0);

    // Discount = gross - payable. Always in [0, gross].
    gross.saturating_sub(after_fixed)
}

/// Increment the global redemption counter for a coupon code.
///
/// Called once per successful `apply_coupon` binding.
pub fn increment_redemptions(env: &Env, code: &Symbol) {
    let count = read_redemptions(env, code);
    write_redemptions(env, code, count.saturating_add(1));
}

/// Compare-and-swap increment of the redemption counter (Issue #388).
///
/// Succeeds only if the stored counter still equals `expected` (the value the
/// caller read when validating) and the post-increment value stays within
/// `max_redemptions` (0 = unlimited). A stale read or a full coupon yields
/// `CouponRedemptionLimitReached`, so two interleaved binders can never both
/// consume the last slot.
pub fn cas_increment_redemptions(
    env: &Env,
    code: &Symbol,
    expected: u32,
    max_redemptions: u32,
) -> Result<(), Error> {
    let current = read_redemptions(env, code);
    if current != expected {
        return Err(Error::CouponRedemptionLimitReached);
    }
    if max_redemptions > 0 && current >= max_redemptions {
        return Err(Error::CouponRedemptionLimitReached);
    }
    write_redemptions(env, code, current.saturating_add(1));
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal charge-path helper: apply discount + emit event
// ─────────────────────────────────────────────────────────────────────────────

/// Apply coupon discount to `gross` at charge time.
///
/// Returns `(payable, discount)`. If the coupon is revoked, expired, or token-
/// mismatched the discount is silently skipped (`(gross, 0)`).
pub fn apply_discount_at_charge(
    env: &Env,
    subscription_id: u32,
    now: u64,
    sub_token: &Address,
    gross: i128,
) -> (i128, i128) {
    let coupon = match resolve_coupon_for_charge(env, subscription_id) {
        Some(c) => c,
        None => return (gross, 0),
    };

    match validate_coupon_for_charge(env, now, sub_token, &coupon) {
        Err(_) => {
            // Expired or revoked — skip silently; billing must not be blocked.
            (gross, 0)
        }
        Ok(()) => {
            let discount = compute_discount(gross, &coupon);
            let payable = gross - discount; // discount <= gross is guaranteed

            env.events().publish(
                (Symbol::new(env, "discount_applied"), subscription_id),
                DiscountAppliedEvent {
                    subscription_id,
                    gross_amount: gross,
                    discount_amount: discount,
                    discounted_amount: payable,
                    coupon_code: coupon.code,
                    timestamp: now,
                    batch_nonce: crate::admin::current_batch_nonce(env),
                    schema_version: DISCOUNT_APPLIED_EVENT_SCHEMA_VERSION,
                    contract_id: env.current_contract_address(),
                },
            );

            (payable, discount)
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests: coupon discount vs. fee ordering (Issue #475)
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod coupon_fee_ordering_tests {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{Address, Env, Symbol};

    /// The discount must be computed on the **gross** amount, not the
    /// post-fee amount. This test asserts that the discount + fee + merchant
    /// net sums to the gross exactly when the discount is applied first.
    #[test]
    fn discount_on_gross_preserves_fund_conservation() {
        let gross = 10_000i128;
        let percent_off_bps = 2_000u32; // 20 %
        let fixed_off = 0i128;
        let fee_bps = 500u32; // 5 %

        let coupon = Coupon {
            code: Symbol::new(&Env::default(), "TEST"),
            merchant: Address::generate(&Env::default()),
            token: Address::generate(&Env::default()),
            percent_off_bps,
            fixed_off,
            max_redemptions: 0,
            expires_at: 0,
            revoked: false,
        };

        // Step 1 — discount on gross
        let discount = compute_discount(gross, &coupon);
        let payable = gross - discount;

        // Step 2 — fee on payable (post-discount)
        let fee = payable * fee_bps as i128 / 10_000i128;
        let merchant_net = payable - fee;

        // Invariant: Gross = Discount + Merchant Net + Treasury Fee
        assert_eq!(
            discount + merchant_net + fee,
            gross,
            "fund conservation must hold when discount is applied to gross"
        );

        // Specific values: gross=10_000, 20% off → payable=8_000,
        // 5% fee on 8_000 → fee=400, merchant_net=7_600, discount=2_000
        assert_eq!(payable, 8_000);
        assert_eq!(discount, 2_000);
        assert_eq!(fee, 400);
        assert_eq!(merchant_net, 7_600);
    }

    /// If the discount were (incorrectly) applied *after* the fee, the merchant
    /// would absorb part of the discount. This test demonstrates the
    /// *incorrect* ordering produces different merchant payouts for non-round
    /// values, confirming the current (correct) ordering matters.
    #[test]
    fn discount_on_gross_differs_from_discount_on_post_fee() {
        // Choose values where the two orderings diverge:
        //   gross = 9_999, coupon = 33 %, fee = 7 %
        let gross = 9_999i128;
        let percent_off_bps = 3_300u32; // 33 %
        let fee_bps = 700u32; // 7 %

        let coupon = Coupon {
            code: Symbol::new(&Env::default(), "DIV"),
            merchant: Address::generate(&Env::default()),
            token: Address::generate(&Env::default()),
            percent_off_bps,
            fixed_off: 0,
            max_redemptions: 0,
            expires_at: 0,
            revoked: false,
        };

        // Correct ordering: discount on gross, then fee on payable
        let discount_correct = compute_discount(gross, &coupon);
        let payable_correct = gross - discount_correct;
        let fee_correct = payable_correct * fee_bps as i128 / 10_000i128;
        let merchant_net_correct = payable_correct - fee_correct;

        // Incorrect ordering: fee on gross, then discount on post-fee
        let fee_wrong = gross * fee_bps as i128 / 10_000i128;
        let post_fee = gross - fee_wrong;
        let discount_wrong = post_fee * percent_off_bps as i128 / 10_000i128;
        let merchant_net_wrong = post_fee - discount_wrong;

        // The two orderings produce different merchant payouts.
        assert_ne!(
            merchant_net_correct, merchant_net_wrong,
            "correct and incorrect orderings must diverge for non-round values"
        );

        // The correct ordering preserves: Gross = Discount + Merchant Net + Fee
        assert_eq!(discount_correct + merchant_net_correct + fee_correct, gross);
    }

    /// CAS increment rejects stale reads and enforces the cap (Issue #388).
    #[test]
    fn cas_increment_rejects_stale_read_and_full_coupon() {
        let env = Env::default();
        let contract_id = env.register(crate::SubscriptionVault, ());
        let code = Symbol::new(&env, "CAS");

        env.as_contract(&contract_id, || {
            // First claimer wins the only slot.
            assert_eq!(cas_increment_redemptions(&env, &code, 0, 1), Ok(()));
            // Second claimer holding the stale read (0) is rejected.
            assert_eq!(
                cas_increment_redemptions(&env, &code, 0, 1),
                Err(Error::CouponRedemptionLimitReached)
            );
            // Fresh read but coupon is full: rejected.
            assert_eq!(
                cas_increment_redemptions(&env, &code, 1, 1),
                Err(Error::CouponRedemptionLimitReached)
            );
            assert_eq!(read_redemptions(&env, &code), 1);
            // Unlimited coupon (0) still advances with a fresh read.
            assert_eq!(cas_increment_redemptions(&env, &code, 1, 0), Ok(()));
            assert_eq!(read_redemptions(&env, &code), 2);
        });
    }

    /// Verify the full `apply_discount_at_charge` path returns a payable that,
    /// when the fee is applied, preserves the fund conservation invariant.
    #[test]
    fn apply_discount_then_fee_preserves_invariant() {
        let env = Env::default();
        env.mock_all_auths();

        let gross = 5_000i128;
        let fee_bps = 300u32; // 3 %

        // No coupon bound → payable = gross, discount = 0
        let (payable, discount) =
            apply_discount_at_charge(&env, 999, 0, &Address::generate(&env), gross);
        assert_eq!(payable, gross);
        assert_eq!(discount, 0);

        // With no discount, fee = gross * fee_bps / 10_000
        let fee = payable * fee_bps as i128 / 10_000i128;
        let merchant_net = payable - fee;
        assert_eq!(discount + merchant_net + fee, gross);
    }
}
