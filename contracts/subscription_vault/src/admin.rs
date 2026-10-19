//! Admin and config: init, min_topup, batch_charge, single charge.
//!
//! **PRs that only change admin or batch behavior should edit this file only.**

#![allow(dead_code)]

use crate::types::{
    AcceptedToken, AdminConfigChangedEvent, AdminProposal, AdminProposalCancelledEvent,
    AdminProposalClaimedEvent, AdminProposalCreatedEvent, AdminRotatedEvent, BatchChargeResult,
    DataKey, Error, FeeTokenConfiguredEvent, PendingTreasuryChange, RecoveryEvent,
    RecoveryInitiatorRole, RecoveryReason, SchemaMigrationSummary, Subscription, SubscriptionStatus,
    TreasuryChangeExecutedEvent, TreasuryChangeQueuedEvent, BATCH_MAX_SIZE, MAX_PROTOCOL_FEE_BIPS,
    TOPIC_RECOVERY, SUB_TTL_EXTEND_TO, SUB_TTL_THRESHOLD,
};
use crate::{
    charge_core::{charge_one, charge_usage_one},
    ChargeExecutionResult,
};
use soroban_sdk::{token, Address, Bytes, Env, String, Symbol, Vec};

pub fn get_schema_version(env: &Env) -> u32 {
    if let Some(v) = env
        .storage()
        .persistent()
        .get::<_, u32>(&DataKey::SchemaVersion)
    {
        v
    } else if let Some(v) = env
        .storage()
        .instance()
        .get::<_, u32>(&DataKey::SchemaVersion)
    {
        v
    } else {
        0
    }
}

pub fn read_config<T>(env: &Env, key: &DataKey) -> Option<T>
where
    T: soroban_sdk::IntoVal<Env, soroban_sdk::Val> + soroban_sdk::TryFromVal<Env, soroban_sdk::Val>,
{
    if let Some(val) = env.storage().persistent().get::<_, T>(key) {
        return Some(val);
    }
    if get_schema_version(env) < 3 {
        if let Some(val) = env.storage().instance().get::<_, T>(key) {
            return Some(val);
        }
    }
    None
}

pub fn write_config<T>(env: &Env, key: &DataKey, value: &T)
where
    T: soroban_sdk::IntoVal<Env, soroban_sdk::Val> + soroban_sdk::TryFromVal<Env, soroban_sdk::Val>,
{
    let version = get_schema_version(env);
    if version >= 3 {
        env.storage().persistent().set(key, value);
        crate::subscription::maybe_extend_ttl(env, key, SUB_TTL_THRESHOLD, SUB_TTL_EXTEND_TO);
        env.storage().instance().remove(key);
    } else {
        env.storage().instance().set(key, value);
    }
}

pub fn has_config(env: &Env, key: &DataKey) -> bool {
    if env.storage().persistent().has(key) {
        return true;
    }
    if get_schema_version(env) < 3 {
        if env.storage().instance().has(key) {
            return true;
        }
    }
    false
}

pub fn remove_config(env: &Env, key: &DataKey) {
    env.storage().persistent().remove(key);
    env.storage().instance().remove(key);
}

// ── Admin-config cooldown ────────────────────────────────────────────────────

/// Default per-key cooldown in seconds between protocol-wide admin config
/// mutations.  Six hours (21 600 s) gives guardians time to detect and respond
/// to a compromised admin key while keeping legitimate operations fast.
pub const CONFIG_COOLDOWN_SECS: u64 = 6 * 60 * 60;

/// Hash a human-readable `key_label` (e.g. `"MinTopup"`) into a
/// collision-free `BytesN<32>` used as the persistent-storage key for the
/// per-config-key cooldown timestamp.
fn hash_key_label(env: &Env, key_label: &str) -> soroban_sdk::BytesN<32> {
    let label_bytes = Bytes::from_slice(env, key_label.as_bytes());
    env.crypto().sha256(&label_bytes).into()
}

/// Enforce a per-key cooldown on protocol-wide admin config mutations.
///
/// 1. Hashes `key_label` to derive the storage key.
/// 2. Reads the previous mutation timestamp (0 if this is the first mutation).
/// 3. If the current ledger timestamp is within [`CONFIG_COOLDOWN_SECS`] of
///    `prev_ts`, returns [`Error::CooldownActive`].
/// 4. Otherwise, records the current timestamp and emits
///    [`AdminConfigChangedEvent`].
///
/// Call this **before** each config-mutating write.  Because Soroban
/// transactions are atomic, if the subsequent mutation fails the entire
/// transaction (including the timestamp write and event) reverts.
///
/// Governance proposals that execute through `do_execute_proposal` call
/// `write_config` directly and intentionally **bypass** this guard, allowing
/// guardians to override the cooldown when a supermajority agrees.
pub fn enforce_config_cooldown(env: &Env, key_label: &str) -> Result<u64, Error> {
    let hash = hash_key_label(env, key_label);
    let storage_key = DataKey::AdminConfigLastChangedAt(hash);

    let prev_ts: u64 = env
        .storage()
        .persistent()
        .get::<_, u64>(&storage_key)
        .unwrap_or(0);

    let now = env.ledger().timestamp();
    if prev_ts > 0 && now.saturating_sub(prev_ts) < CONFIG_COOLDOWN_SECS {
        return Err(Error::CooldownActive);
    }

    env.storage().persistent().set(&storage_key, &now);
    env.storage()
        .persistent()
        .extend_ttl(&storage_key, SUB_TTL_THRESHOLD, SUB_TTL_EXTEND_TO);
    crate::subscription::maybe_extend_ttl(env, &storage_key, SUB_TTL_THRESHOLD, SUB_TTL_EXTEND_TO);

    env.events().publish(
        (Symbol::new(env, "admin_config_changed"),),
        AdminConfigChangedEvent {
            key_label: String::from_str(env, key_label),
            prev_ts,
            timestamp: now,
            schema_version: crate::types::EVENT_SCHEMA_VERSION,
        },
    );

    Ok(prev_ts)
}

fn accepted_tokens_key() -> DataKey {
    DataKey::AcceptedTokens
}

fn accepted_token_decimals_key(token: &Address) -> DataKey {
    DataKey::TokenDecimals(token.clone())
}

pub fn do_init(
    env: &Env,
    token: Address,
    token_decimals: u32,
    admin: Address,
    min_topup: i128,
    grace_period: u64,
) -> Result<(), Error> {
    if has_config(env, &DataKey::Token) || has_config(env, &DataKey::Admin) {
        return Err(Error::AlreadyInitialized);
    }
    if min_topup <= 0 {
        return Err(Error::InvalidAmount);
    }
    if token_decimals > 19 {
        return Err(Error::InvalidTokenDecimals);
    }
    if token == env.current_contract_address() {
        return Err(Error::InvalidToken);
    }

    // Set schema version to target 3 in persistent storage first
    env.storage()
        .persistent()
        .set(&DataKey::SchemaVersion, &crate::STORAGE_VERSION);
    crate::subscription::maybe_extend_ttl(
        env,
        &DataKey::SchemaVersion,
        SUB_TTL_THRESHOLD,
        SUB_TTL_EXTEND_TO,
    );

    write_config(env, &DataKey::Token, &token);

    let instance = env.storage().instance();
    instance.set(&accepted_token_decimals_key(&token), &token_decimals);
    let mut tokens = Vec::new(env);
    tokens.push_back(token.clone());
    instance.set(&accepted_tokens_key(), &tokens);

    write_config(env, &DataKey::Admin, &admin);
    write_config(env, &DataKey::MinTopup, &min_topup);
    write_config(env, &DataKey::GracePeriod, &grace_period);

    env.events().publish(
        (Symbol::new(env, "initialized"),),
        (token, admin, min_topup, grace_period),
    );
    Ok(())
}

