//! Single charge logic (no auth). Used by charge_subscription and batch_charge.
//!
//! Charge runs only when status is Active or GracePeriod. On insufficient balance the
//! subscription is moved to a recoverable non-active state and an explicit failure
//! event is emitted without mutating financial accounting state.
//! On lifetime cap exhaustion the subscription is cancelled (terminal state).
//!
//! See `docs/subscription_lifecycle.md` for lifecycle details.
//! See `docs/lifetime_caps.md` for cap enforcement semantics.
//!
//! **PRs that only change how one subscription is charged should edit this file only.**
//!
//! # Reentrancy Safety
//!
//! This module does **not make external token transfers**. All state mutations happen
//! before any external interactions:
//!
//! 1. **Checks**: Validate expiration, status, interval, balance, replay protection, caps
//! 2. **Effects**: Update subscription state AND merchant earnings in storage
//! 3. **Interactions**: None (no external calls in this module)
//!
//! The public entry-point `lib.rs::charge_subscription` acquires a `ReentrancyGuard`
//! before calling `charge_one`, providing defense-in-depth protection even though
//! this function is naturally safe from external reentrancy.
//!
//! Merchant crediting happens through internal calls to `merchant::credit_merchant_balance_for_token`,
//! which only updates storage and does not call external contracts.
//!
//! See `docs/reentrancy_hardening.md` for complete charge path analysis.

#![allow(dead_code)]

use crate::oracle_adapter::{dispatch_price, PRICE_SCALE};
use crate::queries::get_subscription;
use crate::safe_math::{safe_add, safe_sub, safe_sub_balance};
use crate::state_machine::transition_to;
use crate::statements::append_statement;
use crate::subscription::{next_charge_time, write_subscription};
use crate::types::{
    BillingChargeKind, BillingPeriodSnapshot, ChargeExecutionResult, ChargeFailureEvent, DataKey,
    Error, FeeConvertedEvent, GracePeriodEnteredEvent, LifetimeCapReachedEvent,
    SubscriptionAutoPausedEvent, SubscriptionCancelledEvent, SubscriptionChargeFailedEvent,
    SubscriptionChargedEvent, SubscriptionStatus, UsageChargeRejectedEvent, UsageChargeResult,
    UsageLimits, UsageState, UsageStatementEvent, SNAPSHOT_FLAG_CLOSED,
    SNAPSHOT_FLAG_INTERVAL_CHARGED, SNAPSHOT_FLAG_USAGE_CHARGED, TOPIC_CHARGED,
};
use soroban_sdk::{Address, Env, String, Symbol};

/// Resolve the effective fee rate in basis points for a charge to `merchant`.
///
/// Priority:
/// 1. If a per-merchant override is set (`DataKey::MerchantFeeBps`), use it.
/// 2. Otherwise fall back to the global `DataKey::FeeBps`.
///
/// A zero return value means no fee is collected.
#[inline(always)]
fn route_fee_bps(env: &Env, merchant: &soroban_sdk::Address) -> u32 {
    if let Some(override_bps) = crate::merchant::get_merchant_fee_override_bps(env, merchant) {
        return override_bps;
    }
    crate::admin::get_protocol_fee_bps(env)
}

/// Emits a [`ChargeFailureEvent`] and returns `err` unchanged.
///
/// Call as `return Err(charge_fail(env, id, err, attempted, now))` on every
/// error path inside charge entry-points so that all failures are observable
/// by off-chain indexers regardless of error type.
#[inline(always)]
fn charge_fail(
    env: &Env,
    subscription_id: u32,
    err: Error,
    attempted_amount: i128,
    ledger: u64,
) -> Error {
    env.events().publish(
        (Symbol::new(env, "charge_failed_v2"), subscription_id),
        ChargeFailureEvent {
            subscription_id,
            error_code: err.to_code(),
            attempted_amount,
            ledger,
            batch_nonce: crate::admin::current_batch_nonce(env),
            schema_version: crate::types::CHARGE_FAILURE_EVENT_SCHEMA_VERSION,
            contract_id: env.current_contract_address(),
        },
    );
    err
}

/// Result of a fee-token conversion attempt.
struct FeeConversion {
    /// Converted fee amount in the target token. Equals `original` when no
    /// conversion took place.
    effective_amount: i128,
    /// Target token address, or `None` when no conversion was applied.
    target_token: Option<Address>,
    /// Oracle price used for conversion (quote per base, scaled by 10^7).
    /// 0 when no conversion was applied.
    rate: u128,
}

/// Attempt to convert a fee amount from `source_token` to the configured
/// fee-token override using the oracle.
///
/// Returns the fee amount unchanged (with `target_token = None`) when:
/// - No fee-token override is configured.
/// - The override matches `source_token`.
/// - The oracle is not available (not configured, or price fetch fails).
/// - Conversion would round to zero (precision loss guard).
fn convert_fee(
    env: &Env,
    merchant: &Address,
    source_token: &Address,
    fee_amount: i128,
    price_cache: Option<&mut Vec<(Address, Address, u128)>>,
) -> FeeConversion {
    let fee_token_opt = crate::admin::get_fee_token(env);
    let fee_token = match fee_token_opt {
        Some(ref t) if t != source_token => t.clone(),
        _ => {
            return FeeConversion {
                effective_amount: fee_amount,
                target_token: None,
                rate: 0,
            };
        }
    };

    let oracle_config = crate::oracle::get_oracle_config(env);
    if !oracle_config.enabled || oracle_config.oracle.is_none() {
        return FeeConversion {
            effective_amount: fee_amount,
            target_token: None,
            rate: 0,
        };
    }

    // Reuse a previously-dispatched price for the same (merchant, token) pair
    // within this batch so the oracle is not queried redundantly.
    let cached = price_cache.as_ref().and_then(|cache| {
        cache
            .iter()
            .find(|(m, t, _)| m == merchant && t == source_token)
            .map(|(_, _, p)| *p)
    });

    let price_result = match cached {
        Some(p) => Ok(p),
        None => dispatch_price(env, &oracle_config, source_token, &fee_token),
    };

    match price_result {
        Ok(price) => {
            if cached.is_none() {
                if let Some(cache) = price_cache {
                    cache.push((merchant.clone(), source_token.clone(), price));
                }
            }
            let converted = (fee_amount as u128)
                .checked_mul(price)
                .and_then(|v| v.checked_div(PRICE_SCALE))
                .unwrap_or(0) as i128;

            if converted > 0 {
                FeeConversion {
                    effective_amount: converted,
                    target_token: Some(fee_token),
                    rate: price,
                }
            } else {
                FeeConversion {
                    effective_amount: fee_amount,
                    target_token: None,
                    rate: 0,
                }
            }
        }
        Err(_) => FeeConversion {
            effective_amount: fee_amount,
            target_token: None,
            rate: 0,
        },
    }
}

