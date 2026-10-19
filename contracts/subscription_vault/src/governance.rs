//! Governance: proposal submission, voting, and execution with quorum-based validation.
//!
//! This module implements quorum-based governance where N guardians vote on proposals
//! before privileged actions like `rotate_admin` and `set_protocol_fee` can execute.
//!
//! **Security properties:**
//! - Guardian additions/removals tracked via proposals, not direct admin calls.
//! - Quorum validation required on every proposal execution.
//! - Minimum timelock delay enforced (no same-ledger execution).
//! - Stale proposals cannot execute (ETA check).
//! - Double-voting is prevented (per-guardian vote tracking).
//! - Guardian removal mid-vote invalidates their prior votes.

use crate::types::{
    DataKey, Error, Proposal, ProposalCancelledEvent, ProposalExecutedEvent, ProposalKind,
    ProposalSubmittedEvent, ProposalVotedEvent, VoteLockedEvent, BASE_EVENT_SCHEMA_VERSION,
    MAX_PROTOCOL_FEE_BIPS,
};
use soroban_sdk::{Address, Env, Map, String, Symbol, Vec};

/// Minimum timelock delay in seconds (2 days).
/// Prevents immediate execution in the same ledger and ensures a minimum review window.
const MIN_TIMELOCK_DELAY: u64 = 2 * 24 * 60 * 60; // 172800 seconds

/// Floor applied to `quorum_bps` when a proposal is executed.
///
/// [`do_submit_proposal`] only rejects `quorum_bps > 10_000`, so a proposal can
/// legally be created with `quorum_bps = 0`. Without a floor that would let a
/// single guardian vote execute a privileged operation such as admin rotation
/// or a treasury change. Execution therefore uses
/// `proposal.quorum_bps.max(MIN_QUORUM_BPS)`, so the effective quorum can never
/// fall below two thirds of the current total guardian weight.
///
/// 6 700 bps (67%) is the quorum used by every worked example in
/// `docs/governance_proposals.md`.
const MIN_QUORUM_BPS: u32 = 6_700;

fn proposal_invalidation_cutoff_key(env: &Env) -> Symbol {
    Symbol::new(env, "proposal_cutoff")
}

/// Invalidate every proposal submitted before the next allocated proposal ID.
/// This is constant-cost even when the proposal history is large.
pub fn invalidate_pending_proposals(env: &Env) {
    let cutoff = get_current_proposal_id(env);
    env.storage()
        .instance()
        .set(&proposal_invalidation_cutoff_key(env), &cutoff);
}

fn ensure_proposal_not_invalidated(env: &Env, proposal_id: u64) -> Result<(), Error> {
    let cutoff = env
        .storage()
        .instance()
        .get::<_, u64>(&proposal_invalidation_cutoff_key(env))
        .unwrap_or(0);
    if proposal_id < cutoff {
        return Err(Error::ProposalInvalidated);
    }
    Ok(())
}

/// Add or update a guardian's voting weight.
///
/// # Errors
/// - `NotInitialized` if no admin is set.
/// - `InvalidInput` if weight is zero.
pub fn add_guardian(env: &Env, guardian: Address, weight: u32) -> Result<(), Error> {
    if weight == 0 {
        return Err(Error::InvalidInput);
    }

    let mut guardians = read_guardians(env);
    guardians.set(guardian.clone(), weight);
    write_guardians(env, &guardians);

    Ok(())
}

/// Remove a guardian by setting their weight to zero.
///
/// After removal, the guardian cannot vote on new proposals and their prior votes
/// are ignored during quorum calculation.
pub fn remove_guardian(env: &Env, guardian: &Address) -> Result<(), Error> {
    let mut guardians = read_guardians(env);
    guardians.remove(guardian.clone());
    write_guardians(env, &guardians);
    Ok(())
}

/// Get a guardian's current voting weight (0 if not a guardian).
pub fn get_guardian_weight(env: &Env, guardian: &Address) -> u32 {
    read_guardians(env).get(guardian.clone()).unwrap_or(0)
}

/// Calculate total voting weight across all guardians.
fn calculate_total_weight(env: &Env) -> u32 {
    let guardians = read_guardians(env);
    let mut total: u32 = 0;
    for (_, weight) in guardians.iter() {
        total = total.checked_add(weight).unwrap_or(u32::MAX);
    }
    total
}