pub fn require_admin(env: &Env) -> Result<Address, Error> {
    read_config(env, &DataKey::Admin).ok_or(Error::NotInitialized)
}

pub fn require_admin_auth(env: &Env, admin: &Address) -> Result<(), Error> {
    admin.require_auth();
    let stored_admin = require_admin(env)?;
    if admin != &stored_admin {
        return Err(Error::Forbidden);
    }
    Ok(())
}

pub fn require_stored_admin_auth(env: &Env) -> Result<Address, Error> {
    let stored_admin = require_admin(env)?;
    stored_admin.require_auth();
    Ok(stored_admin)
}

/// Authorize `caller` as **either** the stored admin **or** the stored operator.
///
/// Used by the bulk pause/cancel operational tooling, which both privileged roles
/// may invoke. `caller.require_auth()` runs first (so an unauthenticated caller is
/// rejected before any identity comparison), then the address must match the
/// stored admin or operator; anything else returns [`Error::Unauthorized`].
///
/// This deliberately does **not** widen any other surface: the operator still has
/// no access to fund withdrawal, admin rotation, or governance.
pub fn require_admin_or_operator_auth(env: &Env, caller: &Address) -> Result<(), Error> {
    caller.require_auth();

    let stored_admin = require_admin(env)?;
    if caller == &stored_admin {
        return Ok(());
    }

    if let Some(stored_op) = crate::operator::get_operator(env) {
        if caller == &stored_op {
            return Ok(());
        }
    }

    Err(Error::Unauthorized)
}

// ── Multi-Sig Enforcement ────────────────────────────────────────────────────

/// Enforce multi-sig approval for critical admin operations.
/// 
/// This function checks that a valid governance proposal exists with sufficient
/// quorum for the specified operation. Critical operations that touch funds or
/// halt the contract require multi-sig guardian approval before execution.
pub fn require_multisig_approval(
    env: &Env,
    kind: crate::types::ProposalKind,
    target: &Address,
    target2: Option<&Address>,
    target3: u32,
) -> Result<u64, Error> {
    // Check if we have any guardians configured
    let guardians = crate::governance::list_guardians(env);
    if guardians.is_empty() {
        // If no guardians are configured, fall back to single admin for backward compatibility
        // This maintains the existing behavior until guardians are setup
        return Ok(0);
    }

    // Look for a valid proposal that matches this operation
    let current_proposal_id = crate::governance::get_current_proposal_id(env);
    
    // Search recent proposals to find a matching one
    for i in 0..10 {  // Check last 10 proposals
        if current_proposal_id < i {
            break;
        }
        let proposal_id = current_proposal_id - i;
        
        if let Some(proposal) = crate::governance::get_proposal(env, proposal_id) {
            // Check if this proposal matches our operation
            if proposal.kind == kind 
                && proposal.target == *target
                && proposal.target2.as_ref() == target2
                && proposal.target3 == target3
                && !proposal.executed
            {
                // Check if proposal has passed its ETA (timelock)
                let now = env.ledger().timestamp();
                if now < proposal.eta {
                    return Err(Error::MultiSigProposalExpired);
                }
                
                // Check if quorum is reached
                let (yes_weight, total_weight) = crate::governance::calculate_quorum(env, &proposal);
                let quorum_needed = (total_weight as u64 * proposal.quorum_bps as u64) / 10_000;
                
                if yes_weight as u64 >= quorum_needed {
                    return Ok(proposal_id);
                } else {
                    return Err(Error::MultiSigQuorumNotReached);
                }
            }
        }
    }
    
    Err(Error::MultiSigProposalNotFound)
}

/// Mark a multi-sig proposal as executed to prevent replay.
pub fn consume_multisig_proposal(env: &Env, proposal_id: u64) -> Result<(), Error> {
    if proposal_id == 0 {
        // No proposal to consume (single admin mode)
        return Ok(());
    }
    
    // Mark proposal as executed - this is handled by the governance module
    crate::governance::do_execute_proposal(env, proposal_id)
}

/// Check if multi-sig enforcement is enabled (i.e., guardians are configured).
pub fn is_multisig_enabled(env: &Env) -> bool {
    !crate::governance::list_guardians(env).is_empty()
}

// ── Configuration Functions ───────────────────────────────────────────────────

pub fn do_set_min_topup(env: &Env, admin: Address, min_topup: i128) -> Result<(), Error> {
    require_admin_auth(env, &admin)?;
    if min_topup <= 0 {
        return Err(Error::InvalidAmount);
    }
    let old_min_topup = get_min_topup(env).unwrap_or(0);
    enforce_config_cooldown(env, "MinTopup")?;
    write_config(env, &DataKey::MinTopup, &min_topup);
    env.events().publish(
        (Symbol::new(env, "min_topup_updated"),),
        crate::types::MinTopupUpdatedEvent {
            admin,
            old_min_topup,
            new_min_topup: min_topup,
            timestamp: env.ledger().timestamp(),
            schema_version: crate::types::EVENT_SCHEMA_VERSION,
        },
    );
    Ok(())
}

pub fn get_min_topup(env: &Env) -> Result<i128, Error> {
    read_config(env, &DataKey::MinTopup).ok_or(Error::NotInitialized)
}

pub fn do_set_grace_period(env: &Env, admin: Address, grace_period: u64) -> Result<(), Error> {
    require_admin_auth(env, &admin)?;
    enforce_config_cooldown(env, "GracePeriod")?;
    write_config(env, &DataKey::GracePeriod, &grace_period);
    env.events()
        .publish((Symbol::new(env, "grace_period_updated"),), grace_period);
    Ok(())
}

pub fn get_grace_period(env: &Env) -> Result<u64, Error> {
    read_config(env, &DataKey::GracePeriod).ok_or(Error::NotInitialized)
}

/// Set the global dispute response window. Admin updates are rate-limited;
/// guardian-approved governance proposals intentionally bypass that cooldown.
pub fn do_set_dispute_window_secs(
    env: &Env,
    admin: Address,
    window_secs: u64,
) -> Result<(), Error> {
    require_admin_auth(env, &admin)?;
    enforce_config_cooldown(env, "DisputeWindowSecs")?;
    apply_dispute_window_secs(env, window_secs)
}

/// Return the configured dispute response window, or the historical 14-day default.
pub fn get_dispute_window_secs(env: &Env) -> u64 {
    read_config(env, &DataKey::DisputeWindowSecs)
        .unwrap_or(crate::types::DISPUTE_WINDOW_SECS)
}