/// Performs a single interval-based charge with optional replay protection.
///
/// `merchant_cache` lets callers that charge many subscriptions in one call
/// (e.g. `execute_batch_charge`) reuse the paused/vacation lookup for a
/// merchant across every subscription that shares it, instead of re-reading
/// `DataKey::MerchantPaused`/`DataKey::MerchantVacation` from storage on
/// every iteration. Pass `None` for single-subscription charge paths.
pub fn charge_one(
    env: &Env,
    subscription_id: u32,
    now: u64,
    idempotency_key: Option<soroban_sdk::BytesN<32>>,
    admin_config: Option<&crate::admin::CachedAdminConfig>,
    mut merchant_cache: Option<&mut soroban_sdk::Map<Address, (bool, bool)>>,
) -> Result<ChargeExecutionResult, Error> {
    // ── CRITICAL: Atomic emergency stop check ────────────────────────────────
    // Re-check emergency stop on every iteration to prevent in-flight batch_charge
    // from completing when emergency_stop is triggered mid-execution.
    // This fixes the race condition where the flag is only checked at entry-point.
    if crate::admin::read_config(env, &crate::types::DataKey::EmergencyStop).unwrap_or(false) {
        return Err(charge_fail(
            env,
            subscription_id,
            crate::types::Error::EmergencyStopActive,
            0,
            now,
        ));
    }

    let mut sub = get_subscription(env, subscription_id)
        .map_err(|e| charge_fail(env, subscription_id, e, 0, now))?;

    // Merchant pause/vacation status — cached per merchant within a batch so
    // subscriptions sharing a merchant don't each re-read the same storage.
    let (merchant_paused, merchant_in_vacation) = match merchant_cache.as_deref_mut() {
        Some(cache) => {
            if let Some(cached) = cache.get(sub.merchant.clone()) {
                cached
            } else {
                let status = (
                    crate::merchant::get_merchant_paused(env, sub.merchant.clone()),
                    crate::merchant::is_merchant_in_vacation(env, &sub.merchant, now),
                );
                cache.set(sub.merchant.clone(), status);
                status
            }
        }
        None => (
            crate::merchant::get_merchant_paused(env, sub.merchant.clone()),
            crate::merchant::is_merchant_in_vacation(env, &sub.merchant, now),
        ),
    };

    // Merchant pause guard — mirrors charge_usage_one enforcement
    if merchant_paused {
        return Err(charge_fail(
            env,
            subscription_id,
            Error::MerchantPaused,
            0,
            now,
        ));
    }

    // Merchant vacation guard — block charges during vacation window
    if merchant_in_vacation {
        return Err(charge_fail(
            env,
            subscription_id,
            Error::VacationActive,
            0,
            now,
        ));
    }

    crate::blocklist::require_not_blocklisted(env, &sub.subscriber)
        .map_err(|e| charge_fail(env, subscription_id, e, 0, now))?;
    crate::blocklist::require_not_blocklisted(env, &sub.merchant)
        .map_err(|e| charge_fail(env, subscription_id, e, 0, now))?;

    if let Some(split_payees) = crate::subscription::get_split_payees(env, subscription_id) {
        for entry in split_payees.entries.iter() {
            let (payee, _) = entry;
            crate::blocklist::require_not_blocklisted(env, &payee)
                .map_err(|e| charge_fail(env, subscription_id, e, 0, now))?;
            if crate::merchant::get_merchant_paused(env, payee.clone()) {
                return Err(charge_fail(
                    env,
                    subscription_id,
                    Error::MerchantPaused,
                    0,
                    now,
                ));
            }
            if crate::merchant::is_merchant_in_vacation(env, &payee, now) {
                return Err(charge_fail(
                    env,
                    subscription_id,
                    Error::VacationActive,
                    0,
                    now,
                ));
            }
        }
    }

    // Expiration guard
    if sub.is_expired(now, env.ledger().sequence()) {
        if sub.status != SubscriptionStatus::Expired {
            transition_to(&mut sub.status, SubscriptionStatus::Expired)?;
            write_subscription(env, subscription_id, &sub);
            env.events().publish(
                (Symbol::new(env, "subscription_expired"), subscription_id),
                crate::types::SubscriptionExpiredEvent {
                    subscription_id,
                    timestamp: now,
                    batch_nonce: crate::admin::current_batch_nonce(env),
                    schema_version: crate::types::SUBSCRIPTION_EXPIRED_EVENT_SCHEMA_VERSION,
                    contract_id: env.current_contract_address(),
                },
            );
        }
        return Err(charge_fail(
            env,
            subscription_id,
            Error::SubscriptionExpired,
            0,
            now,
        ));
    }

    let charge_amount = crate::oracle::resolve_charge_amount(env, subscription_id, &sub)
        .map_err(|e| charge_fail(env, subscription_id, e, 0, now))?;

    // ── Coupon discount (applied to gross, before protocol-fee split) ──────────
    //
    // Intended semantics (Issue #475):
    //
    // The coupon discount is applied to the **gross** charge amount — the
    // oracle-resolved amount *before* the protocol fee is deducted. The fee
    // split and merchant credit then operate on the **post-discount payable**
    // (`charge_amount`). This ordering is deliberate:
    //
    //   gross = 10_000, coupon = 20 %, fee = 5 %
    //
    //   Step 1 — discount on gross:   payable = 10_000 * 8_000 / 10_000 = 8_000
    //   Step 2 — fee on payable:      fee = 8_000 * 500 / 10_000 = 400
    //                                merchant_net = 8_000 - 400 = 7_600
    //
    // If the discount were applied *after* the fee, the merchant would absorb
    // part of the discount (fee would be computed on 10_000, then the coupon
    // would reduce the merchant's net share), yielding a different payout:
    //
    //   fee = 10_000 * 500 / 10_000 = 500
    //   merchant_net = (10_000 - 500) * 8_000 / 10_000 = 7_600  (same only by
    //   coincidence for these numbers; diverges for non-round values)
    //
    // The invariant preserved is:
    //
    //   Gross = Discount + Merchant Net + Treasury Fee
    //
    // See also `coupon::compute_discount` for the percentage-then-fixed
    // ordering within the discount itself.
    let (mut charge_amount, _discount_amount) = crate::coupon::apply_discount_at_charge(
        env,
        subscription_id,
        now,
        &sub.token,
        charge_amount,
    );

    // ── Proration for partial first billing period ───────────────────────────
    // If proration is enabled and this is the first charge (last_payment_timestamp == start_time),
    // scale the charge by the elapsed time within the first interval.
    if sub.proration_enabled && sub.last_payment_timestamp == sub.start_time {
        let elapsed_seconds = now.saturating_sub(sub.start_time);
        charge_amount =
            calculate_prorated_first_charge(charge_amount, sub.interval_seconds, elapsed_seconds)
                .map_err(|e| charge_fail(env, subscription_id, e, charge_amount, now))?;
    }

    if let Some(cap) = sub.lifetime_cap {
        if crate::subscription::lifetime_cap_reached(cap, sub.lifetime_charged) {
            if sub.status != SubscriptionStatus::Cancelled {
                transition_to(&mut sub.status, SubscriptionStatus::Cancelled)?;
                write_subscription(env, subscription_id, &sub);
                env.events().publish(
                    (Symbol::new(env, "lifetime_cap_reached"), subscription_id),
                    LifetimeCapReachedEvent {
                        subscription_id,
                        lifetime_cap: cap,
                        lifetime_charged: sub.lifetime_charged,
                        timestamp: now,
                        batch_nonce: crate::admin::current_batch_nonce(env),
                        schema_version: crate::types::LIFETIME_CAP_REACHED_EVENT_SCHEMA_VERSION,
                        contract_id: env.current_contract_address(),
                    },
                );
            }
            return Ok(ChargeExecutionResult::LifetimeCapReached);
        }
    }

    // Scheduled cancellation: fire when cancel_at has arrived.
    if let Some(cancel_at) = sub.cancel_at {
        if now >= cancel_at {
            if sub.status != SubscriptionStatus::Cancelled {
                transition_to(&mut sub.status, SubscriptionStatus::Cancelled)?;
                let refund_amount = sub.prepaid_balance;
                sub.prepaid_balance = 0;
                sub.cancel_at = None;
                let token_addr = sub.token.clone();
                write_subscription(env, subscription_id, &sub);
                if refund_amount > 0 {
                    let token_client = soroban_sdk::token::Client::new(env, &token_addr);
                    token_client.transfer(
                        &env.current_contract_address(),
                        &sub.subscriber,
                        &refund_amount,
                    );
                    crate::accounting::sub_total_accounted(env, &token_addr, refund_amount)?;
                }
                env.events().publish(
                    (
                        soroban_sdk::Symbol::new(env, "subscription_cancelled"),
                        subscription_id,
                    ),
                    SubscriptionCancelledEvent {
                        subscription_id,
                        subscriber: sub.subscriber.clone(),
                        merchant: sub.merchant.clone(),
                        token: sub.token.clone(),
                        authorizer: sub.subscriber.clone(),
                        refund_amount,
                        timestamp: now,
                        batch_nonce: crate::admin::current_batch_nonce(env),
                        schema_version: crate::types::SUBSCRIPTION_CANCELLED_EVENT_SCHEMA_VERSION,
                        contract_id: env.current_contract_address(),
                    },
                );
            }
            return Ok(ChargeExecutionResult::ScheduledCancellation);
        }
    }

    if sub.status != SubscriptionStatus::Active && sub.status != SubscriptionStatus::GracePeriod {
        if sub.status == SubscriptionStatus::InsufficientBalance {
            let next_allowed = next_charge_time(sub.last_payment_timestamp, sub.interval_seconds)?;
            if now < next_allowed {
                return Err(charge_fail(
                    env,
                    subscription_id,
                    Error::NotActive,
                    charge_amount,
                    now,
                ));
            }
        } else {
            return Err(charge_fail(
                env,
                subscription_id,
                Error::NotActive,
                charge_amount,
                now,
            ));
        }
    }

    // ── Auto-renewal gate ────────────────────────────────────────────────────
    // When auto_renew is false the billing engine skips the charge once the
    // interval has elapsed. The charge is silently skipped (not an error) so
    // that batch operations can continue past non-renewing subscriptions.
    if !sub.auto_renew {
        let next_allowed = next_charge_time(sub.last_payment_timestamp, sub.interval_seconds)?;
        if now >= next_allowed {
            // Interval has elapsed but auto-renewal is disabled — skip.
            return Ok(ChargeExecutionResult::Skipped);
        }
        // Interval hasn't elapsed yet: fall through to IntervalNotElapsed below.
    }

    let period_index = now.saturating_sub(sub.start_time) / sub.interval_seconds;
    let period_start = sub
        .start_time
        .checked_add(period_index.saturating_mul(sub.interval_seconds))
        .unwrap_or(u64::MAX);
    let period_end = period_start
        .checked_add(sub.interval_seconds)
        .unwrap_or(u64::MAX);

    // Anti-frontrunning salt
    let seq = env.ledger().sequence();
    let salt = {
        let mut salt_buf = [0u8; 20];
        salt_buf[..4].copy_from_slice(&subscription_id.to_be_bytes());
        salt_buf[4..12].copy_from_slice(&sub.last_payment_timestamp.to_be_bytes());
        salt_buf[12..20].copy_from_slice(&seq.to_be_bytes());
        let salt_input = soroban_sdk::Bytes::from_slice(env, &salt_buf);
        let hash: soroban_sdk::BytesN<32> = env.crypto().sha256(&salt_input).into();
        hash
    };

    let salt_key = DataKey::ChargeSalt(subscription_id);
    if let Some(last_salt) = env
        .storage()
        .instance()
        .get::<_, soroban_sdk::BytesN<32>>(&salt_key)
    {
        if last_salt == salt {
            return Err(charge_fail(
                env,
                subscription_id,
                Error::Replay,
                charge_amount,
                now,
            ));
        }
    }

    // Idempotent return: same idempotency key already processed
    if let Some(ref k) = idempotency_key {
        let hashed = crate::idempotency::hash_idem_key(
            env,
            crate::nonce::DOMAIN_CHARGE_INTERVAL.as_u32(),
            subscription_id,
            k,
        );
        if crate::idempotency::check_key(env, subscription_id, &hashed) {
            return Ok(ChargeExecutionResult::Charged);
        }
    }

    // Replay: already charged for this billing period
    if let Some(stored_period) = env
        .storage()
        .instance()
        .get::<_, u64>(&DataKey::ChargedPeriod(subscription_id))
    {
        if period_index <= stored_period {
            return Err(charge_fail(
                env,
                subscription_id,
                Error::Replay,
                charge_amount,
                now,
            ));
        }
    }

    let next_allowed = next_charge_time(sub.last_payment_timestamp, sub.interval_seconds)?;
    if now < next_allowed {
        return Err(charge_fail(
            env,
            subscription_id,
            Error::IntervalNotElapsed,
            charge_amount,
            now,
        ));
    }

    // -- Lifetime cap pre-check -----------------------------------------------
    if let Some(cap) = sub.lifetime_cap {
        let remaining = if crate::subscription::lifetime_cap_reached(cap, sub.lifetime_charged) {
            0
        } else {
            safe_sub(cap, sub.lifetime_charged)?
        };

        if remaining == 0 || charge_amount > remaining {
            // Cap already exhausted or this charge would exceed it: cancel without
            // moving funds and return an explicit terminal error.
            transition_to(&mut sub.status, SubscriptionStatus::Cancelled)?;
            write_subscription(env, subscription_id, &sub);

            env.events().publish(
                (Symbol::new(env, "lifetime_cap_reached"), subscription_id),
                LifetimeCapReachedEvent {
                    subscription_id,
                    lifetime_cap: cap,
                    lifetime_charged: sub.lifetime_charged,
                    timestamp: now,
                    batch_nonce: crate::admin::current_batch_nonce(env),
                    schema_version: crate::types::LIFETIME_CAP_REACHED_EVENT_SCHEMA_VERSION,
                    contract_id: env.current_contract_address(),
                },
            );

            return Ok(ChargeExecutionResult::LifetimeCapReached);
        }
    }

    let storage = env.storage().instance();

    match safe_sub_balance(sub.prepaid_balance, charge_amount) {
        Ok(new_balance) => {
            sub.prepaid_balance = new_balance;
            let (fee_bps, treasury_opt) = if let Some(cfg) = admin_config {
                (cfg.fee_bps, cfg.treasury.clone())
            } else {
                (
                    crate::admin::get_protocol_fee_bps(env),
                    crate::admin::get_treasury(env),
                )
            };
            // Determine the protocol fee and merchant credit.
            //
            // Rounding rule: percentage fee is computed with integer division
            // (truncating). Any remainder from the division is deterministically
            // allocated to the merchant credit (i.e. merchant receives
            // `charge_amount - fee`). This ensures `merchant_amount + fee_amount == charge_amount`
            // exactly in the charge token and prevents 1-unit dust from remaining
            // in the vault. Converted fees (fee-token overrides) are handled
            // separately and do not affect the source-token accounting invariant.
            //
            // Minimum-fee floor: if fee_bps > 0 and a treasury is configured but
            // the integer division rounds to 0 (charge_amount < 10_000 / fee_bps),
            // we enforce a minimum fee of 1 base unit. This prevents fee evasion
            // via very small charge amounts. The floor is only applied when there
            // is an active treasury; if no treasury is set the fee is always 0.
            let (merchant_amount, fee_amount) = if fee_bps > 0 {
                if let Some(ref _t) = treasury_opt {
                    let fee = (charge_amount * fee_bps as i128 / 10_000i128).max(1);
                    let net = safe_sub(charge_amount, fee)?;
                    (net, fee)
                } else {
                    (charge_amount, 0i128)
                }
            } else {
                (charge_amount, 0i128)
            };

            // Invariant sanity check: the split must sum exactly to the charged amount.
            // If this ever fails it indicates an arithmetic bug; keep as a debug
            // assertion so normal execution is unaffected in release builds.
            debug_assert!(
                merchant_amount + fee_amount == charge_amount,
                "fee + merchant != charge_amount (fee routing invariant)"
            );
            credit_charge_payees(
                env,
                subscription_id,
                &sub,
                merchant_amount,
                BillingChargeKind::Interval,
            )?;

            // Route merchant amount to sub-account if subscription has one
            if let Some(ref label) = sub.sub_account_label {
                crate::merchant::credit_sub_account(
                    env,
                    &sub.merchant,
                    label,
                    &sub.token,
                    merchant_amount,
                )?;
                // Deduct from parent balance (parent earnings stay for roll-up reporting)
                let parent_bal =
                    crate::merchant::get_merchant_balance_by_token(env, &sub.merchant, &sub.token);
                let new_parent_bal = crate::safe_math::safe_sub(parent_bal, merchant_amount)?;
                crate::merchant::set_merchant_balance(
                    env,
                    &sub.merchant,
                    &sub.token,
                    &new_parent_bal,
                );
            }

            let conversion = if fee_amount > 0 {
                Some(convert_fee(
                    env,
                    &sub.merchant,
                    &sub.token,
                    fee_amount,
                    None,
                ))
            } else {
                None
            };
            let should_emit_fee_event = if fee_amount > 0 {
                if let Some(ref treasury) = treasury_opt {
                    let conv = conversion.as_ref().unwrap();
                    let fee_token = conv.target_token.clone();
                    let fee_credit_amount = conv.effective_amount;
                    if let Some(ref ft) = fee_token {
                        crate::merchant::credit_merchant_balance_for_token(
                            env,
                            treasury,
                            ft,
                            fee_credit_amount,
                            BillingChargeKind::Interval,
                        )?;
                    } else {
                        crate::merchant::credit_merchant_balance_for_token(
                            env,
                            treasury,
                            &sub.token,
                            fee_credit_amount,
                            BillingChargeKind::Interval,
                        )?;
                    }
                    Some((treasury.clone(), fee_amount))
                } else {
                    None
                }
            } else {
                None
            };
            sub.last_payment_timestamp = now.max(sub.last_payment_timestamp);

            sub.lifetime_charged = safe_add(sub.lifetime_charged, charge_amount)?;

            // Recover from grace period or insufficient balance on successful charge.
            // Clear the grace clock so the next charge window uses fresh timestamps.
            if sub.status == SubscriptionStatus::GracePeriod
                || sub.status == SubscriptionStatus::InsufficientBalance
            {
                transition_to(&mut sub.status, SubscriptionStatus::Active)?;
                sub.grace_start_timestamp = None;
            }

            // Reset consecutive failure counter on any successful charge.
            env.storage()
                .instance()
                .remove(&DataKey::ChargeFailureCounter(subscription_id));

            // Check if cap is now exactly reached -- auto-cancel
            let cap_reached = sub
                .lifetime_cap
                .map(|cap| crate::subscription::lifetime_cap_reached(cap, sub.lifetime_charged))
                .unwrap_or(false);

            if cap_reached {
                transition_to(&mut sub.status, SubscriptionStatus::Cancelled)?;
            }

            write_subscription(env, subscription_id, &sub);

            // Emit protocol fee event after state is written
            if let Some((treasury, fee)) = should_emit_fee_event {
                env.events().publish(
                    (Symbol::new(env, "protocol_fee_charged"), subscription_id),
                    crate::types::ProtocolFeeChargedEvent {
                        subscription_id,
                        merchant: sub.merchant.clone(),
                        token: sub.token.clone(),
                        fee_amount: fee,
                        treasury,
                        timestamp: now,
                        batch_nonce: crate::admin::current_batch_nonce(env),
                        schema_version: crate::types::PROTOCOL_FEE_CHARGED_EVENT_SCHEMA_VERSION,
                        contract_id: env.current_contract_address(),
                    },
                );
                // Emit fee conversion event when fee-token override was applied
                if let Some(conv) = &conversion {
                    if let Some(ref target) = conv.target_token {
                        env.events().publish(
                            (Symbol::new(env, "fee_converted"), subscription_id),
                            FeeConvertedEvent {
                                subscription_id,
                                source_token: sub.token.clone(),
                                target_token: target.clone(),
                                original_fee_amount: fee,
                                converted_fee_amount: conv.effective_amount,
                                rate: conv.rate,
                                timestamp: now,
                                batch_nonce: crate::admin::current_batch_nonce(env),
                                schema_version: crate::types::FEE_CONVERTED_EVENT_SCHEMA_VERSION,
                                contract_id: env.current_contract_address(),
                            },
                        );
                    }
                }
            }

            append_statement(
                env,
                subscription_id,
                charge_amount,
                sub.merchant.clone(),
                sub.token.clone(),
                BillingChargeKind::Interval,
                next_allowed.saturating_sub(sub.interval_seconds),
                now,
            )?;

            crate::period_snapshots::write_period_snapshot(
                env,
                BillingPeriodSnapshot {
                    subscription_id,
                    period_index,
                    period_start: next_allowed.saturating_sub(sub.interval_seconds),
                    period_end: now,
                    total_charged: charge_amount,
                    total_usage_units: 0,
                    status_flags: SNAPSHOT_FLAG_CLOSED | SNAPSHOT_FLAG_INTERVAL_CHARGED,
                    finalized_at: now,
                },
            )?;

            // Record charged period and optional idempotency key
            storage.set(&DataKey::ChargedPeriod(subscription_id), &period_index);
            storage.set(&salt_key, &salt);
            if let Some(k) = idempotency_key {
                let hashed = crate::idempotency::hash_idem_key(
                    env,
                    crate::nonce::DOMAIN_CHARGE_INTERVAL.as_u32(),
                    subscription_id,
                    &k,
                );
                crate::idempotency::push_key(env, subscription_id, &hashed, now)?;
            }

            env.events().publish(
                (TOPIC_CHARGED,),
                SubscriptionChargedEvent {
                    subscription_id,
                    subscriber: sub.subscriber.clone(),
                    merchant: sub.merchant.clone(),
                    token: sub.token.clone(),
                    amount: merchant_amount,
                    lifetime_charged: sub.lifetime_charged,
                    timestamp: now,
                    period_start,
                    period_end,
                    salt: salt.clone(),
                    batch_nonce: crate::admin::current_batch_nonce(env),
                    schema_version: crate::types::SUBSCRIPTION_CHARGED_EVENT_SCHEMA_VERSION,
                    contract_id: env.current_contract_address(),
                    prepaid_balance: sub.prepaid_balance,
                },
            );

            if cap_reached {
                if let Some(cap) = sub.lifetime_cap {
                    env.events().publish(
                        (Symbol::new(env, "lifetime_cap_reached"), subscription_id),
                        LifetimeCapReachedEvent {
                            subscription_id,
                            lifetime_cap: cap,
                            lifetime_charged: sub.lifetime_charged,
                            timestamp: now,
                            batch_nonce: crate::admin::current_batch_nonce(env),
                            schema_version: crate::types::LIFETIME_CAP_REACHED_EVENT_SCHEMA_VERSION,
                            contract_id: env.current_contract_address(),
                        },
                    );
                }
            }

            Ok(ChargeExecutionResult::Charged)
        }
        Err(_) => {
            let grace_duration = if let Some(cfg) = admin_config {
                cfg.grace_duration
            } else {
                crate::admin::get_grace_period(env)?
            };
            let previous_status = sub.status;

            if sub.status == SubscriptionStatus::GracePeriod {
                // Already in grace — check whether the window has expired since
                // the clock first started.  Keep the original grace_start_timestamp
                // so a single deposit within the window restores Active.
                if let Some(grace_start) = sub.grace_start_timestamp {
                    let grace_expires = grace_start.saturating_add(grace_duration);
                    if grace_duration == 0 || now >= grace_expires {
                        // Grace window closed — move to InsufficientBalance
                        transition_to(&mut sub.status, SubscriptionStatus::InsufficientBalance)?;
                    }
                    // else: stay in GracePeriod, keep clock unchanged
                } else {
                    // Sanity: GracePeriod status without a timestamp — treat as
                    // fresh entry so the clock is always initialised.
                    sub.grace_start_timestamp = Some(now);
                }
            } else if grace_duration > 0 {
                // First underfunded charge — enter GracePeriod and start the clock
                transition_to(&mut sub.status, SubscriptionStatus::GracePeriod)?;
                sub.grace_start_timestamp = Some(now);
            } else {
                // No grace period configured — go straight to InsufficientBalance
                transition_to(&mut sub.status, SubscriptionStatus::InsufficientBalance)?;
                sub.grace_start_timestamp = None;
            }

            write_subscription(env, subscription_id, &sub);

            // Emit grace_period_entered event after state is written
            if grace_duration > 0 && previous_status != SubscriptionStatus::GracePeriod {
                let grace_expires_at = now.saturating_add(grace_duration);
                env.events().publish(
                    (Symbol::new(env, "grace_period_entered"), subscription_id),
                    GracePeriodEnteredEvent {
                        subscription_id,
                        previous_status,
                        grace_expires_at,
                        timestamp: now,
                        batch_nonce: crate::admin::current_batch_nonce(env),
                        schema_version: crate::types::GRACE_PERIOD_ENTERED_EVENT_SCHEMA_VERSION,
                        contract_id: env.current_contract_address(),
                    },
                );
            }

            let shortfall = charge_amount.saturating_sub(sub.prepaid_balance).max(0);
            env.events().publish(
                (Symbol::new(env, "charge_failed"), subscription_id),
                SubscriptionChargeFailedEvent {
                    subscription_id,
                    merchant: sub.merchant,
                    required_amount: charge_amount,
                    available_balance: sub.prepaid_balance,
                    shortfall,
                    resulting_status: sub.status,
                    timestamp: now,
                    batch_nonce: crate::admin::current_batch_nonce(env),
                    schema_version: crate::types::SUBSCRIPTION_CHARGE_FAILED_EVENT_SCHEMA_VERSION,
                    contract_id: env.current_contract_address(),
                },
            );

            // Increment consecutive failure counter and auto-pause if threshold reached.
            let threshold = if let Some(cfg) = admin_config {
                cfg.auto_pause_threshold
            } else {
                crate::admin::get_auto_pause_threshold(env)
            };
            if threshold > 0 && sub.status == SubscriptionStatus::InsufficientBalance {
                let counter_key = DataKey::ChargeFailureCounter(subscription_id);
                let failures: u32 = env
                    .storage()
                    .instance()
                    .get(&counter_key)
                    .unwrap_or(0u32)
                    .saturating_add(1);
                env.storage().instance().set(&counter_key, &failures);

                if failures >= threshold {
                    // Re-load subscription to apply the Paused transition cleanly.
                    let mut sub2 = crate::queries::get_subscription(env, subscription_id).unwrap();
                    if sub2.status == SubscriptionStatus::InsufficientBalance {
                        if transition_to(&mut sub2.status, SubscriptionStatus::Paused).is_ok() {
                            crate::subscription::write_subscription(env, subscription_id, &sub2);
                            env.storage().instance().remove(&counter_key);
                            env.events().publish(
                                (Symbol::new(env, "sub_auto_paused"), subscription_id),
                                SubscriptionAutoPausedEvent {
                                    subscription_id,
                                    consecutive_failures: failures,
                                    threshold,
                                    timestamp: now,
                                    batch_nonce: crate::admin::current_batch_nonce(env),
                                    schema_version: crate::types::SUBSCRIPTION_AUTO_PAUSED_EVENT_SCHEMA_VERSION,
                                    contract_id: env.current_contract_address(),
                                },
                            );
                        }
                    }
                }
            }

            Ok(ChargeExecutionResult::InsufficientBalance)
        }
    }
}

