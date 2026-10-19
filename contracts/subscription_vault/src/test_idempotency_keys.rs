use crate::{
    idempotency::{
        check_key, check_key_at, hash_idem_key, IdemRingBuffer, IDEM_HISTORY,
        IDEM_TTL_SECS,
    },
    ChargeExecutionResult, SubscriptionVault, SubscriptionVaultClient,
};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token, Address, BytesN, Env,
};

const AMOUNT: i128 = 10_000_000;
const INTERVAL: u64 = 86_400;
const DEPOSIT: i128 = 50_000_000;
const MIN_TOPUP: i128 = 1_000_000;

fn setup_test_env() -> (Env, SubscriptionVaultClient<'static>, Address) {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000_000);

    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    let token = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    client.init(&token, &6, &admin, &MIN_TOPUP, &(7 * 24 * 60 * 60));

    let token_admin = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    token_admin.mint(&contract_id, &1_000_000_000i128);

    (env, client, token)
}

fn create_and_fund_sub(
    env: &Env,
    client: &SubscriptionVaultClient,
    subscriber: &Address,
    merchant: &Address,
    token: &Address,
) -> u32 {
    let id = client.create_subscription(
        subscriber,
        merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
        &None::<u64>,
        &None::<u32>,
);

    let token_client = token::Client::new(env, token);
    if token_client.balance(subscriber) < DEPOSIT {
        token::StellarAssetClient::new(env, token).mint(subscriber, &(DEPOSIT * 2));
    }

    let none_key: Option<BytesN<32>> = None;
    client.deposit_funds(&id, &DEPOSIT, &none_key);
    env.ledger().set_timestamp(env.ledger().timestamp() + 1);

    id
}

fn make_key(env: &Env, val: u8) -> BytesN<32> {
    let mut arr = [0u8; 32];
    arr[31] = val;
    BytesN::from_array(env, &arr)
}

#[test]
fn test_charge_subscription_idempotent_replay() {
    let (env, client, token) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let id = create_and_fund_sub(&env, &client, &subscriber, &merchant, &token);

    env.ledger().set_timestamp(env.ledger().timestamp() + INTERVAL);

    let key = make_key(&env, 1);
    let r1 = client.charge_subscription(&id, &Some(key.clone()));
    assert_eq!(r1, ChargeExecutionResult::Charged);

    let r2 = client.charge_subscription(&id, &Some(key.clone()));
    assert_eq!(r2, ChargeExecutionResult::Charged);
}

#[test]
fn test_charge_subscription_different_keys_allowed() {
    let (env, client, token) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let id = create_and_fund_sub(&env, &client, &subscriber, &merchant, &token);

    env.ledger().set_timestamp(env.ledger().timestamp() + INTERVAL);

    let key1 = make_key(&env, 1);
    let r1 = client.charge_subscription(&id, &Some(key1));
    assert_eq!(r1, ChargeExecutionResult::Charged);

    env.ledger().set_timestamp(env.ledger().timestamp() + INTERVAL);

    let key2 = make_key(&env, 2);
    let r2 = client.charge_subscription(&id, &Some(key2));
    assert_eq!(r2, ChargeExecutionResult::Charged);
}

#[test]
fn test_charge_subscription_none_key_ok() {
    let (env, client, token) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let id = create_and_fund_sub(&env, &client, &subscriber, &merchant, &token);

    env.ledger().set_timestamp(env.ledger().timestamp() + INTERVAL);

    let none_key: Option<BytesN<32>> = None;
    let r = client.charge_subscription(&id, &none_key);
    assert_eq!(r, ChargeExecutionResult::Charged);
}

#[test]
fn test_deposit_funds_idempotent_replay() {
    let (env, client, token) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let id = create_and_fund_sub(&env, &client, &subscriber, &merchant, &token);

    let key = make_key(&env, 10);
    let extra = 5_000_000i128;
    let token_admin = token::StellarAssetClient::new(&env, &token);
    token_admin.mint(&subscriber, &extra);

    client.deposit_funds(&id, &extra, &Some(key.clone()));

    let sub = client.get_subscription(&id);
    assert_eq!(sub.prepaid_balance, DEPOSIT + extra);

    client.deposit_funds(&id, &extra, &Some(key.clone()));

    let sub2 = client.get_subscription(&id);
    assert_eq!(sub2.prepaid_balance, DEPOSIT + extra);
}

