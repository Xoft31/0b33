#![cfg(test)]

use crate::types::MAX_PROTOCOL_FEE_BIPS;
use crate::{Error, ProposalKind, SubscriptionVault, SubscriptionVaultClient};
use soroban_sdk::{
	testutils::{Address as _, Ledger as _},
	Address, Env,
};

#[test]
fn admin_rotation_invalidates_old_governance_proposals() {
	let env = Env::default();
	env.mock_all_auths();
	let contract_id = env.register(SubscriptionVault, ());
	let client = SubscriptionVaultClient::new(&env, &contract_id);
	let admin = Address::generate(&env);
	let token = env
		.register_stellar_asset_contract_v2(admin.clone())
		.address();
	client.init(&token, &6, &admin, &1_000_000i128, &(7 * 24 * 60 * 60));

	env.ledger().set_timestamp(1_000_000);
	client.add_guardian(&admin, &admin, &1u32);

	let old_eta = 1_000_000 + 3 * 24 * 60 * 60;
	let stale_id = client.submit_proposal(
		&ProposalKind::EmergencyStop,
		&admin,
		&None::<Address>,
		&1u32,
		&5_000u32,
		&old_eta,
	);
	client.vote_proposal(&stale_id, &true, &0u64);

	let new_admin = Address::generate(&env);
	client.propose_admin(&admin, &new_admin);
	env.ledger().set_timestamp(1_000_000 + 24 * 60 * 60);
	client.claim_admin_role(&new_admin);
	client.add_guardian(&new_admin, &new_admin, &1u32);

	assert_eq!(
		client.try_vote_proposal(&stale_id, &true, &0u64),
		Err(Ok(Error::ProposalInvalidated))
	);

	let fresh_eta = 1_000_000 + 3 * 24 * 60 * 60;
	let fresh_id = client.submit_proposal(
		&ProposalKind::EmergencyStop,
		&new_admin,
		&None::<Address>,
		&1u32,
		&5_000u32,
		&fresh_eta,
	);
	client.vote_proposal(&fresh_id, &true, &0u64);

	env.ledger().set_timestamp(old_eta);
	assert_eq!(
		client.try_execute_proposal(&stale_id),
		Err(Ok(Error::ProposalInvalidated))
	);
	assert_eq!(client.try_execute_proposal(&fresh_id), Ok(Ok(())));
}

#[test]
fn direct_admin_rotation_invalidates_outstanding_proposals() {
	let env = Env::default();
	env.mock_all_auths();
	let contract_id = env.register(SubscriptionVault, ());
	let client = SubscriptionVaultClient::new(&env, &contract_id);
	let admin = Address::generate(&env);
	let token = env
		.register_stellar_asset_contract_v2(admin.clone())
		.address();
	client.init(&token, &6, &admin, &1_000_000i128, &(7 * 24 * 60 * 60));
	env.ledger().set_timestamp(1_000_000);
	client.add_guardian(&admin, &admin, &1u32);

	let eta = 1_000_000 + 3 * 24 * 60 * 60;
	let proposal_id = client.submit_proposal(
		&ProposalKind::EmergencyStop,
		&admin,
		&None::<Address>,
		&1u32,
		&5_000u32,
		&eta,
	);

	let new_admin = Address::generate(&env);
	let nonce = client.get_admin_nonce(&admin, &1u32);
	client.rotate_admin(&admin, &new_admin, &nonce);
	client.add_guardian(&new_admin, &new_admin, &1u32);

	assert_eq!(
		client.try_vote_proposal(&proposal_id, &true, &0u64),
		Err(Ok(Error::ProposalInvalidated))
	);
}