/// Debit a metered `usage_amount` from a subscription's prepaid balance.
pub fn charge_usage_one(
    env: &Env,
    subscription_id: u32,
    usage_amount: i128,
    reference: String,
) -> Result<UsageChargeResult, Error> {
    // ── CRITICAL: Atomic emergency stop check ────────────────────────────────
    // Re-check emergency stop on every iteration to prevent in-flight charging
    // operations from completing when emergency_stop is triggered mid-execution.
    let now = env.ledger().timestamp();
    if crate::admin::read_config(env, &crate::types::DataKey::EmergencyStop).unwrap_or(false) {
        return Err(charge_fail(
            env,
            subscription_id,
            crate::types::Error::EmergencyStopActive,
            0,
            now,
        ));
    }

    let mut sub = get_subscription(env, subscription_id)
        .map_err(|e| charge_fail(env, subscription_id, e, 0, env.ledger().timestamp()))?;
    let merchant = sub.merchant.clone();

    if crate::merchant::get_merchant_paused(env, merchant.clone()) {
        return Err(charge_fail(
            env,
            subscription_id,
            Error::MerchantPaused,
            0,
            env.ledger().timestamp(),
        ));
    }

    // Merchant vacation guard — block charges during vacation window
    let now = env.ledger().timestamp();
    if crate::merchant::is_merchant_in_vacation(env, &merchant, now) {
        return Err(charge_fail(
            env,
            subscription_id,
            Error::VacationActive,
            0,
            now,
        ));
    }

    crate::blocklist::require_not_blocklisted(env, &sub.subscriber)
        .map_err(|e| charge_fail(env, subscription_id, e, 0, env.ledger().timestamp()))?;
    crate::blocklist::require_not_blocklisted(env, &sub.merchant)
        .map_err(|e| charge_fail(env, subscription_id, e, 0, env.ledger().timestamp()))?;

    if let Some(split_payees) = crate::subscription::get_split_payees(env, subscription_id) {
        for entry in split_payees.entries.iter() {
            let (payee, _) = entry;
            crate::blocklist::require_not_blocklisted(env, &payee)
                .map_err(|e| charge_fail(env, subscription_id, e, 0, env.ledger().timestamp()))?;
            if crate::merchant::get_merchant_paused(env, payee.clone()) {
                return Err(charge_fail(
                    env,
                    subscription_id,
                    Error::MerchantPaused,
                    0,
                    env.ledger().timestamp(),
                ));
            }
            if crate::merchant::is_merchant_in_vacation(env, &payee, now) {
                return Err(charge_fail(
                    env,
                    subscription_id,
                    Error::VacationActive,
                    0,
                    now,
                ));
            }
        }
    }

    // Expiration guard
    if sub.is_expired(now, env.ledger().sequence()) {
        if sub.status != SubscriptionStatus::Expired {
            transition_to(&mut sub.status, SubscriptionStatus::Expired)?;
            write_subscription(env, subscription_id, &sub);
            env.events().publish(
                (Symbol::new(env, "subscription_expired"), subscription_id),
                crate::types::SubscriptionExpiredEvent {
                    subscription_id,
                    timestamp: now,
                    batch_nonce: crate::admin::current_batch_nonce(env),
                    schema_version: crate::types::SUBSCRIPTION_EXPIRED_EVENT_SCHEMA_VERSION,
                    contract_id: env.current_contract_address(),
                },
            );
        }
        return Err(charge_fail(
            env,
            subscription_id,
            Error::SubscriptionExpired,
            0,
            now,
        ));
    }

    if let Some(cap) = sub.lifetime_cap {
        if crate::subscription::lifetime_cap_reached(cap, sub.lifetime_charged) {
            if sub.status != SubscriptionStatus::Cancelled {
                transition_to(&mut sub.status, SubscriptionStatus::Cancelled)?;
                write_subscription(env, subscription_id, &sub);
                env.events().publish(
                    (Symbol::new(env, "lifetime_cap_reached"), subscription_id),
                    LifetimeCapReachedEvent {
                        subscription_id,
                        lifetime_cap: cap,
                        lifetime_charged: sub.lifetime_charged,
                        timestamp: now,
                        batch_nonce: crate::admin::current_batch_nonce(env),
                        schema_version: crate::types::LIFETIME_CAP_REACHED_EVENT_SCHEMA_VERSION,
                        contract_id: env.current_contract_address(),
                    },
                );
            }
            return Err(charge_fail(
                env,
                subscription_id,
                Error::LifetimeCapReached,
                usage_amount,
                now,
            ));
        }
    }

    if sub.status != SubscriptionStatus::Active {
        return Err(charge_fail(
            env,
            subscription_id,
            Error::NotActive,
            usage_amount,
            now,
        ));
    }

    if !sub.usage_enabled {
        return Err(charge_fail(
            env,
            subscription_id,
            Error::UsageNotEnabled,
            usage_amount,
            now,
        ));
    }

    if !sub.auto_renew {
        let next_allowed = next_charge_time(sub.last_payment_timestamp, sub.interval_seconds)?;
        if now >= next_allowed {
            return Err(charge_fail(
                env,
                subscription_id,
                Error::NotActive,
                usage_amount,
                now,
            ));
        }
    }

    if usage_amount <= 0 {
        return Err(charge_fail(
            env,
            subscription_id,
            Error::InvalidAmount,
            usage_amount,
            now,
        ));
    }

    let usage_cap_override: bool = env
        .storage()
        .instance()
        .get(&DataKey::UsageChargeCapOverride(merchant.clone()))
        .unwrap_or(false);
    if !usage_cap_override && usage_amount > sub.amount {
        env.events().publish(
            (Symbol::new(env, "usage_charge_rejected"), subscription_id),
            UsageChargeRejectedEvent {
                subscription_id,
                merchant: sub.merchant.clone(),
                token: sub.token.clone(),
                usage_amount,
                timestamp: now,
                reference,
                result: UsageChargeResult::UsageCapExceeded,
                schema_version: crate::types::BASE_EVENT_SCHEMA_VERSION,
                contract_id: env.current_contract_address(),
            },
        );
        return Ok(UsageChargeResult::UsageCapExceeded);
    }

    if sub.prepaid_balance < usage_amount {
        return Err(charge_fail(
            env,
            subscription_id,
            Error::InsufficientPrepaidBalance,
            usage_amount,
            now,
        ));
    }

    // -- Replay protection (Reference-based) ----------------------------------
    // We use the reference as a unique idempotency key for usage charges.
    // If the reference has been seen before for this subscription, we return Replay.
    let ref_key = (
        Symbol::new(env, "usage_ref"),
        subscription_id,
        reference.clone(),
    );

    if env.storage().instance().has(&ref_key) {
        env.events().publish(
            (Symbol::new(env, "usage_charge_rejected"), subscription_id),
            UsageChargeRejectedEvent {
                subscription_id,
                merchant: sub.merchant.clone(),
                token: sub.token.clone(),
                usage_amount,
                timestamp: now,
                reference,
                result: UsageChargeResult::Replay,
                schema_version: crate::types::BASE_EVENT_SCHEMA_VERSION,
                contract_id: env.current_contract_address(),
            },
        );
        return Ok(UsageChargeResult::Replay);
    }

    // -- Usage Limits & State -------------------------------------------------
    let now = env.ledger().timestamp();
    let limits_key = DataKey::UsageLimits(subscription_id);
    let maybe_limits: Option<UsageLimits> = env.storage().instance().get(&limits_key);

    if let Some(limits) = maybe_limits {
        let state_key = DataKey::UsageState(subscription_id);
        let mut state = env
            .storage()
            .instance()
            .get(&state_key)
            .unwrap_or(UsageState {
                last_usage_timestamp: 0,
                window_start_timestamp: now,
                window_call_count: 0,
                current_period_usage_units: 0,
                period_index: now.saturating_sub(sub.start_time) / sub.interval_seconds,
            });

        // A zero interval explicitly disables burst protection.
        if limits.burst_min_interval_secs > 0 {
            let elapsed = now.saturating_sub(state.last_usage_timestamp);
            if elapsed < limits.burst_min_interval_secs {
                env.events().publish(
                    (Symbol::new(env, "usage_charge_rejected"), subscription_id),
                    UsageChargeRejectedEvent {
                        subscription_id,
                        merchant: sub.merchant.clone(),
                        token: sub.token.clone(),
                        usage_amount,
                        timestamp: now,
                        reference,
                        result: UsageChargeResult::BurstLimitExceeded,
                        schema_version: crate::types::BASE_EVENT_SCHEMA_VERSION,
                        contract_id: env.current_contract_address(),
                    },
                );
                return Ok(UsageChargeResult::BurstLimitExceeded);
            }
        }

        // 2. Rate limit (sliding window approximate)
        if let Some(max_calls) = limits.rate_limit_max_calls {
            if now
                >= state
                    .window_start_timestamp
                    .saturating_add(limits.rate_window_secs)
            {
                state.window_start_timestamp = now;
                state.window_call_count = 0;
            }
            if state.window_call_count >= max_calls {
                env.events().publish(
                    (Symbol::new(env, "usage_charge_rejected"), subscription_id),
                    UsageChargeRejectedEvent {
                        subscription_id,
                        merchant: sub.merchant.clone(),
                        token: sub.token.clone(),
                        usage_amount,
                        timestamp: now,
                        reference,
                        result: UsageChargeResult::RateLimitExceeded,
                        schema_version: crate::types::BASE_EVENT_SCHEMA_VERSION,
                        contract_id: env.current_contract_address(),
                    },
                );
                return Ok(UsageChargeResult::RateLimitExceeded);
            }
        }

        // 3. Usage cap (per-interval)
        if let Some(cap_units) = limits.usage_cap_units {
            let current_period = now.saturating_sub(sub.start_time) / sub.interval_seconds;
            if current_period > state.period_index {
                let previous_period_index = state.period_index;
                let previous_period_usage_units = state.current_period_usage_units;
                state.period_index = current_period;
                state.current_period_usage_units = 0;
                env.storage().instance().set(&state_key, &state);
                env.events().publish(
                    (Symbol::new(env, "usage_cap_reset"), subscription_id),
                    UsageCapResetEvent {
                        subscription_id,
                        merchant: sub.merchant.clone(),
                        token: sub.token.clone(),
                        previous_period_index,
                        current_period_index: current_period,
                        previous_period_usage_units,
                        usage_cap_units: cap_units,
                        timestamp: now,
                        schema_version: crate::types::BASE_EVENT_SCHEMA_VERSION,
                        contract_id: env.current_contract_address(),
                    },
                );
            }
            if state
                .current_period_usage_units
                .saturating_add(usage_amount)
                > cap_units
            {
                env.events().publish(
                    (Symbol::new(env, "usage_charge_rejected"), subscription_id),
                    UsageChargeRejectedEvent {
                        subscription_id,
                        merchant: sub.merchant.clone(),
                        token: sub.token.clone(),
                        usage_amount,
                        timestamp: now,
                        reference,
                        result: UsageChargeResult::UsageCapExceeded,
                        schema_version: crate::types::BASE_EVENT_SCHEMA_VERSION,
                        contract_id: env.current_contract_address(),
                    },
                );
                return Ok(UsageChargeResult::UsageCapExceeded);
            }
        }

        // Update state
        state.last_usage_timestamp = now;
        state.window_call_count = state.window_call_count.saturating_add(1);
        state.current_period_usage_units = state
            .current_period_usage_units
            .saturating_add(usage_amount);
        env.storage().instance().set(&state_key, &state);
    }

    // -- Lifetime cap pre-check -----------------------------------------------
    // Over-cap attempts are blocked and cancel the subscription without debiting funds.
    let pending_lifetime = safe_add(sub.lifetime_charged, usage_amount)?;
    if let Some(cap) = sub.lifetime_cap {
        if pending_lifetime > cap {
            transition_to(&mut sub.status, SubscriptionStatus::Cancelled)?;
            write_subscription(env, subscription_id, &sub);
            env.events().publish(
                (Symbol::new(env, "lifetime_cap_reached"), subscription_id),
                LifetimeCapReachedEvent {
                    subscription_id,
                    lifetime_cap: cap,
                    lifetime_charged: sub.lifetime_charged,
                    timestamp: now,
                    batch_nonce: crate::admin::current_batch_nonce(env),
                    schema_version: crate::types::LIFETIME_CAP_REACHED_EVENT_SCHEMA_VERSION,
                    contract_id: env.current_contract_address(),
                },
            );
            return Ok(UsageChargeResult::Charged);
        }
    }

    match crate::safe_math::safe_sub_balance(sub.prepaid_balance, usage_amount) {
        Ok(new_balance) => {
            sub.prepaid_balance = new_balance;
            let fee_bps = route_fee_bps(env, &sub.merchant);
            let treasury_opt = crate::admin::get_treasury(env);
            let (merchant_amount, fee_amount) = if fee_bps > 0 {
                if let Some(ref _t) = treasury_opt {
                    // Minimum-fee floor: see charge_one for rationale.
                    let fee = (usage_amount * fee_bps as i128 / 10_000i128).max(1);
                    let net = safe_sub(usage_amount, fee)?;
                    (net, fee)
                } else {
                    (usage_amount, 0i128)
                }
            } else {
                (usage_amount, 0i128)
            };
            credit_charge_payees(
                env,
                subscription_id,
                &sub,
                merchant_amount,
                BillingChargeKind::Usage,
            )?;

            // Route merchant amount to sub-account if subscription has one
            if let Some(ref label) = sub.sub_account_label {
                crate::merchant::credit_sub_account(
                    env,
                    &sub.merchant,
                    label,
                    &sub.token,
                    merchant_amount,
                )?;
                // Deduct from parent balance (parent earnings stay for roll-up reporting)
                let parent_bal =
                    crate::merchant::get_merchant_balance_by_token(env, &sub.merchant, &sub.token);
                let new_parent_bal = crate::safe_math::safe_sub(parent_bal, merchant_amount)?;
                crate::merchant::set_merchant_balance(
                    env,
                    &sub.merchant,
                    &sub.token,
                    &new_parent_bal,
                );
            }

            let conversion = if fee_amount > 0 {
                Some(convert_fee(
                    env,
                    &sub.merchant,
                    &sub.token,
                    fee_amount,
                    None,
                ))
            } else {
                None
            };
            let should_emit_fee_event = if fee_amount > 0 {
                if let Some(ref treasury) = treasury_opt {
                    let conv = conversion.as_ref().unwrap();
                    let fee_token = conv.target_token.clone();
                    let fee_credit_amount = conv.effective_amount;
                    if let Some(ref ft) = fee_token {
                        crate::merchant::credit_merchant_balance_for_token(
                            env,
                            treasury,
                            ft,
                            fee_credit_amount,
                            BillingChargeKind::Usage,
                        )?;
                    } else {
                        crate::merchant::credit_merchant_balance_for_token(
                            env,
                            treasury,
                            &sub.token,
                            fee_credit_amount,
                            BillingChargeKind::Usage,
                        )?;
                    }
                    Some((treasury.clone(), fee_amount))
                } else {
                    None
                }
            } else {
                None
            };

            sub.lifetime_charged = pending_lifetime;
            let cap_reached = sub
                .lifetime_cap
                .map(|cap| crate::subscription::lifetime_cap_reached(cap, sub.lifetime_charged))
                .unwrap_or(false);

            if cap_reached {
                transition_to(&mut sub.status, SubscriptionStatus::Cancelled)?;
            } else if new_balance == 0 {
                // Without a cap hit, zero remaining prepaid means underfunded for future usage.
                transition_to(&mut sub.status, SubscriptionStatus::InsufficientBalance)?;
            }

            write_subscription(env, subscription_id, &sub);

            // Emit protocol fee event after state is written
            if let Some((treasury, fee)) = should_emit_fee_event {
                env.events().publish(
                    (Symbol::new(env, "protocol_fee_charged"), subscription_id),
                    crate::types::ProtocolFeeChargedEvent {
                        subscription_id,
                        merchant: sub.merchant.clone(),
                        token: sub.token.clone(),
                        fee_amount: fee,
                        treasury,
                        timestamp: now,
                        batch_nonce: crate::admin::current_batch_nonce(env),
                        schema_version: crate::types::PROTOCOL_FEE_CHARGED_EVENT_SCHEMA_VERSION,
                        contract_id: env.current_contract_address(),
                    },
                );
                // Emit fee conversion event when fee-token override was applied
                if let Some(conv) = &conversion {
                    if let Some(ref target) = conv.target_token {
                        env.events().publish(
                            (Symbol::new(env, "fee_converted"), subscription_id),
                            FeeConvertedEvent {
                                subscription_id,
                                source_token: sub.token.clone(),
                                target_token: target.clone(),
                                original_fee_amount: fee,
                                converted_fee_amount: conv.effective_amount,
                                rate: conv.rate,
                                timestamp: now,
                                batch_nonce: crate::admin::current_batch_nonce(env),
                                schema_version: crate::types::FEE_CONVERTED_EVENT_SCHEMA_VERSION,
                                contract_id: env.current_contract_address(),
                            },
                        );
                    }
                }
            }

            env.storage().instance().set(&ref_key, &true); // Mark reference as used

            let period_index = now.saturating_sub(sub.start_time) / sub.interval_seconds;
            let period_start = sub
                .start_time
                .checked_add(
                    period_index
                        .checked_mul(sub.interval_seconds)
                        .ok_or(Error::Overflow)?,
                )
                .ok_or(Error::Overflow)?;

            crate::period_snapshots::write_period_snapshot(
                env,
                BillingPeriodSnapshot {
                    subscription_id,
                    period_index,
                    period_start,
                    period_end: now,
                    total_charged: usage_amount,
                    total_usage_units: usage_amount,
                    status_flags: SNAPSHOT_FLAG_USAGE_CHARGED,
                    finalized_at: now,
                },
            )?;

            append_statement(
                env,
                subscription_id,
                usage_amount,
                sub.merchant.clone(),
                sub.token.clone(),
                BillingChargeKind::Usage,
                now,
                now,
            )?;

            env.events().publish(
                (Symbol::new(env, "usage_charged"), subscription_id),
                UsageStatementEvent {
                    subscription_id,
                    merchant: sub.merchant.clone(),
                    usage_amount,
                    token: sub.token.clone(),
                    timestamp: now,
                    reference,
                    schema_version: crate::types::BASE_EVENT_SCHEMA_VERSION,
                    contract_id: env.current_contract_address(),
                },
            );

            if cap_reached {
                if let Some(cap) = sub.lifetime_cap {
                    env.events().publish(
                        (Symbol::new(env, "lifetime_cap_reached"), subscription_id),
                        LifetimeCapReachedEvent {
                            subscription_id,
                            lifetime_cap: cap,
                            lifetime_charged: sub.lifetime_charged,
                            timestamp: now,
                            batch_nonce: crate::admin::current_batch_nonce(env),
                            schema_version: crate::types::LIFETIME_CAP_REACHED_EVENT_SCHEMA_VERSION,
                            contract_id: env.current_contract_address(),
                        },
                    );
                }
            }
            Ok(UsageChargeResult::Charged)
        }
        Err(_) => {
            transition_to(&mut sub.status, SubscriptionStatus::InsufficientBalance)?;
            write_subscription(env, subscription_id, &sub);

            env.events().publish(
                (Symbol::new(env, "charge_failed"), subscription_id),
                SubscriptionChargeFailedEvent {
                    subscription_id,
                    merchant: sub.merchant,
                    required_amount: usage_amount,
                    available_balance: sub.prepaid_balance,
                    shortfall: usage_amount.saturating_sub(sub.prepaid_balance),
                    resulting_status: SubscriptionStatus::InsufficientBalance,
                    timestamp: now,
                    batch_nonce: crate::admin::current_batch_nonce(env),
                    schema_version: crate::types::SUBSCRIPTION_CHARGE_FAILED_EVENT_SCHEMA_VERSION,
                    contract_id: env.current_contract_address(),
                },
            );
            Ok(UsageChargeResult::Charged)
        }
    }
}

