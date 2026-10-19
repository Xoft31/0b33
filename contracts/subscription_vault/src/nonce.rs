//! Nonce: replay-protection counters for privileged operations.
//!
//! This module implements persistent, domain-separated monotonic nonce counters
//! that prevent replay attacks on sensitive operations like `batch_charge` and
//! `rotate_admin`. Each `(signer, domain)` pair maintains an independent counter
//! stored in persistent ledger storage, ensuring correctness across contract upgrades
//! and ledger TTL extensions.
//!
//! # Design
//!
//! - **Monotonic**: Nonces increment by exactly 1 on each successful consumption.
//! - **Domain-separated**: Each operation type (batch_charge, rotate_admin, operator_batch_charge)
//!   uses a distinct domain constant to prevent cross-domain replay.
//! - **Per-signer**: Each caller maintains its own independent counter.
//! - **Persistent**: Stored in ledger persistent storage, surviving upgrades.
//! - **Bounded storage**: Exactly one `u64` per `(signer, domain)` pair.
//!
//! # Security
//!
//! - Auth check (`require_admin_auth`) runs **before** nonce check to reject invalid signers early.
//! - Nonce overflow is prevented by Rust's checked arithmetic (panics rather than wraps).
//! - Cross-domain collision is impossible: domain is part of the storage key.
//! - Out-of-order submission is rejected: only the exact stored value is accepted.

use soroban_sdk::{Address, Env};
use crate::types::{DataKey, Error, NonceConsumedEvent};

/// Type-enforced nonce domain. Replaces bare `u32` domain constants so a call
/// site cannot silently pass the wrong domain (or a value from an unrelated
/// constant set) without a compile error — the wrong domain constant would
/// still be a valid `u32`, but it cannot be a valid `NonceDomain` variant
/// belonging to a different operation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum NonceDomain {
    BatchCharge = 0,
    AdminRotation = 1,
    OperatorBatchCharge = 2,
    MetadataSigned = 3,
    MerchantRotation = 4,
    ChargeInterval = 5,
    DepositFunds = 6,
    ChargeOneoff = 7,
    SubscriberWithdrawal = 8,
    ChargebackDispute = 9,
    GovernanceVote = 10,
}

impl NonceDomain {
    pub fn as_u32(self) -> u32 {
        self as u32
    }
}

impl TryFrom<u32> for NonceDomain {
    type Error = ();

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(NonceDomain::BatchCharge),
            1 => Ok(NonceDomain::AdminRotation),
            2 => Ok(NonceDomain::OperatorBatchCharge),
            3 => Ok(NonceDomain::MetadataSigned),
            4 => Ok(NonceDomain::MerchantRotation),
            5 => Ok(NonceDomain::ChargeInterval),
            6 => Ok(NonceDomain::DepositFunds),
            7 => Ok(NonceDomain::ChargeOneoff),
            8 => Ok(NonceDomain::SubscriberWithdrawal),
            9 => Ok(NonceDomain::ChargebackDispute),
            10 => Ok(NonceDomain::GovernanceVote),
            _ => Err(()),
        }
    }
}

/// Domain constant for batch charge operations.
/// Prevents replay of batch_charge nonces into rotate_admin and vice versa.
pub const DOMAIN_BATCH_CHARGE: NonceDomain = NonceDomain::BatchCharge;

/// Domain constant for admin rotation operations.
pub const DOMAIN_ADMIN_ROTATION: NonceDomain = NonceDomain::AdminRotation;

/// Domain constant for operator batch charge operations.
pub const DOMAIN_OPERATOR_BATCH_CHARGE: NonceDomain = NonceDomain::OperatorBatchCharge;

/// Domain constant for off-chain signed metadata updates (`set_metadata_signed`).
///
/// Keeps signed-metadata-update nonces separated from privileged batch,
/// rotation, and operator nonces, so a captured signed metadata payload cannot
/// be replayed into a higher-privilege domain. Auth check (signer must be
/// subscriber or merchant) runs **before** the nonce check.
pub const DOMAIN_METADATA_SIGNED: NonceDomain = NonceDomain::MetadataSigned;