#[test]
fn dispute_window_can_be_changed_by_governance_proposal() {
	let env = Env::default();
	env.mock_all_auths();
	let contract_id = env.register(SubscriptionVault, ());
	let client = SubscriptionVaultClient::new(&env, &contract_id);
	let admin = Address::generate(&env);
	let token = env
		.register_stellar_asset_contract_v2(admin.clone())
		.address();
	client.init(&token, &6, &admin, &1_000_000i128, &(7 * 24 * 60 * 60));
	client.add_guardian(&admin, &admin, &1u32);

	let eta = env.ledger().timestamp() + 2 * 24 * 60 * 60;
	let proposal_id = client.submit_proposal(
		&ProposalKind::SetDisputeWindow,
		&admin,
		&None::<Address>,
		&3_600u32,
		&6_700u32,
		&eta,
	);
	client.vote_proposal(&proposal_id, &true, &0u64);

	env.ledger().set_timestamp(eta);
	client.execute_proposal(&proposal_id);

	assert_eq!(client.get_dispute_window_secs(), 3_600);
}

// ── Pre-validation tests: reject invalid configs at submission time ──────────

#[test]
fn set_protocol_fee_proposal_rejects_fee_too_high() {
	let env = Env::default();
	env.mock_all_auths();
	let contract_id = env.register(SubscriptionVault, ());
	let client = SubscriptionVaultClient::new(&env, &contract_id);
	let admin = Address::generate(&env);
	let token = env
		.register_stellar_asset_contract_v2(admin.clone())
		.address();
	client.init(&token, &6, &admin, &1_000_000i128, &(7 * 24 * 60 * 60));

	let eta = env.ledger().timestamp() + 3 * 24 * 60 * 60;
	let treasury = Address::generate(&env);

	// Fee bps exceeds MAX_PROTOCOL_FEE_BIPS (500)
	let result = client.try_submit_proposal(
		&ProposalKind::SetProtocolFee,
		&admin,
		&Some(treasury),
		&(MAX_PROTOCOL_FEE_BIPS + 1),
		&5_000u32,
		&eta,
	);

	assert_eq!(result, Err(Ok(Error::ProtocolFeeTooHigh)));
}

#[test]
fn set_protocol_fee_proposal_rejects_missing_treasury() {
	let env = Env::default();
	env.mock_all_auths();
	let contract_id = env.register(SubscriptionVault, ());
	let client = SubscriptionVaultClient::new(&env, &contract_id);
	let admin = Address::generate(&env);
	let token = env
		.register_stellar_asset_contract_v2(admin.clone())
		.address();
	client.init(&token, &6, &admin, &1_000_000i128, &(7 * 24 * 60 * 60));

	let eta = env.ledger().timestamp() + 3 * 24 * 60 * 60;

	// Valid fee bps but missing treasury
	let result = client.try_submit_proposal(
		&ProposalKind::SetProtocolFee,
		&admin,
		&None::<Address>,
		&250u32, // Valid fee (5%)
		&5_000u32,
		&eta,
	);

	assert_eq!(result, Err(Ok(Error::InvalidInput)));
}

#[test]
fn set_protocol_fee_proposal_accepts_valid_config() {
	let env = Env::default();
	env.mock_all_auths();
	let contract_id = env.register(SubscriptionVault, ());
	let client = SubscriptionVaultClient::new(&env, &contract_id);
	let admin = Address::generate(&env);
	let token = env
		.register_stellar_asset_contract_v2(admin.clone())
		.address();
	client.init(&token, &6, &admin, &1_000_000i128, &(7 * 24 * 60 * 60));

	let eta = env.ledger().timestamp() + 3 * 24 * 60 * 60;
	let treasury = Address::generate(&env);

	// Valid: fee bps at max and treasury provided
	let result = client.try_submit_proposal(
		&ProposalKind::SetProtocolFee,
		&admin,
		&Some(treasury),
		&MAX_PROTOCOL_FEE_BIPS,
		&5_000u32,
		&eta,
	);

	assert!(result.is_ok());
	let proposal_id = result.unwrap().unwrap();
	assert!(proposal_id > 0);
}