/// Apply a validated dispute-window value. Used by direct admin and governance paths.
pub(crate) fn apply_dispute_window_secs(env: &Env, window_secs: u64) -> Result<(), Error> {
    if window_secs == 0 || window_secs > u64::from(u32::MAX) {
        return Err(Error::InvalidInput);
    }

    let old_window_secs = get_dispute_window_secs(env);
    write_config(env, &DataKey::DisputeWindowSecs, &window_secs);
    env.events().publish(
        (Symbol::new(env, "dispute_window_updated"),),
        crate::types::DisputeWindowUpdatedEvent {
            old_window_secs,
            new_window_secs: window_secs,
            timestamp: env.ledger().timestamp(),
            schema_version: crate::types::EVENT_SCHEMA_VERSION,
        },
    );
    Ok(())
}

pub fn do_set_subscriber_create_cap(env: &Env, admin: Address, cap: u32) -> Result<(), Error> {
    require_admin_auth(env, &admin)?;
    write_config(env, &DataKey::SubscriberCreateCap, &cap);
    env.events().publish(
        (Symbol::new(env, "subscriber_create_cap_updated"),),
        cap,
    );
    Ok(())
}

pub fn get_subscriber_create_cap(env: &Env) -> u32 {
    read_config(env, &DataKey::SubscriberCreateCap).unwrap_or(50u32)
}

pub fn get_token(env: &Env) -> Result<Address, Error> {
    read_config(env, &DataKey::Token).ok_or(Error::NotFound)
}

pub fn get_token_decimals(env: &Env, token: &Address) -> Result<u32, Error> {
    env.storage()
        .instance()
        .get(&accepted_token_decimals_key(token))
        .ok_or(Error::NotFound)
}

pub fn is_token_accepted(env: &Env, token: &Address) -> bool {
    env.storage()
        .instance()
        .has(&accepted_token_decimals_key(token))
}

pub fn add_accepted_token(
    env: &Env,
    admin: Address,
    token: Address,
    decimals: u32,
) -> Result<(), Error> {
    require_admin_auth(env, &admin)?;

    // Require multi-sig approval for adding tokens
    let proposal_id = require_multisig_approval(
        env,
        crate::types::ProposalKind::AddAcceptedToken,
        &token,
        None,
        decimals,
    )?;

    let storage = env.storage().instance();
    if !storage.has(&accepted_token_decimals_key(&token)) {
        enforce_config_cooldown(env, "AcceptedTokens")?;
        let mut tokens: Vec<Address> = storage.get(&accepted_tokens_key()).unwrap_or(Vec::new(env));
        tokens.push_back(token.clone());
        storage.set(&accepted_tokens_key(), &tokens);
    }
    storage.set(&accepted_token_decimals_key(&token), &decimals);
    
    // Mark the multi-sig proposal as consumed
    consume_multisig_proposal(env, proposal_id)?;
    
    Ok(())
}

pub fn remove_accepted_token(env: &Env, admin: Address, token: Address) -> Result<(), Error> {
    require_admin_auth(env, &admin)?;

    let default_token = get_token(env)?;
    if token == default_token {
        return Err(Error::InvalidInput);
    }

    // A token cannot be removed while any live subscription still references
    // it; otherwise existing subscriptions could become unchargeable.
    let token_ids: Vec<u32> = env
        .storage()
        .instance()
        .get(&DataKey::TokenSubs(token.clone()))
        .unwrap_or(Vec::new(env));
    for id in token_ids.iter() {
        if let Some(sub) = env.storage().persistent().get::<_, Subscription>(&DataKey::Sub(id)) {
            if matches!(sub.status, SubscriptionStatus::Active | SubscriptionStatus::GracePeriod) {
                return Err(Error::InvalidStatusTransition);
            }
        }
    }

    // Require multi-sig approval for removing tokens
    let proposal_id = require_multisig_approval(
        env,
        crate::types::ProposalKind::RemoveAcceptedToken,
        &token,
        None,
        0, // no additional parameter needed for removal
    )?;

    enforce_config_cooldown(env, "AcceptedTokens")?;

    let storage = env.storage().instance();
    storage.remove(&accepted_token_decimals_key(&token));

    let tokens: Vec<Address> = storage.get(&accepted_tokens_key()).unwrap_or(Vec::new(env));
    let mut next = Vec::new(env);
    for t in tokens.iter() {
        if t != token {
            next.push_back(t);
        }
    }
    storage.set(&accepted_tokens_key(), &next);
    
    // Mark the multi-sig proposal as consumed
    consume_multisig_proposal(env, proposal_id)?;
    
    Ok(())
}

pub fn list_accepted_tokens(env: &Env) -> Vec<AcceptedToken> {
    let storage = env.storage().instance();
    let tokens: Vec<Address> = storage.get(&accepted_tokens_key()).unwrap_or(Vec::new(env));
    let mut out = Vec::new(env);
    for token in tokens.iter() {
        if let Some(decimals) = storage.get::<_, u32>(&accepted_token_decimals_key(&token)) {
            out.push_back(AcceptedToken { token, decimals });
        }
    }
    out
}

/// Cached admin configuration values to avoid repeated instance-storage
/// lookups inside batch loops.
pub(crate) struct CachedAdminConfig {
    pub fee_bps: u32,
    pub treasury: Option<Address>,
    pub grace_duration: u64,
    pub auto_pause_threshold: u32,
}

const BATCH_NONCE_CONTEXT: &str = "batch_nonce_ctx";

pub(crate) fn current_batch_nonce(env: &Env) -> Option<u64> {
    env.storage()
        .instance()
        .get(&Symbol::new(env, BATCH_NONCE_CONTEXT))
}

/// Read all admin charge-config values from storage at once.
/// Returns `Err` when `get_grace_period` fails (contract not initialized).
pub(crate) fn read_cached_admin_config(env: &Env) -> Result<CachedAdminConfig, Error> {
    Ok(CachedAdminConfig {
        fee_bps: get_protocol_fee_bps(env),
        treasury: get_treasury(env),
        grace_duration: get_grace_period(env)?,
        auto_pause_threshold: get_auto_pause_threshold(env),
    })
}