/// Domain constant for merchant address rotation operations.
///
/// Keeps `rotate_merchant_address` nonces separated from every other domain so
/// a captured rotation payload cannot be replayed against a different
/// privileged operation for the same admin signer.
pub const DOMAIN_MERCHANT_ROTATION: NonceDomain = NonceDomain::MerchantRotation;

pub const DOMAIN_SUBSCRIBER_WITHDRAWAL: NonceDomain = NonceDomain::SubscriberWithdrawal;
pub const DOMAIN_CHARGEBACK_DISPUTE: NonceDomain = NonceDomain::ChargebackDispute;

/// Domain constant for governance vote casting operations.
///
/// Prevents a captured signed vote payload from being replayed into a
/// different operation domain. Auth check (caller must be a guardian)
/// runs **before** the nonce check.
pub const DOMAIN_GOVERNANCE_VOTE: NonceDomain = NonceDomain::GovernanceVote;

/// Domain constant for charge_interval operations.
pub const DOMAIN_CHARGE_INTERVAL: NonceDomain = NonceDomain::ChargeInterval;

/// Domain constant for deposit_funds operations.
pub const DOMAIN_DEPOSIT_FUNDS: NonceDomain = NonceDomain::DepositFunds;

/// Domain constant for charge_one_off operations.
pub const DOMAIN_CHARGE_ONEOFF: NonceDomain = NonceDomain::ChargeOneoff;

/// Retrieve the current (next-expected) nonce for a `(signer, domain)` pair.
///
/// Returns `0` when no nonce has been consumed yet for this combination (first call).
///
/// # Arguments
///
/// * `env` — Soroban environment (for storage access).
/// * `signer` — The address consuming nonces in this domain.
/// * `domain` — The operation domain (e.g., `DOMAIN_BATCH_CHARGE`).
///
/// # Returns
///
/// The next expected nonce value (starting at 0).
pub fn get_nonce(env: &Env, signer: &Address, domain: NonceDomain) -> u64 {
    env.storage()
        .persistent()
        .get::<DataKey, u64>(&DataKey::AdminNonce(signer.clone(), domain.as_u32()))
        .unwrap_or(0)
}

/// Validate the expected nonce without consuming it.
pub fn validate_nonce(
    env: &Env,
    signer: &Address,
    domain: NonceDomain,
    expected: u64,
) -> Result<(), Error> {
    let key = DataKey::AdminNonce(signer.clone(), domain.as_u32());
    let stored = env.storage().persistent().get::<DataKey, u64>(&key).unwrap_or(0);
    compute_next_nonce(stored, expected).map(|_| ())
}

/// Consume a nonce, verifying it matches the current expected value and incrementing for the next call.
///
/// This function implements the core replay-protection logic:
/// 1. Reads the stored nonce (default 0 if absent).
/// 2. Asserts `expected == stored`.
/// 3. Increments and persists `stored + 1`.
/// 4. Emits `NonceConsumedEvent` for audit.
///
/// # Arguments
///
/// * `env` — Soroban environment.
/// * `signer` — The address that consumed this nonce (must already be auth'd).
/// * `domain` — The operation domain (DOMAIN_BATCH_CHARGE, etc.).
/// * `expected` — The nonce value caller believes is current. Must equal stored exactly.
///
/// # Errors
///
/// * [`Error::NonceAlreadyUsed`] — `expected != stored`. Nonce has already been consumed,
///   or caller skipped ahead, or is reusing an old nonce.
///
/// # Panics
///
/// Panics if `stored.checked_add(1)` overflows (u64::MAX reached). The transaction aborts
/// rather than wrapping to 0, preventing accidental nonce reuse.
///
/// # Security
///
/// Auth check **must** run before calling this function. Invalid signers are rejected
/// before the nonce counter is touched, preventing auth bypass via nonce manipulation.
/// Pure-logic for advancing a nonce. Extracted to allow formal verification without
/// Soroban environment dependencies.
#[inline(always)]
pub fn compute_next_nonce(stored: u64, expected: u64) -> Result<u64, Error> {
    if expected != stored {
        return Err(Error::NonceAlreadyUsed);
    }

    stored.checked_add(1).ok_or(Error::Overflow)
}