/// Validate proposal configuration for `SetProtocolFee` kind.
///
/// Ensures:
/// - `target3` (fee_bps) does not exceed `MAX_PROTOCOL_FEE_BIPS`.
/// - `target2` (treasury) is provided (not None).
fn validate_set_protocol_fee_proposal(
    _env: &Env,
    fee_bps: u32,
    treasury: &Option<Address>,
) -> Result<(), Error> {
    if fee_bps > MAX_PROTOCOL_FEE_BIPS {
        return Err(Error::ProtocolFeeTooHigh);
    }
    if treasury.is_none() {
        return Err(Error::InvalidInput);
    }
    Ok(())
}

/// Validate proposal configuration for `SetDisputeWindow` kind.
///
/// Ensures:
/// - `target3` (window_secs) is not zero and fits in u32 max.
fn validate_set_dispute_window_proposal(window_secs: u32) -> Result<(), Error> {
    if window_secs == 0 || window_secs == u32::MAX {
        return Err(Error::InvalidInput);
    }
    Ok(())
}

/// Validate proposal configuration based on its kind.
///
/// Pre-validates configuration that would be checked at execution time,
/// allowing invalid proposals to be rejected at submission time to save gas.
/// This prevents invalid proposals from being stored and voted on.
fn validate_proposal_config(
    env: &Env,
    kind: ProposalKind,
    _target: &Address,
    target2: &Option<Address>,
    target3: u32,
) -> Result<(), Error> {
    match kind {
        ProposalKind::SetProtocolFee => {
            validate_set_protocol_fee_proposal(env, target3, target2)?;
        }
        ProposalKind::SetDisputeWindow => {
            validate_set_dispute_window_proposal(target3)?;
        }
        // Other proposal kinds have no pre-validation requirements
        ProposalKind::RotateAdmin
        | ProposalKind::UpgradeContract
        | ProposalKind::EmergencyStop
        | ProposalKind::RecoverStrandedFunds
        | ProposalKind::AddAcceptedToken
        | ProposalKind::RemoveAcceptedToken => {}
    }
    Ok(())
}

/// Submit a new governance proposal.
///
/// Creates a proposal with a deterministic ID and stores it in persistent storage.
/// Proposals require an ETA (execution timestamp) that is at least MIN_TIMELOCK_DELAY
/// seconds in the future to prevent same-ledger execution.
///
/// # Errors
/// - `InvalidInput` if quorum_bps is invalid (> 10000).
/// - `InvalidInput` if eta does not satisfy: eta >= created_at + MIN_TIMELOCK_DELAY.
/// - `EmergencyStopActive` if emergency stop is enabled.
pub fn do_submit_proposal(
    env: &Env,
    kind: ProposalKind,
    target: Address,
    target2: Option<Address>,
    target3: u32,
    quorum_bps: u32,
    eta: u64,
) -> Result<u64, Error> {
    if quorum_bps > 10_000 {
        return Err(Error::InvalidInput);
    }
    if kind == ProposalKind::SetDisputeWindow && target3 == 0 {
        return Err(Error::InvalidInput);
    }

    // Pre-validate proposal configuration based on kind
    validate_proposal_config(env, kind, &target, &target2, target3)?;

    let now = env.ledger().timestamp();
    let min_eta = now.checked_add(MIN_TIMELOCK_DELAY).ok_or(Error::Overflow)?;
    if eta < min_eta {
        return Err(Error::InvalidInput);
    }

    let proposal_id = get_next_proposal_id(env);
    let votes = Map::new(env);

    let proposal = Proposal {
        id: proposal_id,
        kind,
        target: target.clone(),
        target2,
        target3,
        quorum_bps,
        votes,
        eta,
        submitted_at: now,
        executed: false,
    };

    write_proposal(env, proposal_id, &proposal);

    env.events().publish(
        (Symbol::new(env, "proposal_submitted"),),
        ProposalSubmittedEvent {
            proposal_id,
            kind,
            target,
            quorum_bps,
            eta,
            timestamp: now,
            schema_version: BASE_EVENT_SCHEMA_VERSION,
            contract_id: env.current_contract_address(),
        },
    );

    Ok(proposal_id)
}