/// Execute the core batch-charge loop without auth or nonce advancement.
///
/// Called by both `do_batch_charge` (admin path) and
/// `operator::do_operator_batch_charge` (operator path) after authorization and
/// nonce validation have been satisfied.
pub(crate) fn execute_batch_charge(
    env: &Env,
    subscription_ids: &Vec<u32>,
    idem_keys: Option<&Vec<soroban_sdk::BytesN<32>>>,
) -> Vec<BatchChargeResult> {
    let now = env.ledger().timestamp();
    // Read all admin config values once so they are cached across the batch loop.
    let cached_admin = read_cached_admin_config(env);
    // Cache per-merchant paused/vacation status across the batch loop so
    // subscriptions sharing a merchant only hit storage once for it.
    let mut merchant_cache: soroban_sdk::Map<Address, (bool, bool)> = soroban_sdk::Map::new(env);
    let mut results = Vec::new(env);
    // Cache oracle prices per (merchant, token) pair so subscriptions sharing a
    // merchant and token within the batch don't redundantly re-query the oracle.
    let mut price_cache: std::vec::Vec<(Address, Address, u128)> = std::vec::Vec::new();
    for (idx, id) in subscription_ids.iter().enumerate() {
        let admin_ref = match &cached_admin {
            Ok(cfg) => Some(cfg),
            Err(_) => None,
        };
        // Extract the per-subscription idempotency key for this position, if
        // the caller supplied an idem_keys vector.  A missing entry at index
        // `idx` (shorter keys vec) is silently treated as no key for that ID.
        let idem_key: Option<soroban_sdk::BytesN<32>> = idem_keys
            .and_then(|keys| keys.get(idx as u32));
        let r = charge_one(env, id, now, idem_key, admin_ref, Some(&mut merchant_cache));
        let res = match r {
            Ok(ChargeExecutionResult::Charged) => BatchChargeResult {
                subscription_id: id,
                success: true,
                error_code: 0,
            },
            Ok(ChargeExecutionResult::InsufficientBalance) => BatchChargeResult {
                subscription_id: id,
                success: false,
                error_code: Error::InsufficientBalance.to_code(),
            },
            Ok(ChargeExecutionResult::LifetimeCapReached) => BatchChargeResult {
                subscription_id: id,
                success: false,
                error_code: Error::LifetimeCapReached.to_code(),
            },
            Ok(ChargeExecutionResult::ScheduledCancellation) => BatchChargeResult {
                subscription_id: id,
                success: true,
                error_code: 0,
            },
            // auto_renew=false and interval elapsed: silently skip without error.
            Ok(ChargeExecutionResult::Skipped) => BatchChargeResult {
                subscription_id: id,
                success: true,
                error_code: 0,
            },
            Err(e) => BatchChargeResult {
                subscription_id: id,
                success: false,
                error_code: e.to_code(),
            },
        };
        results.push_back(res);
    }
    results
}

/// Charge a batch of subscriptions. Admin only.
///
/// # Input validation
///
/// Input validation runs before charge processing. The nonce is validated
/// before processing, then consumed only if at least one result succeeds.
/// An all-failure batch leaves the nonce available for a corrected retry.
///
/// 1. **Batch length** — at most [`BATCH_MAX_SIZE`] ids. Larger batches are
///    rejected with [`Error::InvalidInput`]. This is a contract-level guard,
///    not a network one: without it an oversized batch would run past the
///    Soroban per-transaction instruction limit and fail with a generic
///    execution error that the caller cannot distinguish from out-of-gas.
///    Oversized batches are rejected wholesale — there are no partial results.
/// 2. **Duplicate ids** — a repeated id is rejected with
///    [`Error::InvalidInput`], matching the size check. The size check runs
///    first so an oversized batch is reported as oversized.
///
/// An empty batch is an explicit no-op that returns an empty result vector
/// without consuming the nonce.
pub fn do_batch_charge(
    env: &Env,
    subscription_ids: &Vec<u32>,
    idem_keys: Option<&Vec<soroban_sdk::BytesN<32>>>,
    nonce: u64,
) -> Result<Vec<BatchChargeResult>, Error> {
    let response = do_batch_charge_detailed(env, subscription_ids, idem_keys, nonce)?;
    if response.duplicate_id.is_some() {
        return Err(Error::InvalidInput);
    }
    Ok(response.results)
}

pub fn do_batch_charge_detailed(
    env: &Env,
    subscription_ids: &Vec<u32>,
    idem_keys: Option<&Vec<soroban_sdk::BytesN<32>>>,
    nonce: u64,
) -> Result<crate::types::BatchChargeResponse, Error> {
    let admin = require_stored_admin_auth(env)?;

    // Validate batch size before processing. Reported as InvalidInput so the
    // caller can distinguish "this input shape is rejected" from an item-level
    // charge failure, and so it cannot be confused with a network-level
    // instruction-budget abort.
    if subscription_ids.len() > BATCH_MAX_SIZE {
        return Err(Error::InvalidInput);
    }

    // Empty batch is allowed as a no-op
    if subscription_ids.len() == 0 {
        return Ok(crate::types::BatchChargeResponse {
            results: Vec::new(env),
            duplicate_id: None,
        });
    }

    // When idem_keys are supplied their count must match subscription_ids exactly
    // so that each key maps unambiguously to one ID by position.
    if let Some(keys) = idem_keys {
        if keys.len() != subscription_ids.len() {
            return Err(Error::InvalidInput);
        }
    }

    // Check for duplicate IDs and reject the entire batch if found
    let mut seen_ids = soroban_sdk::Vec::<u32>::new(env);
    for id in subscription_ids.iter() {
        if seen_ids.contains(&id) {
            return Ok(crate::types::BatchChargeResponse {
                results: Vec::new(env),
                duplicate_id: Some(id),
            });
        }
        seen_ids.push_back(id);
    }

    // Validate the nonce before processing, then consume it only if at least
    // one per-item result succeeds. The entrypoint's reentrancy lock protects
    // this deferred advance while charges perform external token calls.
    crate::nonce::validate_nonce(env, &admin, crate::nonce::DOMAIN_BATCH_CHARGE, nonce)?;
    let batch_nonce_key = Symbol::new(env, BATCH_NONCE_CONTEXT);
    env.storage().instance().set(&batch_nonce_key, &nonce);

    let results = execute_batch_charge(env, subscription_ids, idem_keys);
    if results.iter().any(|result| result.success) {
        if let Err(error) =
            crate::nonce::check_and_advance(env, &admin, crate::nonce::DOMAIN_BATCH_CHARGE, nonce)
        {
            env.storage().instance().remove(&batch_nonce_key);
            return Err(error);
        }
    }
    env.storage().instance().remove(&batch_nonce_key);
    Ok(crate::types::BatchChargeResponse {
        results,
        duplicate_id: None,
    })
}

/// Performs a single interval-based charge. Admin only.
pub fn do_charge_subscription(
    env: &Env,
    subscription_id: u32,
) -> Result<ChargeExecutionResult, Error> {
    let _admin = require_stored_admin_auth(env)?;

    let now = env.ledger().timestamp();
    charge_one(env, subscription_id, now, None, None, None)
}

/// Performs a single usage-based charge. Admin only.
pub fn do_charge_usage(
    env: &Env,
    subscription_id: u32,
    usage_amount: i128,
    reference: String,
) -> Result<(), Error> {
    let _admin = require_stored_admin_auth(env)?;

    charge_usage_one(env, subscription_id, usage_amount, reference)?;
    Ok(())
}

pub fn do_get_admin(env: &Env) -> Result<Address, Error> {
    read_config(env, &DataKey::Admin).ok_or(Error::NotInitialized)
}