// Distribute `net_merchant_amount` among configured split payees.
//
// Rounding rule: per-payee shares are computed using integer division
// (`share = net * weight / 10000`). To ensure the total distributed
// amount equals `net_merchant_amount` exactly, any remainder from the
// per-payee truncation is allocated to the first payee (index 0).
// This deterministic allocation prevents dust from accumulating in the
// vault and makes accounting auditable.
pub(crate) fn credit_charge_payees(
    env: &Env,
    subscription_id: u32,
    sub: &crate::types::Subscription,
    net_merchant_amount: i128,
    charge_kind: crate::types::BillingChargeKind,
) -> Result<(), Error> {
    if let Some(split_payees) = crate::subscription::get_split_payees(env, subscription_id) {
        let mut total_distributed_amount = 0i128;
        let num_payees = split_payees.entries.len();

        for i in 1..num_payees {
            if let Some(entry) = split_payees.entries.get(i) {
                let (payee, weight) = entry;
                let share = net_merchant_amount * weight as i128 / 10_000i128;
                total_distributed_amount =
                    crate::safe_math::safe_add(total_distributed_amount, share)?;
                crate::merchant::credit_merchant_balance_for_token(
                    env,
                    &payee,
                    &sub.token,
                    share,
                    charge_kind,
                )?;
            }
        }

        if let Some(entry) = split_payees.entries.get(0) {
            let (payee, _) = entry;
            let first_share = net_merchant_amount - total_distributed_amount;
            crate::merchant::credit_merchant_balance_for_token(
                env,
                &payee,
                &sub.token,
                first_share,
                charge_kind,
            )?;
        }

        let mut payees_vec = soroban_sdk::Vec::new(env);
        for i in 0..num_payees {
            if let Some(entry) = split_payees.entries.get(i) {
                let (payee, weight) = entry;
                let share = if i == 0 {
                    net_merchant_amount - total_distributed_amount
                } else {
                    net_merchant_amount * weight as i128 / 10_000i128
                };
                payees_vec.push_back((payee, share));
            }
        }

        env.events().publish(
            (
                soroban_sdk::Symbol::new(env, "split_charge"),
                subscription_id,
            ),
            crate::types::SplitChargeEvent {
                subscription_id,
                payees: payees_vec,
                timestamp: env.ledger().timestamp(),
                batch_nonce: crate::admin::current_batch_nonce(env),
                schema_version: crate::types::SPLIT_CHARGE_EVENT_SCHEMA_VERSION,
                contract_id: env.current_contract_address(),
            },
        );
    } else {
        crate::merchant::credit_merchant_balance_for_token(
            env,
            &sub.merchant,
            &sub.token,
            net_merchant_amount,
            charge_kind,
        )?;
    }
    Ok(())
}