#[test]
fn set_dispute_window_proposal_rejects_zero_window() {
	let env = Env::default();
	env.mock_all_auths();
	let contract_id = env.register(SubscriptionVault, ());
	let client = SubscriptionVaultClient::new(&env, &contract_id);
	let admin = Address::generate(&env);
	let token = env
		.register_stellar_asset_contract_v2(admin.clone())
		.address();
	client.init(&token, &6, &admin, &1_000_000i128, &(7 * 24 * 60 * 60));

	let eta = env.ledger().timestamp() + 3 * 24 * 60 * 60;

	// Zero window_secs should be rejected
	let result = client.try_submit_proposal(
		&ProposalKind::SetDisputeWindow,
		&admin,
		&None::<Address>,
		&0u32,
		&5_000u32,
		&eta,
	);

	assert_eq!(result, Err(Ok(Error::InvalidInput)));
}

#[test]
fn set_dispute_window_proposal_rejects_max_u32_window() {
	let env = Env::default();
	env.mock_all_auths();
	let contract_id = env.register(SubscriptionVault, ());
	let client = SubscriptionVaultClient::new(&env, &contract_id);
	let admin = Address::generate(&env);
	let token = env
		.register_stellar_asset_contract_v2(admin.clone())
		.address();
	client.init(&token, &6, &admin, &1_000_000i128, &(7 * 24 * 60 * 60));

	let eta = env.ledger().timestamp() + 3 * 24 * 60 * 60;

	// u32::MAX window_secs should be rejected
	let result = client.try_submit_proposal(
		&ProposalKind::SetDisputeWindow,
		&admin,
		&None::<Address>,
		&u32::MAX,
		&5_000u32,
		&eta,
	);

	assert_eq!(result, Err(Ok(Error::InvalidInput)));
}

#[test]
fn set_dispute_window_proposal_accepts_valid_config() {
	let env = Env::default();
	env.mock_all_auths();
	let contract_id = env.register(SubscriptionVault, ());
	let client = SubscriptionVaultClient::new(&env, &contract_id);
	let admin = Address::generate(&env);
	let token = env
		.register_stellar_asset_contract_v2(admin.clone())
		.address();
	client.init(&token, &6, &admin, &1_000_000i128, &(7 * 24 * 60 * 60));

	let eta = env.ledger().timestamp() + 3 * 24 * 60 * 60;

	// Valid: window_secs > 0 and < u32::MAX
	let result = client.try_submit_proposal(
		&ProposalKind::SetDisputeWindow,
		&admin,
		&None::<Address>,
		&3_600u32,
		&5_000u32,
		&eta,
	);

	assert!(result.is_ok());
	let proposal_id = result.unwrap().unwrap();
	assert!(proposal_id > 0);
}

#[test]
fn valid_proposal_can_be_executed_after_validation_passes() {
	let env = Env::default();
	env.mock_all_auths();
	let contract_id = env.register(SubscriptionVault, ());
	let client = SubscriptionVaultClient::new(&env, &contract_id);
	let admin = Address::generate(&env);
	let token = env
		.register_stellar_asset_contract_v2(admin.clone())
		.address();
	client.init(&token, &6, &admin, &1_000_000i128, &(7 * 24 * 60 * 60));
	client.add_guardian(&admin, &admin, &1u32);

	// Submit valid SetDisputeWindow proposal
	let eta = env.ledger().timestamp() + 3 * 24 * 60 * 60;
	let proposal_id = client.submit_proposal(
		&ProposalKind::SetDisputeWindow,
		&admin,
		&None::<Address>,
		&7_200u32,
		&5_000u32,
		&eta,
	);

	// Vote on proposal
	client.vote_proposal(&proposal_id, &true, &0u64);

	// Advance to execution time
	env.ledger().set_timestamp(eta);

	// Execute should succeed
	assert!(client.try_execute_proposal(&proposal_id).is_ok());

	// Verify the change was applied
	assert_eq!(client.get_dispute_window_secs(), 7_200);
}