pub fn do_rotate_admin(
    env: &Env,
    current_admin: Address,
    new_admin: Address,
    nonce: u64,
) -> Result<(), Error> {
    require_admin_auth(env, &current_admin)?;

    // Consume nonce for this domain before any other state mutation.
    crate::nonce::check_and_advance(
        env,
        &current_admin,
        crate::nonce::DOMAIN_ADMIN_ROTATION,
        nonce,
    )?;

    // Disallow self-rotation: rotating to the same address is a no-op that
    // could mask misconfiguration and wastes a transaction.
    if new_admin == current_admin {
        return Err(Error::SelfRotation);
    }

    // Disallow rotating to the contract itself: that would permanently lock
    // admin privileges since the contract cannot sign transactions.
    if new_admin == env.current_contract_address() {
        return Err(Error::InvalidNewAdmin);
    }

    // Require multi-sig approval for admin rotation
    let proposal_id = require_multisig_approval(
        env,
        crate::types::ProposalKind::RotateAdmin,
        &new_admin,
        None,
        0, // no additional target3 parameter needed
    )?;

    enforce_config_cooldown(env, "Admin")?;

    // Atomic swap: write new admin before emitting the event so any indexer
    // that reads state on the event sees the already-updated value.
    write_config(env, &DataKey::Admin, &new_admin);
    crate::governance::invalidate_pending_proposals(env);

    // Mark the multi-sig proposal as consumed
    consume_multisig_proposal(env, proposal_id)?;

    env.events().publish(
        (Symbol::new(env, "admin_rotated"),),
        AdminRotatedEvent {
            old_admin: current_admin,
            new_admin,
            timestamp: env.ledger().timestamp(),
            schema_version: crate::types::EVENT_SCHEMA_VERSION,
        },
    );

    Ok(())
}

pub fn do_recover_stranded_funds(
    env: &Env,
    admin: Address,
    token: Address,
    recipient: Address,
    amount: i128,
    recovery_id: String,
    reason: RecoveryReason,
) -> Result<(), Error> {
    require_admin_auth(env, &admin)?;

    if amount <= 0 {
        return Err(Error::InvalidRecoveryAmount);
    }

    // Require multi-sig approval for fund recovery
    // We use the amount truncated to u32 as a reasonable approximation
    // for the proposal matching. For large amounts, consider splitting.
    let amount_u32 = if amount > u32::MAX as i128 {
        u32::MAX
    } else {
        amount as u32
    };
    
    let proposal_id = require_multisig_approval(
        env,
        crate::types::ProposalKind::RecoverStrandedFunds,
        &recipient,
        Some(&token),
        amount_u32,
    )?;

    // Check for replay protection
    let recovery_key = DataKey::Recovery(recovery_id.clone());
    if env.storage().persistent().has(&recovery_key) {
        return Err(Error::Replay);
    }

    // Validate available recoverable balance
    let token_client = token::Client::new(env, &token);
    let contract_balance = token_client.balance(&env.current_contract_address());
    let accounted_balance = crate::accounting::get_total_accounted(env, &token);

    let recoverable = contract_balance
        .checked_sub(accounted_balance)
        .ok_or(Error::Underflow)?;
    if amount > recoverable {
        return Err(Error::InsufficientBalance);
    }

    // Mark recovery as executed
    env.storage().persistent().set(&recovery_key, &true);

    // Mark the multi-sig proposal as consumed
    consume_multisig_proposal(env, proposal_id)?;

    let recovery_event = RecoveryEvent {
        admin: admin.clone(),
        recipient: recipient.clone(),
        token: token.clone(),
        amount,
        reason,
        timestamp: env.ledger().timestamp(),
        schema_version: crate::types::EVENT_SCHEMA_VERSION,
        // `require_admin_auth` above already pinned this call to the admin
        // path, so the role is emitted rather than derived from caller input.
        initiator_role: RecoveryInitiatorRole::Admin,
    };

    env.events().publish(
        (TOPIC_RECOVERY, admin.clone()),
        recovery_event,
    );

    // Actual token transfer logic
    token_client.transfer(&env.current_contract_address(), &recipient, &amount);

    Ok(())
}

// ── Protocol fee helpers ──────────────────────────────────────────────────────

/// Set protocol fee basis points and treasury address. Admin only.
///
/// fee_bps must be in 0..=MAX_PROTOCOL_FEE_BIPS (500). Setting fee_bps to 0
/// disables fee collection. Values above the cap are rejected with
/// `ProtocolFeeTooHigh`.
const TREASURY_CHANGE_DELAY_SECS: u64 = 48 * 24 * 60 * 60;

pub fn queue_treasury_change(
    env: &Env,
    admin: Address,
    treasury: Address,
    fee_bps: u32,
) -> Result<(), Error> {
    require_admin_auth(env, &admin)?;
    if fee_bps > MAX_PROTOCOL_FEE_BIPS {
        return Err(Error::ProtocolFeeTooHigh);
    }
    if env.storage().persistent().has(&DataKey::PendingTreasuryChange) {
        return Err(Error::InvalidInput);
    }

    // Require multi-sig approval for protocol fee changes
    let proposal_id = require_multisig_approval(
        env,
        crate::types::ProposalKind::SetProtocolFee,
        &treasury,
        None,
        fee_bps,
    )?;

    let effective_at = env.ledger().timestamp().saturating_add(TREASURY_CHANGE_DELAY_SECS);
    let pending = PendingTreasuryChange {
        new_treasury: treasury.clone(),
        new_fee_bps: fee_bps,
        effective_at,
    };
    env.storage().persistent().set(&DataKey::PendingTreasuryChange, &pending);
    env.storage()
        .persistent()
        .extend_ttl(&DataKey::PendingTreasuryChange, SUB_TTL_THRESHOLD, SUB_TTL_EXTEND_TO);

    enforce_config_cooldown(env, "ProtocolFee")?;
    write_config(env, &DataKey::FeeBps, &fee_bps);
    write_config(env, &DataKey::Treasury, &treasury);
    
    // Mark the multi-sig proposal as consumed
    consume_multisig_proposal(env, proposal_id)?;
    
    env.events().publish(
        (Symbol::new(env, "treasury_change_queued"),),
        TreasuryChangeQueuedEvent {
            admin: admin.clone(),
            treasury,
            fee_bps,
            effective_at,
            timestamp: env.ledger().timestamp(),
            schema_version: crate::types::EVENT_SCHEMA_VERSION,
        },
    );
    Ok(())
}

pub fn execute_treasury_change(env: &Env, admin: Address) -> Result<(), Error> {
    require_admin_auth(env, &admin)?;
    let pending = env
        .storage()
        .persistent()
        .get::<_, PendingTreasuryChange>(&DataKey::PendingTreasuryChange)
        .ok_or(Error::NotFound)?;

    let now = env.ledger().timestamp();
    if now < pending.effective_at {
        return Err(Error::TimelockNotElapsed);
    }

    write_config(env, &DataKey::FeeBps, &pending.new_fee_bps);
    write_config(env, &DataKey::Treasury, &pending.new_treasury);
    env.storage().persistent().remove(&DataKey::PendingTreasuryChange);

    env.events().publish(
        (Symbol::new(env, "treasury_change_executed"),),
        TreasuryChangeExecutedEvent {
            admin: admin.clone(),
            treasury: pending.new_treasury.clone(),
            fee_bps: pending.new_fee_bps,
            effective_at: pending.effective_at,
            timestamp: now,
            schema_version: crate::types::EVENT_SCHEMA_VERSION,
        },
    );
    Ok(())
}

pub fn cancel_treasury_change(env: &Env, admin: Address) -> Result<(), Error> {
    require_admin_auth(env, &admin)?;
    if !env.storage().persistent().has(&DataKey::PendingTreasuryChange) {
        return Err(Error::NotFound);
    }
    env.storage().persistent().remove(&DataKey::PendingTreasuryChange);
    Ok(())
}