/// Calculates the prorated charge amount for a partial first billing period.
///
/// # Arguments
/// * `amount` - The full charge amount for one complete interval (must be non-negative)
/// * `interval` - The complete billing interval in seconds (must be > 0)
/// * `remaining_seconds` - Seconds remaining in the current partial interval (0..=u64::MAX)
///
/// # Returns
/// * `Ok(prorated_amount)` - The charge amount scaled proportionally to elapsed time
/// * `Err(Error::InvalidAmount)` - If amount is negative
/// * `Err(Error::InvalidInput)` - If interval is 0
///
/// # Formula
/// `prorated_amount = (amount * remaining_seconds) / interval`
///
/// # Invariants
/// - `0 <= prorated_amount <= amount`
/// - Monotonic: if `rem_low <= rem_high`, then `charge(rem_low) <= charge(rem_high)`
/// - If `remaining_seconds >= interval`, returns `amount` (capped at full amount)
/// - If `remaining_seconds == 0`, returns `0`
pub fn calculate_prorated_first_charge(
    amount: i128,
    interval: u64,
    remaining_seconds: u64,
) -> Result<i128, Error> {
    // Validate inputs
    if amount < 0 {
        return Err(Error::InvalidAmount);
    }

    if interval == 0 {
        return Err(Error::InvalidInput);
    }

    // Handle edge case: if remaining_seconds is 0, no charge
    if remaining_seconds == 0 {
        return Ok(0);
    }

    // Cap remaining_seconds to interval to avoid overflow
    // If remaining >= interval, we charge the full amount
    if remaining_seconds >= interval {
        return Ok(amount);
    }

    // Split amount into quotient and remainder so neither intermediate product
    // scales with the full i128 amount:
    // (amount * remaining) / interval
    // = (amount / interval) * remaining
    //   + ((amount % interval) * remaining) / interval.
    let interval_i128 = interval as i128;
    let remaining_i128 = remaining_seconds as i128;
    let whole_product = (amount / interval_i128)
        .checked_mul(remaining_i128)
        .ok_or(Error::Overflow)?;

    // Both factors are less than interval (a u64), so this product fits u128.
    let fractional_product = ((amount % interval_i128) as u128)
        .checked_mul(remaining_seconds as u128)
        .ok_or(Error::Overflow)?;
    let fractional = (fractional_product / interval as u128) as i128;

    let prorated = whole_product
        .checked_add(fractional)
        .ok_or(Error::Overflow)?;
    Ok(prorated.min(amount))
}