pub fn check_and_advance(
    env: &Env,
    signer: &Address,
    domain: NonceDomain,
    expected: u64,
) -> Result<(), Error> {
    let domain = domain.as_u32();
    let key = DataKey::AdminNonce(signer.clone(), domain);
    let stored = env.storage().persistent().get::<DataKey, u64>(&key).unwrap_or(0);

    let next = compute_next_nonce(stored, expected)?;

    // Persist the incremented nonce before emitting event (effects-before-interactions).
    env.storage().persistent().set(&key, &next);

    // Emit audit event with current timestamp.
    env.events().publish(
        (soroban_sdk::Symbol::new(env, "nonce_consumed"), signer.clone(), domain),
        NonceConsumedEvent {
            signer: signer.clone(),
            domain,
            nonce: stored,
            timestamp: env.ledger().timestamp(),
            batch_nonce: crate::admin::current_batch_nonce(env),
            schema_version: crate::types::NONCE_CONSUMED_EVENT_SCHEMA_VERSION,
            contract_id: env.current_contract_address(),
        },
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::testutils::Address as _;

    /// Mock test to verify constant values are correct.
    #[test]
    fn test_domain_constants() {
        assert_eq!(DOMAIN_BATCH_CHARGE.as_u32(), 0);
        assert_eq!(DOMAIN_ADMIN_ROTATION.as_u32(), 1);
        assert_eq!(DOMAIN_OPERATOR_BATCH_CHARGE.as_u32(), 2);
        assert_eq!(DOMAIN_METADATA_SIGNED.as_u32(), 3);
        assert_eq!(DOMAIN_MERCHANT_ROTATION.as_u32(), 4);
        assert_eq!(DOMAIN_CHARGE_INTERVAL.as_u32(), 5);
        assert_eq!(DOMAIN_DEPOSIT_FUNDS.as_u32(), 6);
        assert_eq!(DOMAIN_CHARGE_ONEOFF.as_u32(), 7);
        assert_eq!(DOMAIN_SUBSCRIBER_WITHDRAWAL.as_u32(), 8);
        assert_eq!(DOMAIN_CHARGEBACK_DISPUTE.as_u32(), 9);
        assert_eq!(DOMAIN_GOVERNANCE_VOTE.as_u32(), 10);
    }

    /// All nine domain constants must be pairwise distinct — a collision here
    /// would let a nonce from one privileged operation be replayed as another.
    #[test]
    fn test_domain_constants_are_unique() {
        let domains = [
            DOMAIN_BATCH_CHARGE,
            DOMAIN_ADMIN_ROTATION,
            DOMAIN_OPERATOR_BATCH_CHARGE,
            DOMAIN_METADATA_SIGNED,
            DOMAIN_MERCHANT_ROTATION,
            DOMAIN_CHARGE_INTERVAL,
            DOMAIN_DEPOSIT_FUNDS,
            DOMAIN_CHARGE_ONEOFF,
            DOMAIN_SUBSCRIBER_WITHDRAWAL,
            DOMAIN_CHARGEBACK_DISPUTE,
            DOMAIN_GOVERNANCE_VOTE,
        ];
        for i in 0..domains.len() {
            for j in (i + 1)..domains.len() {
                assert_ne!(domains[i], domains[j], "domain collision at indices {i} and {j}");
            }
        }
    }

    #[test]
    fn test_check_and_advance_overflow() {
        let env = Env::default();
        let signer = Address::generate(&env);
        let domain = DOMAIN_BATCH_CHARGE;
        let contract_id = env.register(crate::SubscriptionVault, ());

        let res = env.as_contract(&contract_id, || {
            let key = DataKey::AdminNonce(signer.clone(), domain.as_u32());
            // Seed with u64::MAX
            env.storage().persistent().set(&key, &u64::MAX);

            // Try to advance it, it should return Err(Error::Overflow)
            check_and_advance(&env, &signer, domain, u64::MAX)
        });
        assert_eq!(res, Err(Error::Overflow));
    }
}