pub fn set_protocol_fee(
    env: &Env,
    admin: Address,
    treasury: Address,
    fee_bps: u32,
) -> Result<(), Error> {
    queue_treasury_change(env, admin, treasury, fee_bps)
}

/// Return the configured protocol fee in basis points (0 = disabled).
pub fn get_protocol_fee_bps(env: &Env) -> u32 {
    read_config(env, &DataKey::FeeBps).unwrap_or(0u32)
}

/// Return the configured treasury address, or None if not set.
pub fn get_treasury(env: &Env) -> Option<Address> {
    read_config(env, &DataKey::Treasury)
}

/// Set the fee-token override address. Admin only.
///
/// When set, protocol fees are charged in `fee_token` instead of the
/// subscription's settlement token, converted through the oracle at charge
/// time. Pass `None` to clear the override and revert to the default behaviour
/// (fees paid in the subscription's settlement token).
pub fn set_fee_token(
    env: &Env,
    admin: Address,
    fee_token: Option<Address>,
) -> Result<(), crate::types::Error> {
    admin.require_auth();
    let stored = require_admin(env)?;
    if admin != stored {
        return Err(crate::types::Error::Unauthorized);
    }
    enforce_config_cooldown(env, "FeeToken")?;
    if let Some(ref token) = fee_token {
        write_config(env, &DataKey::FeeToken, token);
    } else {
        remove_config(env, &DataKey::FeeToken);
    }
    env.events().publish(
        (Symbol::new(env, "fee_token_configured"),),
        FeeTokenConfiguredEvent {
            admin,
            fee_token,
            timestamp: env.ledger().timestamp(),
            schema_version: crate::types::EVENT_SCHEMA_VERSION,
        },
    );
    Ok(())
}

/// Return the configured fee-token override address, or `None` if not set.
pub fn get_fee_token(env: &Env) -> Option<Address> {
    read_config(env, &DataKey::FeeToken)
}

/// Return the configured buyout premium in basis points, defaulting to 0.
pub fn get_buyout_premium_bps(env: &Env) -> u32 {
    read_config(env, &DataKey::BuyoutPremiumBps).unwrap_or(0u32)
}

/// Set the auto-pause threshold (number of consecutive InsufficientBalance failures
/// before a subscription is automatically paused). `0` disables auto-pause.
pub fn do_set_auto_pause_threshold(env: &Env, admin: Address, threshold: u32) -> Result<(), Error> {
    require_admin_auth(env, &admin)?;
    env.storage()
        .instance()
        .set(&DataKey::AutoPauseThreshold, &threshold);
    Ok(())
}

// ── Two-step admin proposal ──────────────────────────────────────────────────

const PROPOSAL_WINDOW_SECS: u64 = 7 * 24 * 60 * 60;
const ADMIN_PROPOSAL_COOLDOWN_SECS: u64 = 24 * 60 * 60;

fn proposal_key(env: &Env) -> Symbol {
    Symbol::new(env, "admin_proposal")
}

pub fn do_propose_admin(env: &Env, current_admin: Address, new_admin: Address) -> Result<(), Error> {
    current_admin.require_auth();
    let stored = require_admin(env)?;
    if current_admin != stored {
        return Err(Error::Unauthorized);
    }

    if new_admin == env.current_contract_address() {
        return Err(Error::InvalidNewAdmin);
    }

    let storage = env.storage().instance();
    let now = env.ledger().timestamp();

    // A pending proposal only blocks a replacement while it is still inside its
    // `PROPOSAL_WINDOW_SECS` window. `do_claim_admin_role` refuses anything past
    // `expires_at`, so once the window has elapsed the stored proposal is dead:
    // it can never be claimed again. Leaving it in place would keep returning
    // `ProposalAlreadyExists` forever and force the admin to call
    // `cancel_admin_proposal` before they could ever rotate again — which is not
    // what `docs/admin_rotation.md` promises ("a second propose_admin fails with
    // ProposalAlreadyExists until the first is claimed, cancelled, or expires").
    //
    // An expired proposal is therefore treated as absent and overwritten. The
    // boundary is `now <= expires_at` so that this agrees exactly with the
    // claim-side guard, which rejects on `now > expires_at`.
    if let Some(pending) = storage.get::<_, AdminProposal>(&proposal_key(env)) {
        if now <= pending.expires_at {
            return Err(Error::ProposalAlreadyExists);
        }
    }

    let proposal = AdminProposal {
        new_admin: new_admin.clone(),
        proposed_at: now,
        expires_at: now.saturating_add(PROPOSAL_WINDOW_SECS),
    };
    storage.set(&proposal_key(env), &proposal);

    env.events().publish(
        (Symbol::new(env, "admin_proposal_created"),),
        AdminProposalCreatedEvent {
            old_admin: current_admin,
            new_admin,
            expires_at: proposal.expires_at,
            timestamp: now,
        },
    );
    Ok(())
}

pub fn do_claim_admin_role(env: &Env, claimant: Address) -> Result<(), Error> {
    claimant.require_auth();

    let storage = env.storage().instance();
    let proposal: AdminProposal = storage
        .get(&proposal_key(env))
        .ok_or(Error::ProposalNotFound)?;

    if claimant != proposal.new_admin {
        return Err(Error::InvalidClaimant);
    }

    let now = env.ledger().timestamp();
    if now < proposal.proposed_at.saturating_add(ADMIN_PROPOSAL_COOLDOWN_SECS) {
        return Err(Error::ProposalCooldownActive);
    }

    if now > proposal.expires_at {
        storage.remove(&proposal_key(env));
        return Err(Error::ProposalExpired);
    }

    let old_admin: Address = require_admin(env)?;

    storage.remove(&proposal_key(env));
    write_config(env, &DataKey::Admin, &claimant);
    crate::governance::invalidate_pending_proposals(env);

    env.events().publish(
        (Symbol::new(env, "admin_proposal_claimed"),),
        AdminProposalClaimedEvent {
            old_admin,
            new_admin: claimant,
            timestamp: now,
        },
    );
    Ok(())
}

pub fn do_cancel_admin_proposal(env: &Env, admin: Address) -> Result<(), Error> {
    admin.require_auth();
    let stored = require_admin(env)?;
    if admin != stored {
        return Err(Error::Unauthorized);
    }

    let storage = env.storage().instance();
    if !storage.has(&proposal_key(env)) {
        return Err(Error::NoActiveProposal);
    }

    storage.remove(&proposal_key(env));

    env.events().publish(
        (Symbol::new(env, "admin_proposal_cancelled"),),
        AdminProposalCancelledEvent {
            admin,
            timestamp: env.ledger().timestamp(),
        },
    );
    Ok(())
}

pub fn get_admin_proposal(env: &Env) -> Option<AdminProposal> {
    env.storage().instance().get(&proposal_key(env))
}

/// Return the configured auto-pause threshold. `0` means disabled.
pub fn get_auto_pause_threshold(env: &Env) -> u32 {
    env.storage()
        .instance()
        .get(&DataKey::AutoPauseThreshold)
        .unwrap_or(0u32)
}

// ── Schema migration ──────────────────────────────────────────────────────────