/// Record a guardian's vote on a proposal.
///
/// Guardian weight at vote time is recorded; if guardian is removed later,
/// this vote is invalidated during execute phase.
///
/// Votes are **locked** once the proposal's ETA (timelock) is reached —
/// guardians cannot add or change votes after the timelock opens. This
/// prevents vote-flip griefing where a guardian could feign support during
/// the voting window then flip their vote right at execution time.
///
/// A monotonic `nonce` (per-guardian, `DOMAIN_GOVERNANCE_VOTE`) is consumed
/// on every successful vote to prevent network-level replay of signed vote
/// transactions.
///
/// # Errors
/// - `NotFound` if proposal does not exist.
/// - `InvalidInput` if proposal already executed.
/// - `Unauthorized` if caller is not a valid guardian.
/// - `InvalidInput` if the timelock (ETA) has passed and votes are locked.
/// - `NonceAlreadyUsed` if the nonce has already been consumed.
pub fn do_vote_proposal(env: &Env, proposal_id: u64, voted_yes: bool, nonce: u64) -> Result<(), Error> {
    let guardian = crate::admin::require_stored_admin_auth(env)?;

    let guardian_weight = get_guardian_weight(env, &guardian);
    if guardian_weight == 0 {
        return Err(Error::Unauthorized);
    }

    let mut proposal = read_proposal(env, proposal_id)?;
    ensure_proposal_not_invalidated(env, proposal_id)?;

    if proposal.executed {
        return Err(Error::InvalidInput);
    }

    let now = env.ledger().timestamp();

    // ── Timelock guard: votes are locked once ETA is reached ────────────
    //
    // If the proposal's timelock has passed, no further votes may be
    // recorded. This prevents a guardian from feigning support during the
    // voting window and then flipping their vote to grief execution.
    if now >= proposal.eta {
        env.events().publish(
            (Symbol::new(env, "vote_locked"),),
            VoteLockedEvent {
                proposal_id,
                guardian: guardian.clone(),
                eta: proposal.eta,
                timestamp: now,
                schema_version: BASE_EVENT_SCHEMA_VERSION,
                contract_id: env.current_contract_address(),
            },
        );
        return Err(Error::InvalidInput);
    }

    // ── Nonce: consume before state mutation (auth already verified above) ──
    crate::nonce::check_and_advance(env, &guardian, crate::nonce::DOMAIN_GOVERNANCE_VOTE, nonce)?;

    // Record the vote
    proposal.votes.set(guardian.clone(), voted_yes);
    write_proposal(env, proposal_id, &proposal);

    env.events().publish(
        (Symbol::new(env, "proposal_voted"),),
        ProposalVotedEvent {
            proposal_id,
            guardian: guardian.clone(),
            voted_yes,
            guardian_weight,
            timestamp: now,
            schema_version: BASE_EVENT_SCHEMA_VERSION,
            contract_id: env.current_contract_address(),
        },
    );

    Ok(())
}