#[test]
fn test_deposit_funds_different_keys_allowed() {
    let (env, client, token) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let id = create_and_fund_sub(&env, &client, &subscriber, &merchant, &token);

    let key1 = make_key(&env, 20);
    let key2 = make_key(&env, 21);
    let token_admin = token::StellarAssetClient::new(&env, &token);
    token_admin.mint(&subscriber, &20_000_000i128);

    client.deposit_funds(&id, &10_000_000i128, &Some(key1));
    client.deposit_funds(&id, &10_000_000i128, &Some(key2));

    let sub = client.get_subscription(&id);
    assert_eq!(sub.prepaid_balance, DEPOSIT + 20_000_000i128);
}

#[test]
fn test_charge_one_off_idempotent_replay() {
    let (env, client, token) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let id = create_and_fund_sub(&env, &client, &subscriber, &merchant, &token);

    let key = make_key(&env, 30);
    let amount: i128 = 5_000_000;

    client.charge_one_off(&id, &merchant, &amount, &Some(key.clone()));
    client.charge_one_off(&id, &merchant, &amount, &Some(key.clone()));

    let sub = client.get_subscription(&id);
    assert_eq!(sub.prepaid_balance, DEPOSIT - amount);
}

#[test]
fn test_charge_one_off_different_keys_allowed() {
    let (env, client, token) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let id = create_and_fund_sub(&env, &client, &subscriber, &merchant, &token);

    let key1 = make_key(&env, 31);
    let key2 = make_key(&env, 32);

    client.charge_one_off(&id, &merchant, &1_000_000i128, &Some(key1));
    client.charge_one_off(&id, &merchant, &2_000_000i128, &Some(key2));

    let sub = client.get_subscription(&id);
    assert_eq!(sub.prepaid_balance, DEPOSIT - 3_000_000i128);
}

#[test]
fn test_same_raw_key_different_entrypoints_no_collision() {
    let (env, client, token) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let id = create_and_fund_sub(&env, &client, &subscriber, &merchant, &token);
    let token_admin = token::StellarAssetClient::new(&env, &token);
    token_admin.mint(&subscriber, &10_000_000i128);

    let key = make_key(&env, 99);

    client.charge_one_off(&id, &merchant, &1_000_000i128, &Some(key.clone()));
    client.deposit_funds(&id, &5_000_000i128, &Some(key.clone()));

    env.ledger().set_timestamp(env.ledger().timestamp() + INTERVAL);
    client.charge_subscription(&id, &Some(key.clone()));
}

#[test]
fn test_ring_buffer_evicts_oldest_key() {
    let (env, client, token) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let id = create_and_fund_sub(&env, &client, &subscriber, &merchant, &token);
    let token_admin = token::StellarAssetClient::new(&env, &token);
    token_admin.mint(&subscriber, &1_000_000_000i128);

    // Insert 33 unique keys to fill buffer (32) + evict oldest (key 0)
    for i in 0..33u8 {
        let key = make_key(&env, i);
        token_admin.mint(&subscriber, &MIN_TOPUP);
        client.deposit_funds(&id, &MIN_TOPUP, &Some(key));
    }

    // Buffer now holds [32, 1, 2, 3, ..., 31], cursor = 1.
    // Key 0 was evicted (overwritten by key 32 at index 0).

    let balance_before = client.get_subscription(&id).prepaid_balance;

    // Key 1 is still present → idempotent no-op (balance unchanged)
    let key1 = make_key(&env, 1);
    client.deposit_funds(&id, &MIN_TOPUP, &Some(key1));
    assert_eq!(
        client.get_subscription(&id).prepaid_balance,
        balance_before,
        "key 1 should be idempotent (no balance change)"
    );

    // Key 0 was evicted → fresh deposit (balance increases)
    let key0 = make_key(&env, 0);
    token_admin.mint(&subscriber, &MIN_TOPUP);
    client.deposit_funds(&id, &MIN_TOPUP, &Some(key0));
    assert_eq!(
        client.get_subscription(&id).prepaid_balance,
        balance_before + MIN_TOPUP,
        "key 0 should be a fresh deposit"
    );
}