pub fn rewrite_subscriptions_for_ledger_expiration(env: &Env) -> u32 {
    let next_id: u32 = read_config(env, &DataKey::NextId).unwrap_or(0);
    let mut touched = 0u32;
    for id in 0..next_id {
        let key = DataKey::Sub(id);
        if let Some(sub) = env
            .storage()
            .persistent()
            .get::<_, crate::types::Subscription>(&key)
        {
            env.storage().persistent().set(&key, &sub);
            env.storage().persistent().extend_ttl(
                &key,
                SUB_TTL_THRESHOLD,
                SUB_TTL_EXTEND_TO,
            );
            touched = touched.saturating_add(1);
        }
    }
    touched
}

/// v4 → v5 migration: rewrite every `DataKey::Sub(id)` record so the new
/// `sub_account_label: Option<Symbol>` field deserializes cleanly for
/// subscriptions created before STORAGE_VERSION 5.  The in-memory struct
/// already carries `sub_account_label: None` after the deserialization
/// round-trip, so this just needs to read-write each record.
pub fn rewrite_subscriptions_for_sub_account_label(env: &Env) -> u32 {
    let next_id: u32 = read_config(env, &DataKey::NextId).unwrap_or(0);
    let mut touched = 0u32;
    for id in 0..next_id {
        let key = DataKey::Sub(id);
        if let Some(sub) = env
            .storage()
            .persistent()
            .get::<_, crate::types::Subscription>(&key)
        {
            // Round-trip: deserialise populates `sub_account_label: None`,
            // then write back so XDR encoding includes the new field.
            env.storage().persistent().set(&key, &sub);
            env.storage().persistent().extend_ttl(
                &key,
                SUB_TTL_THRESHOLD,
                SUB_TTL_EXTEND_TO,
            );
            touched = touched.saturating_add(1);
        }
    }
    touched
}

fn count_legacy_config_keys(env: &Env) -> u32 {
    let instance = env.storage().instance();
    let keys = [
        DataKey::Token,
        DataKey::Admin,
        DataKey::MinTopup,
        DataKey::GracePeriod,
        DataKey::DisputeWindowSecs,
        DataKey::NextId,
        DataKey::EmergencyStop,
        DataKey::Treasury,
        DataKey::FeeBps,
        DataKey::Operator,
        DataKey::SchemaVersion,
    ];
    keys.iter().filter(|key| instance.has(*key)).count() as u32
}

fn count_rewriteable_subscriptions(env: &Env) -> u32 {
    let next_id: u32 = read_config(env, &DataKey::NextId).unwrap_or(0);
    let mut count = 0u32;
    for id in 0..next_id {
        if env
            .storage()
            .persistent()
            .get::<_, crate::types::Subscription>(&DataKey::Sub(id))
            .is_some()
        {
            count = count.saturating_add(1);
        }
    }
    count
}

fn plan_schema_migration(
    env: &Env,
    stored_version: u32,
    binary_version: u32,
    dry_run: bool,
) -> SchemaMigrationSummary {
    let needs_config_migration = stored_version <= 2 && binary_version >= 3;
    let needs_ledger_expiration_rewrite = stored_version <= 3 && binary_version >= 4;
    let needs_sub_account_label_rewrite = stored_version <= 4 && binary_version >= 5;
    let subscription_count = if dry_run
        && (needs_ledger_expiration_rewrite || needs_sub_account_label_rewrite)
    {
        count_rewriteable_subscriptions(env)
    } else {
        0
    };

    SchemaMigrationSummary {
        from_version: stored_version,
        to_version: binary_version,
        config_keys_to_migrate: if needs_config_migration {
            count_legacy_config_keys(env)
        } else {
            0
        },
        subs_for_ledger_expiry: if needs_ledger_expiration_rewrite {
            subscription_count
        } else {
            0
        },
        subs_for_sub_account_label: if needs_sub_account_label_rewrite {
            subscription_count
        } else {
            0
        },
        dry_run,
        would_change: stored_version != binary_version,
    }
}

pub fn do_migrate_config_to_persistent_internal(env: &Env) -> Result<(), Error> {
    let instance = env.storage().instance();
    let persistent = env.storage().persistent();

    // 1. Token
    if instance.has(&DataKey::Token) {
        let val: Address = instance.get(&DataKey::Token).unwrap();
        persistent.set(&DataKey::Token, &val);
        crate::subscription::maybe_extend_ttl(env, &DataKey::Token, SUB_TTL_THRESHOLD, SUB_TTL_EXTEND_TO);
        instance.remove(&DataKey::Token);
    }

    // 2. Admin
    if instance.has(&DataKey::Admin) {
        let val: Address = instance.get(&DataKey::Admin).unwrap();
        persistent.set(&DataKey::Admin, &val);
        crate::subscription::maybe_extend_ttl(env, &DataKey::Admin, SUB_TTL_THRESHOLD, SUB_TTL_EXTEND_TO);
        instance.remove(&DataKey::Admin);
    }

    // 3. MinTopup
    if instance.has(&DataKey::MinTopup) {
        let val: i128 = instance.get(&DataKey::MinTopup).unwrap();
        persistent.set(&DataKey::MinTopup, &val);
        crate::subscription::maybe_extend_ttl(env, &DataKey::MinTopup, SUB_TTL_THRESHOLD, SUB_TTL_EXTEND_TO);
        instance.remove(&DataKey::MinTopup);
    }

    // 4. GracePeriod
    if instance.has(&DataKey::GracePeriod) {
        let val: u64 = instance.get(&DataKey::GracePeriod).unwrap_or(0);
        persistent.set(&DataKey::GracePeriod, &val);
        crate::subscription::maybe_extend_ttl(env, &DataKey::GracePeriod, SUB_TTL_THRESHOLD, SUB_TTL_EXTEND_TO);
        instance.remove(&DataKey::GracePeriod);
    }

    // DisputeWindowSecs
    if instance.has(&DataKey::DisputeWindowSecs) {
        let val: u64 = instance.get(&DataKey::DisputeWindowSecs).unwrap_or(0);
        persistent.set(&DataKey::DisputeWindowSecs, &val);
        crate::subscription::maybe_extend_ttl(
            env,
            &DataKey::DisputeWindowSecs,
            SUB_TTL_THRESHOLD,
            SUB_TTL_EXTEND_TO,
        );
        instance.remove(&DataKey::DisputeWindowSecs);
    }

    // 5. NextId
    if instance.has(&DataKey::NextId) {
        let val: u32 = instance.get(&DataKey::NextId).unwrap_or(0);
        persistent.set(&DataKey::NextId, &val);
        crate::subscription::maybe_extend_ttl(env, &DataKey::NextId, SUB_TTL_THRESHOLD, SUB_TTL_EXTEND_TO);
        instance.remove(&DataKey::NextId);
    }

    // 5. EmergencyStop
    if instance.has(&DataKey::EmergencyStop) {
        let val: bool = instance.get(&DataKey::EmergencyStop).unwrap_or(false);
        persistent.set(&DataKey::EmergencyStop, &val);
        crate::subscription::maybe_extend_ttl(
            env,
            &DataKey::EmergencyStop,
            SUB_TTL_THRESHOLD,
            SUB_TTL_EXTEND_TO,
        );
        instance.remove(&DataKey::EmergencyStop);
    }

    // 6. Treasury
    if instance.has(&DataKey::Treasury) {
        let val: Address = instance.get(&DataKey::Treasury).unwrap();
        persistent.set(&DataKey::Treasury, &val);
        crate::subscription::maybe_extend_ttl(env, &DataKey::Treasury, SUB_TTL_THRESHOLD, SUB_TTL_EXTEND_TO);
        instance.remove(&DataKey::Treasury);
    }

    // 7. FeeBps
    if instance.has(&DataKey::FeeBps) {
        let val: u32 = instance.get(&DataKey::FeeBps).unwrap_or(0);
        persistent.set(&DataKey::FeeBps, &val);
        crate::subscription::maybe_extend_ttl(env, &DataKey::FeeBps, SUB_TTL_THRESHOLD, SUB_TTL_EXTEND_TO);
        instance.remove(&DataKey::FeeBps);
    }

    // 8. Operator
    if instance.has(&DataKey::Operator) {
        let val: Address = instance.get(&DataKey::Operator).unwrap();
        persistent.set(&DataKey::Operator, &val);
        crate::subscription::maybe_extend_ttl(env, &DataKey::Operator, SUB_TTL_THRESHOLD, SUB_TTL_EXTEND_TO);
        instance.remove(&DataKey::Operator);
    }

    // 9. SchemaVersion
    if instance.has(&DataKey::SchemaVersion) {
        persistent.set(&DataKey::SchemaVersion, &3u32);
        crate::subscription::maybe_extend_ttl(
            env,
            &DataKey::SchemaVersion,
            SUB_TTL_THRESHOLD,
            SUB_TTL_EXTEND_TO,
        );
        instance.remove(&DataKey::SchemaVersion);
    } else {
        persistent.set(&DataKey::SchemaVersion, &3u32);
        crate::subscription::maybe_extend_ttl(
            env,
            &DataKey::SchemaVersion,
            SUB_TTL_THRESHOLD,
            SUB_TTL_EXTEND_TO,
        );
    }

    Ok(())
}