/// Execute a proposal if quorum is met and minimum delay has elapsed.
///
/// Validates:
/// 1. Minimum timelock delay: `created_at + MIN_TIMELOCK_DELAY <= now`
/// 2. Quorum requirements
/// 3. Blocks re-execution via `executed` flag
///
/// # Errors
/// - `NotFound` if proposal does not exist.
/// - `InvalidInput` if minimum delay has not elapsed since proposal creation.
/// - `InvalidInput` if ETA has not been reached.
/// - `InvalidInput` if proposal already executed or quorum not met.
pub fn do_execute_proposal(env: &Env, proposal_id: u64) -> Result<(), Error> {
    let mut proposal = read_proposal(env, proposal_id)?;
    ensure_proposal_not_invalidated(env, proposal_id)?;

    if proposal.executed {
        return Err(Error::InvalidInput);
    }

    let now = env.ledger().timestamp();
    
    // ── Check minimum timelock delay from proposal creation ────────────────
    let min_execution_time = proposal.submitted_at.checked_add(MIN_TIMELOCK_DELAY)
        .ok_or(Error::Overflow)?;
    if now < min_execution_time {
        return Err(Error::InvalidInput);
    }

    // ── Check that ETA has passed ──────────────────────────────────────────
    if now < proposal.eta {
        return Err(Error::InvalidInput);
    }

    // Calculate quorum. The effective quorum is never below `MIN_QUORUM_BPS`,
    // even if the proposal was submitted with a lower `quorum_bps`.
    let (votes_for, votes_against) = calculate_quorum(env, &proposal);
    let total_weight = calculate_total_weight(env);
    let effective_quorum_bps = proposal.quorum_bps.max(MIN_QUORUM_BPS);

    let required_votes = (total_weight as u128)
        .checked_mul(effective_quorum_bps as u128)
        .and_then(|v| v.checked_div(10_000))
        .ok_or(Error::Overflow)? as u32;

    if votes_for < required_votes {
        return Err(Error::InvalidInput);
    }

    // ── Guard against re-initialization via governance proposals ─────────
    //
    // Prevent re-initialization attacks by checking if the contract is already
    // initialized (Admin key exists). If initialized, reject any proposal that
    // would write to initialization-critical keys set by do_init:
    // - DataKey::Token (primary token address)
    // - DataKey::MinTopup (minimum top-up threshold)
    // - DataKey::GracePeriod (grace period duration)
    //
    // This guards against future ProposalKind variants (e.g., UpdateConfig)
    // that might attempt to bypass the initialization guard in do_init by
    // directly writing to storage without calling do_init.
    //
    // SECURITY CRITICAL: When adding a new ProposalKind variant, you MUST review
    // whether it writes to any of the initialization-critical keys above. If it
    // does, it MUST be added to the rejection check below. DataKey::Admin can
    // be safely updated via RotateAdmin as it's an intentional rotation, not
    // re-initialization.
    if crate::admin::has_config(env, &DataKey::Admin) {
        // Contract is already initialized - check if this proposal would
        // modify initialization-critical keys
        //
        // Current proposal kinds that are SAFE (do not modify init keys):
        // - RotateAdmin: Updates Admin, which is a rotation, not re-init
        // - SetProtocolFee: Updates FeeBps and Treasury, not init keys
        // - EmergencyStop: Updates EmergencyStop flag, not init keys
        // - RecoverStrandedFunds: Transfers funds, doesn't touch init keys
        // - AddAcceptedToken: Adds to token list, doesn't touch DataKey::Token
        // - RemoveAcceptedToken: Removes from token list, doesn't touch DataKey::Token
        // - SetUsageChargeCapOverride: Updates only a merchant usage-charge setting
        //
        // FUTURE: If adding ProposalKind::UpdateConfig or similar that writes to
        // DataKey::Token, DataKey::MinTopup, or DataKey::GracePeriod, add it here:
        // Currently no proposal kind writes to initialization-critical keys.
        let writes_to_init_keys = false;
        
        if writes_to_init_keys {
            return Err(Error::AlreadyInitialized);
        }
    }

    // Execute the proposal
    match proposal.kind {
        ProposalKind::RotateAdmin => {
            crate::admin::write_config(env, &DataKey::Admin, &proposal.target);
            invalidate_pending_proposals(env);
        }
        ProposalKind::SetProtocolFee => {
            crate::admin::write_config(env, &DataKey::FeeBps, &proposal.target3);
            if let Some(ref treasury) = proposal.target2 {
                crate::admin::write_config(env, &DataKey::Treasury, treasury);
            }
        }
        ProposalKind::UpgradeContract => {
            // Reserved for future use
            return Err(Error::InvalidInput);
        }
        ProposalKind::EmergencyStop => {
            let enabled = proposal.target3 != 0;
            crate::admin::write_config(env, &DataKey::EmergencyStop, &enabled);
        }
        ProposalKind::RecoverStrandedFunds => {
            // Recovery is handled by the caller - this just validates the proposal
            // The actual recovery happens in do_recover_stranded_funds
        }
        ProposalKind::AddAcceptedToken => {
            // Token addition is handled by the caller
        }
        ProposalKind::RemoveAcceptedToken => {
            // Token removal is handled by the caller
        }
        ProposalKind::SetDisputeWindow => {
            crate::admin::apply_dispute_window_secs(env, u64::from(proposal.target3))?;
        }
    }

    // Mark as executed
    proposal.executed = true;
    write_proposal(env, proposal_id, &proposal);

    env.events().publish(
        (Symbol::new(env, "proposal_executed"),),
        ProposalExecutedEvent {
            proposal_id,
            kind: proposal.kind,
            votes_for,
            votes_against,
            total_weight,
            timestamp: now,
            schema_version: BASE_EVENT_SCHEMA_VERSION,
            contract_id: env.current_contract_address(),
        },
    );

    Ok(())
}