/// Ring-buffer capacity and TTL-driven pruning test.
///
/// Validates that:
///
/// * Keys within the TTL window are rejected (idempotent no-ops).
/// * When all `IDEM_HISTORY` slots are live, a new unique key is rejected
///   with `IdemBufferFull` rather than silently evicting the oldest live slot.
/// * After advancing past `IDEM_TTL_SECS`, expired entries are pruned on the
///   next write and new keys are accepted again.
/// * Duplicate insertions within the live window remain idempotent.
#[test]
fn test_idem_ring_wraparound_preserves_rejection_semantics() {
    let (env, client, token) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let id = create_and_fund_sub(&env, &client, &subscriber, &merchant, &token);
    let token_admin = token::StellarAssetClient::new(&env, &token);
    token_admin.mint(&subscriber, &1_000_000_000i128);

    let domain: u32 = 0; // deposit_funds domain
    let subscription_id = id;

    let extra_per = 500_000i128;
    let mut seen_hashes: Vec<BytesN<32>> = soroban_sdk::Vec::new(&env);

    // ── Phase 1: fill the buffer to exactly IDEM_HISTORY ────────────
    for i in 0..IDEM_HISTORY {
        let raw = make_key(&env, i as u8);
        let hashed = hash_idem_key(&env, domain, subscription_id, &raw);
        seen_hashes.push_back(hashed.clone());

        token_admin.mint(&subscriber, &extra_per);
        let bal_before = client.get_subscription(&id).prepaid_balance;
        let r = client.deposit_funds(&id, &extra_per, &Some(raw));
        assert_eq!(r, ChargeExecutionResult::Charged, "insert {i}: must succeed");

        let bal_after = client.get_subscription(&id).prepaid_balance;
        assert_eq!(
            bal_after,
            bal_before + extra_per,
            "insert {i}: balance must increase"
        );
    }

    // ── Phase 2: overflow must return IdemBufferFull (no eviction) ──
    let overflow_raw = make_key(&env, 0xFF);
    token_admin.mint(&subscriber, &extra_per);
    let result = client.try_deposit_funds(&id, &extra_per, &Some(overflow_raw));
    assert_eq!(
        result,
        Err(Ok(crate::Error::IdemBufferFull)),
        "65th unique key with all slots live must return IdemBufferFull"
    );

    // ── Phase 3: freshest hash must still be rejected (idempotent) ──
    let freshest_hash = seen_hashes.get(IDEM_HISTORY - 1).unwrap();
    let freshest_raw = make_key(&env, (IDEM_HISTORY - 1) as u8);
    let bal_before = client.get_subscription(&id).prepaid_balance;
    client.deposit_funds(&id, &extra_per, &Some(freshest_raw));
    let bal_after = client.get_subscription(&id).prepaid_balance;
    assert_eq!(
        bal_before, bal_after,
        "freshest hash must be rejected (idempotent)"
    );
    assert!(
        check_key(&env, subscription_id, &freshest_hash),
        "check_key must recognise the freshest hash"
    );

    // ── Phase 4: all slots remain live — every key still rejected ───
    for idx in 0..IDEM_HISTORY {
        let raw = make_key(&env, idx as u8);
        let hashed = hash_idem_key(&env, domain, subscription_id, &raw);
        assert!(
            check_key(&env, subscription_id, &hashed),
            "hash {idx} should still live in the ring"
        );

        let bal_before = client.get_subscription(&id).prepaid_balance;
        client.deposit_funds(&id, &extra_per, &Some(raw));
        let bal_after = client.get_subscription(&id).prepaid_balance;
        assert_eq!(
            bal_before, bal_after,
            "live hash {idx} must be rejected (idempotent)"
        );
    }

    // ── Phase 5: after TTL expiry, new keys are accepted ────────────
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + IDEM_TTL_SECS);

    // The overflow key should now be accepted (all prior entries expired).
    let overflow_raw2 = make_key(&env, 0xFE);
    token_admin.mint(&subscriber, &extra_per);
    let bal_before = client.get_subscription(&id).prepaid_balance;
    let r = client.deposit_funds(&id, &extra_per, &Some(overflow_raw2));
    assert_eq!(r, ChargeExecutionResult::Charged, "key accepted after TTL expiry");
    assert_eq!(
        client.get_subscription(&id).prepaid_balance,
        bal_before + extra_per,
        "balance must increase after TTL expiry"
    );
}