pub fn migrate_config_to_persistent(env: &Env, admin: Address) -> Result<(), Error> {
    require_admin_auth(env, &admin)?;

    let stored_version = get_schema_version(env);
    if stored_version > 3 {
        return Err(Error::SchemaMigrationDowngrade);
    }

    do_migrate_config_to_persistent_internal(env)?;
    verify_migration(env, 3)?;

    env.events().publish(
        (Symbol::new(env, "schema_migrated"),),
        crate::types::SchemaMigratedEvent {
            admin,
            from_version: stored_version,
            to_version: 3,
            timestamp: env.ledger().timestamp(),
            schema_version: crate::types::EVENT_SCHEMA_VERSION,
        },
    );

    Ok(())
}

fn verify_migration(env: &Env, expected_version: u32) -> Result<(), Error> {
    let storage = env.storage();
    let schema_is_persistent = storage
        .persistent()
        .get::<_, u32>(&DataKey::SchemaVersion)
        == Some(expected_version);
    let schema_is_instance = storage
        .instance()
        .get::<_, u32>(&DataKey::SchemaVersion)
        == Some(expected_version);

    if get_schema_version(env) != expected_version
        || (expected_version >= 3 && (!schema_is_persistent || schema_is_instance))
        || (expected_version < 3 && (!schema_is_instance || schema_is_persistent))
    {
        return Err(Error::SchemaMigrationVerificationFailed);
    }

    if expected_version >= 3 {
        for key in [
            DataKey::Token,
            DataKey::Admin,
            DataKey::MinTopup,
            DataKey::GracePeriod,
            DataKey::NextId,
            DataKey::EmergencyStop,
            DataKey::Treasury,
            DataKey::FeeBps,
            DataKey::Operator,
        ] {
            if storage.instance().has(&key) {
                return Err(Error::SchemaMigrationVerificationFailed);
            }
        }

        let next_id: u32 = read_config(env, &DataKey::NextId).unwrap_or(0);
        if next_id > 0 {
            for id in [0, next_id / 2, next_id - 1] {
                if let Some(sub) = storage
                    .persistent()
                    .get::<_, Subscription>(&DataKey::Sub(id))
                {
                    if sub.amount <= 0
                        || sub.interval_seconds == 0
                        || sub.prepaid_balance < 0
                        || sub.lifetime_charged < 0
                    {
                        return Err(Error::SchemaMigrationVerificationFailed);
                    }
                }
            }
        }
    }

    Ok(())
}

/// Execute a schema migration from the stored version to `STORAGE_VERSION`.
pub fn do_migrate(
    env: &Env,
    admin: Address,
    binary_version: u32,
    dry_run: bool,
) -> Result<SchemaMigrationSummary, crate::types::Error> {
    require_admin_auth(env, &admin)?;

    let stored_version = get_schema_version(env);

    if stored_version > binary_version {
        return Err(crate::types::Error::SchemaVersionMismatch);
    }

    let mut summary = plan_schema_migration(env, stored_version, binary_version, dry_run);

    if dry_run {
        return Ok(summary);
    }

    if stored_version == binary_version {
        return Ok(summary);
    }

    let mut current = stored_version;
    while current < binary_version {
        match (current, binary_version) {
            (v, _) if v < 2 => {
                current = 2;
            }
            (2, _) => {
                do_migrate_config_to_persistent_internal(env)?;
                current = 3;
            }
            // v3 → v4: rewrite every `DataKey::Sub(id)` record so the new
            // `expires_at_ledger: Option<u32>` field deserializes cleanly
            // for subscriptions created before STORAGE_VERSION 4. Soroban's
            // `#[contracttype]` serialization is positional, so old records
            // would otherwise fail to deserialize and every `get_subscription`
            // call would panic after the binary upgrade.
            (3, _) => {
                summary.subs_for_ledger_expiry =
                    rewrite_subscriptions_for_ledger_expiration(env);
                current = 4;
            }
            // v4 → v5: rewrite every `DataKey::Sub(id)` record so the new
            // `sub_account_label: Option<Symbol>` field deserializes cleanly
            // for subscriptions created before STORAGE_VERSION 5.
            (4, _) => {
                summary.subs_for_sub_account_label =
                    rewrite_subscriptions_for_sub_account_label(env);
                current = 5;
            }
            _ => {
                current += 1;
            }
        }
    }

    if binary_version >= 3 {
        env.storage()
            .persistent()
            .set(&crate::types::DataKey::SchemaVersion, &binary_version);
        crate::subscription::maybe_extend_ttl(
            env,
            &crate::types::DataKey::SchemaVersion,
            SUB_TTL_THRESHOLD,
            SUB_TTL_EXTEND_TO,
        );
        env.storage()
            .instance()
            .remove(&crate::types::DataKey::SchemaVersion);
    } else {
        env.storage()
            .instance()
            .set(&crate::types::DataKey::SchemaVersion, &binary_version);
    }

    verify_migration(env, binary_version)?;

    env.events().publish(
        (soroban_sdk::Symbol::new(env, "schema_migrated"),),
        crate::types::SchemaMigratedEvent {
            admin,
            from_version: stored_version,
            to_version: binary_version,
            timestamp: env.ledger().timestamp(),
            schema_version: crate::types::EVENT_SCHEMA_VERSION,
        },
    );

    Ok(summary)
}