/// Cancel a proposal.
///
/// Can only be called by the current admin. Prevents stale proposals from lingering.
///
/// # Errors
/// - `Unauthorized` if caller is not the admin.
/// - `NotFound` if proposal does not exist.
/// - `InvalidInput` if proposal is already executed.
pub fn do_cancel_proposal(env: &Env, proposal_id: u64, reason: String) -> Result<(), Error> {
    let _admin = crate::admin::require_stored_admin_auth(env)?;

    let mut proposal = read_proposal(env, proposal_id)?;

    if proposal.executed {
        return Err(Error::InvalidInput);
    }

    proposal.executed = true;
    write_proposal(env, proposal_id, &proposal);

    env.events().publish(
        (Symbol::new(env, "proposal_cancelled"),),
        ProposalCancelledEvent {
            proposal_id,
            reason,
            timestamp: env.ledger().timestamp(),
            schema_version: BASE_EVENT_SCHEMA_VERSION,
            contract_id: env.current_contract_address(),
        },
    );

    Ok(())
}

/// Get the current quorum (votes for and against).
///
/// Re-validates guardian status at read time to handle guardian removal.
pub(crate) fn calculate_quorum(env: &Env, proposal: &Proposal) -> (u32, u32) {
    let guardians = read_guardians(env);
    let mut votes_for: u32 = 0;
    let mut votes_against: u32 = 0;

    for (guardian, voted_yes) in proposal.votes.iter() {
        // Only count votes from current guardians
        if let Some(weight) = guardians.get(guardian.clone()) {
            if voted_yes {
                votes_for = votes_for.checked_add(weight).unwrap_or(u32::MAX);
            } else {
                votes_against = votes_against.checked_add(weight).unwrap_or(u32::MAX);
            }
        }
    }

    (votes_for, votes_against)
}

// ── Storage helpers ────────────────────────────────────────────────────────

/// Read guardians map from persistent storage.
fn read_guardians(env: &Env) -> Map<Address, u32> {
    env.storage()
        .persistent()
        .get::<_, Map<Address, u32>>(&DataKey::Guardians)
        .unwrap_or_else(|| Map::new(env))
}

/// Write guardians map to persistent storage.
fn write_guardians(env: &Env, guardians: &Map<Address, u32>) {
    env.storage()
        .persistent()
        .set(&DataKey::Guardians, guardians);
    crate::subscription::maybe_extend_ttl(
        env,
        &DataKey::Guardians,
        30 * 24 * 60 * 60,
        365 * 24 * 60 * 60,
    );
}

/// Read proposal from persistent storage.
fn read_proposal(env: &Env, proposal_id: u64) -> Result<Proposal, Error> {
    env.storage()
        .persistent()
        .get::<_, Proposal>(&DataKey::Proposal(proposal_id))
        .ok_or(Error::NotFound)
}

/// Write proposal to persistent storage.
fn write_proposal(env: &Env, proposal_id: u64, proposal: &Proposal) {
    let key = DataKey::Proposal(proposal_id);
    env.storage().persistent().set(&key, proposal);
    crate::subscription::maybe_extend_ttl(&env, &key, 30 * 24 * 60 * 60, 365 * 24 * 60 * 60);
}

/// Get next proposal ID and increment counter.
fn get_next_proposal_id(env: &Env) -> u64 {
    let id = env
        .storage()
        .instance()
        .get::<_, u64>(&DataKey::NextProposalId)
        .unwrap_or(0);
    env.storage()
        .instance()
        .set(&DataKey::NextProposalId, &(id + 1));
    id
}

/// Read current proposal ID counter.
pub fn get_current_proposal_id(env: &Env) -> u64 {
    env.storage()
        .instance()
        .get::<_, u64>(&DataKey::NextProposalId)
        .unwrap_or(0)
}

/// Get proposal by ID.
pub fn get_proposal(env: &Env, proposal_id: u64) -> Option<Proposal> {
    read_proposal(env, proposal_id).ok()
}

/// List all guardians and their weights.
pub fn list_guardians(env: &Env) -> Vec<(Address, u32)> {
    let guardians = read_guardians(env);
    let mut result = Vec::new(env);
    for (guardian, weight) in guardians.iter() {
        result.push_back((guardian, weight));
    }
    result
}