/// Wraparound at exactly IDEM_HISTORY capacity.
///
/// Insert exactly `IDEM_HISTORY` keys, then one more to trigger the
/// first overwrite, and verify the overwritten slot is now free.
#[test]
fn test_idem_ring_exact_capacity_then_overwrite() {
    let (env, client, token) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let id = create_and_fund_sub(&env, &client, &subscriber, &merchant, &token);
    let token_admin = token::StellarAssetClient::new(&env, &token);
    token_admin.mint(&subscriber, &1_000_000_000i128);

    let domain: u32 = 0;
    let extra = 500_000i128;

    // Fill to exactly IDEM_HISTORY.
    for i in 0..IDEM_HISTORY {
        let raw = make_key(&env, i as u8);
        token_admin.mint(&subscriber, &extra);
        client.deposit_funds(&id, &extra, &Some(raw));
    }

    // All IDEM_HISTORY slots are occupied – re-inserting any of them
    // must be idempotent.
    let mid = IDEM_HISTORY / 2;
    let mid_raw = make_key(&env, mid as u8);
    let bal_before = client.get_subscription(&id).prepaid_balance;
    client.deposit_funds(&id, &extra, &Some(mid_raw));
    assert_eq!(
        client.get_subscription(&id).prepaid_balance,
        bal_before,
        "mid-range hash must be idempotent at capacity"
    );

    // One more insert overwrites slot 0.
    let overwrite_raw = make_key(&env, 0xFF);
    token_admin.mint(&subscriber, &extra);
    let bal_before = client.get_subscription(&id).prepaid_balance;
    client.deposit_funds(&id, &extra, &Some(overwrite_raw));
    let bal_after = client.get_subscription(&id).prepaid_balance;
    assert_eq!(
        bal_after,
        bal_before + extra,
        "new hash must be accepted"
    );

    // Key 0 was overwritten – re-inserting it must be fresh.
    let key0_raw = make_key(&env, 0);
    let key0_hashed = hash_idem_key(&env, domain, id, &key0_raw);
    assert!(
        !check_key(&env, id, &key0_hashed),
        "key 0 must have been evicted"
    );
    let bal_before = client.get_subscription(&id).prepaid_balance;
    token_admin.mint(&subscriber, &extra);
    client.deposit_funds(&id, &extra, &Some(key0_raw));
    assert_eq!(
        client.get_subscription(&id).prepaid_balance,
        bal_before + extra,
        "re-inserting evicted key 0 must succeed"
    );
}

/// Time-based expiry: an entry older than the default `IDEM_TTL_SECS` must no longer
/// be considered a duplicate, even if the ring has not cycled.
///
/// This closes the ring-cycling attack (issue #13): an attacker cannot
/// replay an old hash simply by letting enough newer charges evict it —
/// the TTL provides an independent, time-bounded guard.
#[test]
fn test_idem_ttl_expires_old_entries() {
    let (env, client, token) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let id = create_and_fund_sub(&env, &client, &subscriber, &merchant, &token);
    let token_admin = token::StellarAssetClient::new(&env, &token);
    token_admin.mint(&subscriber, &1_000_000_000i128);

    let domain = crate::nonce::DOMAIN_DEPOSIT_FUNDS.as_u32();
    let extra = 500_000i128;

    // Insert a key at t=0 (relative to setup baseline).
    let raw = make_key(&env, 0xAB);
    let hashed = hash_idem_key(&env, domain, id, &raw);
    token_admin.mint(&subscriber, &extra);
    client.deposit_funds(&id, &extra, &Some(raw.clone()));

    // At insertion time the hash must be present.
    assert!(
        check_key(&env, id, &hashed),
        "hash must be live immediately after insertion"
    );

    // Advance time to just before TTL expiry — hash must still be active.
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + IDEM_TTL_SECS - 1);
    assert!(
        check_key_at(&env, id, &hashed, env.ledger().timestamp()),
        "hash must still be live one second before TTL expires"
    );

    // Advance past the TTL — hash must now be expired.
    env.ledger().set_timestamp(now + IDEM_TTL_SECS);
    assert!(
        !check_key_at(&env, id, &hashed, env.ledger().timestamp()),
        "hash must be expired once TTL has elapsed"
    );

    // Re-submitting the same key after expiry must be accepted as a new deposit.
    token_admin.mint(&subscriber, &extra);
    let bal_before = client.get_subscription(&id).prepaid_balance;
    client.deposit_funds(&id, &extra, &Some(raw));
    assert_eq!(
        client.get_subscription(&id).prepaid_balance,
        bal_before + extra,
        "expired key must be accepted as a fresh deposit"
    );
}

#[test]
fn test_idempotency_ttl_is_configurable_and_prunes_on_write() {
    let (env, client, token) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let id = create_and_fund_sub(&env, &client, &subscriber, &merchant, &token);
    let token_admin = token::StellarAssetClient::new(&env, &token);
    token_admin.mint(&subscriber, &2_000_000i128);

    assert_eq!(client.get_idempotency_ttl(), IDEM_TTL_SECS);
    let admin = client.get_admin();
    assert_eq!(
        client.try_set_idempotency_ttl(&admin, &0),
        Err(Ok(crate::Error::InvalidInput))
    );

    let configured_ttl = 10u64;
    client.set_idempotency_ttl(&admin, &configured_ttl);
    assert_eq!(client.get_idempotency_ttl(), configured_ttl);

    let domain = crate::nonce::DOMAIN_DEPOSIT_FUNDS.as_u32();
    let first_key = make_key(&env, 0xA1);
    let first_hash = hash_idem_key(&env, domain, id, &first_key);
    client.deposit_funds(&id, &500_000i128, &Some(first_key));

    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + configured_ttl);

    let second_key = make_key(&env, 0xA2);
    let second_hash = hash_idem_key(&env, domain, id, &second_key);
    client.deposit_funds(&id, &500_000i128, &Some(second_key));

    let buffer: IdemRingBuffer = env.as_contract(&client.address, || {
        env.storage()
            .instance()
            .get(&crate::types::DataKey::IdemKey(id))
            .unwrap()
    });
    assert_eq!(buffer.entries.len(), 1);
    assert_eq!(buffer.entries.get(0).unwrap().0, second_hash);
    assert!(!check_key(&env, id, &first_hash));
    assert!(check_key(&env, id, &second_hash));

    let attacker = Address::generate(&env);
    assert_eq!(
        client.try_set_idempotency_ttl(&attacker, &configured_ttl),
        Err(Ok(crate::Error::Forbidden))
    );
}

/// Verify that the default TTL + ring-size defence makes the cycling attack
/// infeasible: filling 64 slots within 7 days requires a charge every ~2.6
/// hours — far above any normal subscription billing cadence.
///
/// This is a documentation test; it asserts the constants rather than
/// exercising runtime behaviour.
#[test]
fn test_cycling_attack_infeasibility_constants() {
    // Seconds available before the oldest entry expires.
    let ttl_seconds = IDEM_TTL_SECS;
    // Number of charges an attacker must push through to evict a single entry.
    let slots = IDEM_HISTORY as u64;
    // Minimum seconds between charges to cycle the full ring within the TTL.
    let min_interval_secs = ttl_seconds / slots;

    // A minimum inter-charge interval of ~2.6 hours means the ring cannot
    // be cycled during the TTL window unless the subscription charges more
    // frequently than once every ~2.6 hours — which is not a valid billing
    // cadence for any subscription product.
    assert!(
        min_interval_secs >= 3 * 60 * 60, // 3 hours
        "cycling the ring within TTL requires charges more frequent than 3 h \
         (actual minimum interval: {} s) — tighten IDEM_HISTORY or IDEM_TTL_SECS",
        min_interval_secs
    );
}

/// Verify that charge-path idempotency keys and recovery operation keys occupy
/// entirely separate storage namespaces, so a raw key value shared between a
/// charge call and a recovery call can never cause a false replay.
///
/// ## What is being tested
///
/// Charge-path keys are stored in `DataKey::IdemKey(subscription_id)` as entries
/// in a ring buffer, keyed by `SHA256(domain || subscription_id || raw_key)`.
/// Recovery keys are stored under `DataKey::Recovery(recovery_id)` as a plain
/// boolean flag, where `recovery_id` is a caller-supplied `String`.
///
/// The two key types are:
/// - Different `DataKey` variants (different on-chain discriminants)
/// - Different value types (`IdemRingBuffer` vs `bool`)
/// - Different lookup paths (ring-buffer scan vs direct key presence check)
///
/// This test asserts that:
/// 1. Consuming a charge idem key does NOT poison the recovery namespace — the
///    same 32-byte value used for `charge_subscription` can still be passed as
///    a `recovery_id` string without triggering `Error::Replay`.
/// 2. Consuming a recovery ID does NOT poison the charge-path ring buffer — the
///    same string value can still be used as a raw charge idem key without being
///    treated as a duplicate.
/// 3. The hash stored in the ring buffer for the charge path differs from what
///    the recovery path stores (confirmed via `hash_idem_key` and `check_key`).
#[test]
fn test_charge_and_recovery_keys_are_namespace_separated() {
    use soroban_sdk::String as SorobanString;
    use crate::{RecoveryReason, idempotency::hash_idem_key};

    let (env, client, token) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let id = create_and_fund_sub(&env, &client, &subscriber, &merchant, &token);

    // Mint stranded funds into the contract so recovery has something to send.
    token::StellarAssetClient::new(&env, &token).mint(&client.address, &10_000_000i128);

    let recipient = Address::generate(&env);
    // Retrieve the admin address that was registered during init.
    let admin = client.get_admin().expect("admin must be set after init");

    // Pick a raw 32-byte value that we will use as BOTH a charge idem key AND
    // (its hex-like string representation) a recovery_id.
    let shared_raw = make_key(&env, 0xCC);

    // ── Step 1: use the raw value as a charge_subscription idem key ──────────
    env.ledger().set_timestamp(env.ledger().timestamp() + INTERVAL);
    let r = client.charge_subscription(&id, &Some(shared_raw.clone()));
    assert_eq!(r, ChargeExecutionResult::Charged, "charge must succeed");

    // The hash now lives in the charge-path ring buffer.
    let charge_domain = crate::nonce::DOMAIN_CHARGE_INTERVAL.as_u32();
    let charge_hash = hash_idem_key(&env, charge_domain, id, &shared_raw);
    assert!(
        check_key(&env, id, &charge_hash),
        "charge idem key must be present in the ring buffer after charge"
    );

    // ── Step 2: use the same bytes as a recovery_id — must NOT be blocked ────
    // recovery_id is a Soroban String, not a BytesN<32>.  Even if its content
    // is identical to the raw key bytes, it is stored under DataKey::Recovery,
    // which is a completely different storage slot from DataKey::IdemKey.
    let recovery_id = SorobanString::from_str(&env, "0xCC_shared");
    let result = client.try_recover_stranded_funds(
        &admin,
        &token,
        &recipient,
        &1_000_000i128,
        &recovery_id,
        &RecoveryReason::UserOverpayment,
    );
    assert!(
        result.is_ok(),
        "recovery must NOT be blocked by a charge idem key with the same raw bytes: {result:?}"
    );

    // ── Step 3: use a recovery_id first, then use the same label as an idem key ─
    // A second subscription exercises the reverse direction.
    let id2 = create_and_fund_sub(&env, &client, &subscriber, &merchant, &token);

    let recovery_id2 = SorobanString::from_str(&env, "rec_shared_key");
    // Consume the recovery ID.
    client.try_recover_stranded_funds(
        &admin,
        &token,
        &recipient,
        &1_000_000i128,
        &recovery_id2,
        &RecoveryReason::FailedTransfer,
    ).expect("second recovery must succeed");

    // Now use a charge idem key on sub2 — must NOT be affected by the recovery key.
    env.ledger().set_timestamp(env.ledger().timestamp() + INTERVAL);
    let r2 = client.charge_subscription(&id2, &Some(shared_raw.clone()));
    assert_eq!(
        r2,
        ChargeExecutionResult::Charged,
        "charge on sub2 must not be blocked by a prior recovery with a similar label"
    );

    // ── Step 4: replay the charge idem key — must be idempotent (ring buffer) ─
    let r3 = client.charge_subscription(&id, &Some(shared_raw.clone()));
    assert_eq!(
        r3,
        ChargeExecutionResult::Charged,
        "replaying the charge idem key must be idempotent, not an error"
    );

    // ── Step 5: replay the recovery_id — must return Replay error ────────────
    let replay_result = client.try_recover_stranded_funds(
        &admin,
        &token,
        &recipient,
        &1_000_000i128,
        &recovery_id,
        &RecoveryReason::UserOverpayment,
    );
    assert_eq!(
        replay_result,
        Err(Ok(crate::Error::Replay)),
        "replaying the same recovery_id must return Error::Replay"
    );
}

/// When the idempotency ring is full of **live** entries (none have expired),
/// inserting a new unique key must be rejected with [`Error::IdemBufferFull`]
/// instead of silently evicting the oldest live slot.
///
/// ## What is being tested
///
/// 1. Fill the ring to exactly `IDEM_HISTORY` unique deposits, all within the
///    TTL window so no slot is eligible for pruning.
/// 2. Attempt one more deposit with a fresh key — the contract must return
///    `Error::IdemBufferFull`.
/// 3. Verify that every previously inserted key is still in the ring (no
///    silent eviction occurred).
/// 4. Advance time past `IDEM_TTL_SECS` so all entries expire, then confirm
///    the same fresh key is accepted and the balance increases.
#[test]
fn test_idem_buffer_full_error_when_all_slots_live() {
    let (env, client, token) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let id = create_and_fund_sub(&env, &client, &subscriber, &merchant, &token);
    let token_admin = token::StellarAssetClient::new(&env, &token);
    token_admin.mint(&subscriber, &1_000_000_000i128);

    let domain: u32 = 0; // deposit_funds domain
    let extra: i128 = 500_000;

    // ── Phase 1: fill every slot with a live key ─────────────────────────────
    for i in 0..IDEM_HISTORY {
        let raw = make_key(&env, i as u8);
        token_admin.mint(&subscriber, &extra);
        let r = client.deposit_funds(&id, &extra, &Some(raw));
        assert_eq!(r, ChargeExecutionResult::Charged, "insert {i}: must succeed");
    }

    // ── Phase 2: one more unique key must return IdemBufferFull ──────────────
    let overflow_raw = make_key(&env, 0xFF);
    token_admin.mint(&subscriber, &extra);
    let balance_before = client.get_subscription(&id).prepaid_balance;

    let result = client.try_deposit_funds(&id, &extra, &Some(overflow_raw.clone()));
    assert_eq!(
        result,
        Err(Ok(crate::Error::IdemBufferFull)),
        "a 65th unique key with all slots live must return IdemBufferFull"
    );

    // Balance must be unchanged — the deposit was rejected.
    assert_eq!(
        client.get_subscription(&id).prepaid_balance,
        balance_before,
        "balance must not change when IdemBufferFull is returned"
    );

    // ── Phase 3: all original slots must still be present (no eviction) ──────
    for i in 0..IDEM_HISTORY {
        let raw = make_key(&env, i as u8);
        let hashed = hash_idem_key(&env, domain, id, &raw);
        assert!(
            check_key(&env, id, &hashed),
            "slot {i} must still be live after the failed overflow insert"
        );
    }

    // ── Phase 4: after TTL expiry the fresh key must be accepted ─────────────
    let now = env.ledger().timestamp();
    env.ledger().set_timestamp(now + IDEM_TTL_SECS);

    token_admin.mint(&subscriber, &extra);
    let balance_before = client.get_subscription(&id).prepaid_balance;
    let r = client.deposit_funds(&id, &extra, &Some(overflow_raw));
    assert_eq!(
        r,
        ChargeExecutionResult::Charged,
        "fresh key must be accepted once all prior entries have expired"
    );
    assert_eq!(
        client.get_subscription(&id).prepaid_balance,
        balance_before + extra,
        "balance must increase after the expired-then-accepted deposit"
    );
}
