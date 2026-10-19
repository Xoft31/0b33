#![cfg(test)]

extern crate std;

use crate::test_utils::setup::TestEnv;
use crate::types::{DataKey, Error, SchemaMigratedEvent};
use soroban_sdk::{
    testutils::{Address as _, Events, Ledger as _},
    Address, Env, IntoVal,
};

#[test]
fn test_fresh_init_stores_in_persistent() {
    let te = TestEnv::default();
    te.env.as_contract(&te.client.address, || {
        let storage = te.env.storage();
        assert!(storage.persistent().has(&DataKey::Token));
        assert!(storage.persistent().has(&DataKey::Admin));
        assert!(storage.persistent().has(&DataKey::MinTopup));
        assert!(storage.persistent().has(&DataKey::GracePeriod));
        assert!(storage.persistent().has(&DataKey::SchemaVersion));

        assert!(!storage.instance().has(&DataKey::Token));
        assert!(!storage.instance().has(&DataKey::Admin));
        assert!(!storage.instance().has(&DataKey::MinTopup));
        assert!(!storage.instance().has(&DataKey::GracePeriod));
        assert!(!storage.instance().has(&DataKey::SchemaVersion));
    });
}

#[test]
fn test_grace_period_is_admin_config_persisted() {
    let te = TestEnv::default();

    te.env.as_contract(&te.client.address, || {
        let storage = te.env.storage();
        assert!(storage.persistent().has(&DataKey::GracePeriod));
        assert!(!storage.instance().has(&DataKey::GracePeriod));
    });

    te.env.mock_all_auths();
    te.client.set_grace_period(&te.admin, &86_400u64);
    assert_eq!(te.client.get_grace_period(), 86_400u64);

    te.env.as_contract(&te.client.address, || {
        let storage = te.env.storage();
        assert_eq!(
            storage.persistent().get::<_, u64>(&DataKey::GracePeriod),
            Some(86_400u64)
        );
        assert!(!storage.instance().has(&DataKey::GracePeriod));
    });
}

#[test]
fn test_fallback_reads_on_v2() {
    let env = Env::default();
    let contract_id = env.register(crate::SubscriptionVault, ());
    let client = crate::SubscriptionVaultClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let token = Address::generate(&env);
    let min_topup = 1_000_000i128;

    env.as_contract(&contract_id, || {
        let storage = env.storage();
        storage.instance().set(&DataKey::SchemaVersion, &2u32);
        storage.instance().set(&DataKey::Token, &token);
        storage.instance().set(&DataKey::Admin, &admin);
        storage.instance().set(&DataKey::MinTopup, &min_topup);
    });

    // Check fallback reads
    let stored_token = env.as_contract(&contract_id, || {
        crate::admin::read_config::<Address>(&env, &DataKey::Token).unwrap()
    });
    assert_eq!(stored_token, token);
    assert_eq!(client.get_min_topup(), min_topup);
    
    // Auth mocking
    env.mock_all_auths();
    // Setting min topup should work and verify auth using fallback admin read
    client.set_min_topup(&admin, &2_000_000i128);
    assert_eq!(client.get_min_topup(), 2_000_000i128);
}

#[test]
fn test_migration_moves_all_keys() {
    let env = Env::default();
    let contract_id = env.register(crate::SubscriptionVault, ());
    let client = crate::SubscriptionVaultClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let token = Address::generate(&env);
    let min_topup = 1_000_000i128;
    let next_id = 42u32;
    let emergency_stop = true;
    let treasury = Address::generate(&env);
    let fee_bps = 250u32;
    let operator = Address::generate(&env);
    let grace_period = 86_400u64;

    env.as_contract(&contract_id, || {
        let storage = env.storage();
        storage.instance().set(&DataKey::SchemaVersion, &2u32);
        storage.instance().set(&DataKey::Token, &token);
        storage.instance().set(&DataKey::Admin, &admin);
        storage.instance().set(&DataKey::MinTopup, &min_topup);
        storage.instance().set(&DataKey::GracePeriod, &grace_period);
        storage.instance().set(&DataKey::NextId, &next_id);
        storage.instance().set(&DataKey::EmergencyStop, &emergency_stop);
        storage.instance().set(&DataKey::Treasury, &treasury);
        storage.instance().set(&DataKey::FeeBps, &fee_bps);
        storage.instance().set(&DataKey::Operator, &operator);
    });

    env.mock_all_auths();
    env.ledger().with_mut(|li| li.timestamp = 5_000);
    
    // Call migrate_config_to_persistent
    client.migrate_config_to_persistent(&admin);

    // Verify events
    let events = env.events().all();
    let last = events.last().expect("no events");
    let payload: SchemaMigratedEvent = last.2.into_val(&env);
    assert_eq!(payload.admin, admin);
    assert_eq!(payload.from_version, 2);
    assert_eq!(payload.to_version, 3);
    assert_eq!(payload.timestamp, 5_000);

    // Verify that the keys are now in persistent storage and removed from instance storage
    env.as_contract(&contract_id, || {
        let storage = env.storage();
        
        // Persistent has them
        assert_eq!(storage.persistent().get::<_, u32>(&DataKey::SchemaVersion), Some(3u32));
        assert_eq!(storage.persistent().get::<_, Address>(&DataKey::Token), Some(token));
        assert_eq!(storage.persistent().get::<_, Address>(&DataKey::Admin), Some(admin.clone()));
        assert_eq!(storage.persistent().get::<_, i128>(&DataKey::MinTopup), Some(min_topup));
        assert_eq!(storage.persistent().get::<_, u64>(&DataKey::GracePeriod), Some(grace_period));
        assert_eq!(storage.persistent().get::<_, u32>(&DataKey::NextId), Some(next_id));
        assert_eq!(storage.persistent().get::<_, bool>(&DataKey::EmergencyStop), Some(emergency_stop));
        assert_eq!(storage.persistent().get::<_, Address>(&DataKey::Treasury), Some(treasury));
        assert_eq!(storage.persistent().get::<_, u32>(&DataKey::FeeBps), Some(fee_bps));
        assert_eq!(storage.persistent().get::<_, Address>(&DataKey::Operator), Some(operator));

        // Instance does NOT have them
        assert!(!storage.instance().has(&DataKey::SchemaVersion));
        assert!(!storage.instance().has(&DataKey::Token));
        assert!(!storage.instance().has(&DataKey::Admin));
        assert!(!storage.instance().has(&DataKey::MinTopup));
        assert!(!storage.instance().has(&DataKey::GracePeriod));
        assert!(!storage.instance().has(&DataKey::NextId));
        assert!(!storage.instance().has(&DataKey::EmergencyStop));
        assert!(!storage.instance().has(&DataKey::Treasury));
        assert!(!storage.instance().has(&DataKey::FeeBps));
        assert!(!storage.instance().has(&DataKey::Operator));
    });
}

#[test]
fn test_upgrade_via_migrate() {
    let env = Env::default();
    let contract_id = env.register(crate::SubscriptionVault, ());
    let client = crate::SubscriptionVaultClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let token = Address::generate(&env);
    let min_topup = 1_000_000i128;

    env.as_contract(&contract_id, || {
        let storage = env.storage();
        storage.instance().set(&DataKey::SchemaVersion, &2u32);
        storage.instance().set(&DataKey::Token, &token);
        storage.instance().set(&DataKey::Admin, &admin);
        storage.instance().set(&DataKey::MinTopup, &min_topup);
    });

    env.mock_all_auths();
    client.migrate(&admin, &false);

    env.as_contract(&contract_id, || {
        let storage = env.storage();
        assert_eq!(storage.persistent().get::<_, u32>(&DataKey::SchemaVersion), Some(3u32));
        assert_eq!(storage.persistent().get::<_, Address>(&DataKey::Token), Some(token));
        assert_eq!(storage.persistent().get::<_, Address>(&DataKey::Admin), Some(admin));
        assert_eq!(storage.persistent().get::<_, i128>(&DataKey::MinTopup), Some(min_topup));

        assert!(!storage.instance().has(&DataKey::SchemaVersion));
        assert!(!storage.instance().has(&DataKey::Token));
        assert!(!storage.instance().has(&DataKey::Admin));
        assert!(!storage.instance().has(&DataKey::MinTopup));
    });
}

#[test]
fn test_migration_idempotency_and_crash_recovery() {
    let env = Env::default();
    let contract_id = env.register(crate::SubscriptionVault, ());
    let client = crate::SubscriptionVaultClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let token = Address::generate(&env);
    let min_topup = 1_000_000i128;

    // Simulate mid-migration crash state:
    // Token is in persistent storage (migrated).
    // Admin and MinTopup are still in instance storage.
    env.as_contract(&contract_id, || {
        let storage = env.storage();
        storage.instance().set(&DataKey::SchemaVersion, &2u32);
        storage.persistent().set(&DataKey::Token, &token);
        storage.instance().set(&DataKey::Admin, &admin);
        storage.instance().set(&DataKey::MinTopup, &min_topup);
    });

    env.mock_all_auths();
    
    // First run (resuming from crash state)
    client.migrate_config_to_persistent(&admin);

    env.as_contract(&contract_id, || {
        let storage = env.storage();
        assert_eq!(storage.persistent().get::<_, u32>(&DataKey::SchemaVersion), Some(3u32));
        assert_eq!(storage.persistent().get::<_, Address>(&DataKey::Token), Some(token.clone()));
        assert_eq!(storage.persistent().get::<_, Address>(&DataKey::Admin), Some(admin.clone()));
        assert_eq!(storage.persistent().get::<_, i128>(&DataKey::MinTopup), Some(min_topup));

        assert!(!storage.instance().has(&DataKey::SchemaVersion));
        assert!(!storage.instance().has(&DataKey::Token));
        assert!(!storage.instance().has(&DataKey::Admin));
        assert!(!storage.instance().has(&DataKey::MinTopup));
    });

    // Second run (idempotency check: should be a safe no-op)
    client.migrate_config_to_persistent(&admin);

    env.as_contract(&contract_id, || {
        let storage = env.storage();
        assert_eq!(storage.persistent().get::<_, u32>(&DataKey::SchemaVersion), Some(3u32));
        assert_eq!(storage.persistent().get::<_, Address>(&DataKey::Token), Some(token));
        assert_eq!(storage.persistent().get::<_, Address>(&DataKey::Admin), Some(admin));
        assert_eq!(storage.persistent().get::<_, i128>(&DataKey::MinTopup), Some(min_topup));
    });
}

#[test]
fn test_rejection_of_schema_downgrades() {
    let env = Env::default();
    let contract_id = env.register(crate::SubscriptionVault, ());
    let client = crate::SubscriptionVaultClient::new(&env, &contract_id);
    let admin = Address::generate(&env);

    // Simulate version 4
    env.as_contract(&contract_id, || {
        env.storage().persistent().set(&DataKey::SchemaVersion, &4u32);
        env.storage().persistent().set(&DataKey::Admin, &admin);
    });

    env.mock_all_auths();
    
    // migrate_config_to_persistent should fail
    let err1 = client.try_migrate_config_to_persistent(&admin);
    assert_eq!(err1, Err(Ok(Error::SchemaMigrationDowngrade)));

    // migrate to version 3 should also fail (with SchemaVersionMismatch)
    let err2 = client.try_migrate(&admin, &false);
    assert_eq!(err2, Err(Ok(Error::SchemaVersionMismatch)));
}

#[test]
fn test_migrate_state_unchanged_on_version_far_above_expected() {
    let env = Env::default();
    let contract_id = env.register(crate::SubscriptionVault, ());
    let client = crate::SubscriptionVaultClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let token = Address::generate(&env);

    // Seed an expected stored version far above the binary version (e.g. 99)
    env.as_contract(&contract_id, || {
        let storage = env.storage();
        storage.persistent().set(&DataKey::SchemaVersion, &99u32);
        storage.persistent().set(&DataKey::Admin, &admin);
        storage.persistent().set(&DataKey::Token, &token);
    });
    env.mock_all_auths();

    // Verify initial state: version is 99
    env.as_contract(&contract_id, || {
        assert_eq!(
            env.storage().persistent().get::<_, u32>(&DataKey::SchemaVersion),
            Some(99u32)
        );
    });

    // Attempt to migrate — should fail with SchemaVersionMismatch
    let err = client.try_migrate(&admin, &false);
    assert_eq!(err, Err(Ok(Error::SchemaVersionMismatch)));

    // State must remain untouched
    env.as_contract(&contract_id, || {
        assert_eq!(
            env.storage().persistent().get::<_, u32>(&DataKey::SchemaVersion),
            Some(99u32)
        );
        assert_eq!(
            env.storage().persistent().get::<_, Address>(&DataKey::Admin),
            Some(admin)
        );
        assert_eq!(
            env.storage().persistent().get::<_, Address>(&DataKey::Token),
            Some(token)
        );
    });
}

// ══════════════════════════════════════════════════════════════════════════════
// V1 Storage Layout Backward Compatibility
//
// Migration tests above verify the forward migration path (v2 → v3). This test
// verifies the real production migration scenario: a v1-layout state (config in
// instance storage, no schema version key) is correctly read and migrated by the
// v3 binary.
//
// This test:
// 1. Deploys a v1-layout state (instance storage, no SchemaVersion key)
// 2. Runs the v3 binary against it
// 3. Verifies the v3 binary migrates v1 → v2 → v3 correctly
// 4. Verifies that attempting to run an older binary (v2) against v3 state
//    returns SchemaMigrationDowngrade (Error code 9101)
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_v1_state_migrates_to_v3_correctly() {
    let env = Env::default();
    let contract_id = env.register(crate::SubscriptionVault, ());
    let client = crate::SubscriptionVaultClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let token = Address::generate(&env);
    let min_topup = 1_000_000i128;
    let grace_period = 86_400u64;

    // Simulate v1 storage layout:
    // - No SchemaVersion key (v1 contracts did not have this key)
    // - All config in instance storage
    env.as_contract(&contract_id, || {
        let storage = env.storage();
        // V1 did not store SchemaVersion
        storage.instance().set(&DataKey::Token, &token);
        storage.instance().set(&DataKey::Admin, &admin);
        storage.instance().set(&DataKey::MinTopup, &min_topup);
        storage.instance().set(&DataKey::GracePeriod, &grace_period);
    });

    env.mock_all_auths();

    // Verify initial state: no schema version (v1 behavior)
    env.as_contract(&contract_id, || {
        assert!(!env.storage().instance().has(&DataKey::SchemaVersion));
        assert!(!env.storage().persistent().has(&DataKey::SchemaVersion));
    });

    // Call migrate — should upgrade v1 → v2 → v3
    client.migrate(&admin, &false);

    // Verify final state: schema version = 3, all config in persistent storage
    env.as_contract(&contract_id, || {
        let storage = env.storage();
        
        // Schema version is now 3 in persistent storage
        assert_eq!(
            storage.persistent().get::<_, u32>(&DataKey::SchemaVersion),
            Some(3u32)
        );
        
        // All config is in persistent storage
        assert_eq!(storage.persistent().get::<_, Address>(&DataKey::Token), Some(token.clone()));
        assert_eq!(storage.persistent().get::<_, Address>(&DataKey::Admin), Some(admin.clone()));
        assert_eq!(storage.persistent().get::<_, i128>(&DataKey::MinTopup), Some(min_topup));
        assert_eq!(storage.persistent().get::<_, u64>(&DataKey::GracePeriod), Some(grace_period));
        
        // Instance storage is clean
        assert!(!storage.instance().has(&DataKey::SchemaVersion));
        assert!(!storage.instance().has(&DataKey::Token));
        assert!(!storage.instance().has(&DataKey::Admin));
        assert!(!storage.instance().has(&DataKey::MinTopup));
        assert!(!storage.instance().has(&DataKey::GracePeriod));
    });

    // Verify that normal operations work after migration
    assert_eq!(client.get_min_topup(), min_topup);
    assert_eq!(client.get_grace_period(), grace_period);
    assert_eq!(client.get_admin(), admin);
}

#[test]
fn test_v1_state_with_partial_data_migrates_correctly() {
    let env = Env::default();
    let contract_id = env.register(crate::SubscriptionVault, ());
    let client = crate::SubscriptionVaultClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let token = Address::generate(&env);
    let min_topup = 500_000i128;

    // Simulate v1 storage with only required fields
    env.as_contract(&contract_id, || {
        let storage = env.storage();
        storage.instance().set(&DataKey::Token, &token);
        storage.instance().set(&DataKey::Admin, &admin);
        storage.instance().set(&DataKey::MinTopup, &min_topup);
        // GracePeriod not set (simulating minimal v1 state)
    });

    env.mock_all_auths();

    // Migrate should succeed even with partial data
    client.migrate(&admin, &false);

    // Verify migration completed
    env.as_contract(&contract_id, || {
        let storage = env.storage();
        assert_eq!(
            storage.persistent().get::<_, u32>(&DataKey::SchemaVersion),
            Some(3u32)
        );
        assert_eq!(storage.persistent().get::<_, Address>(&DataKey::Token), Some(token));
        assert_eq!(storage.persistent().get::<_, Address>(&DataKey::Admin), Some(admin));
        assert_eq!(storage.persistent().get::<_, i128>(&DataKey::MinTopup), Some(min_topup));
    });
}

#[test]
fn test_downgrade_attempt_returns_schema_migration_downgrade() {
    let env = Env::default();
    let contract_id = env.register(crate::SubscriptionVault, ());
    let client = crate::SubscriptionVaultClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let token = Address::generate(&env);

    // Deploy v3 state
    env.as_contract(&contract_id, || {
        let storage = env.storage();
        storage.persistent().set(&DataKey::SchemaVersion, &3u32);
        storage.persistent().set(&DataKey::Admin, &admin);
        storage.persistent().set(&DataKey::Token, &token);
    });

    env.mock_all_auths();

    // Attempt to migrate (which checks for downgrades)
    // Since we're already at v3, calling migrate should be a no-op
    // But migrate_config_to_persistent explicitly checks for downgrades
    
    // Simulate running a v2 binary against v3 state by trying to downgrade
    // The contract should reject this with SchemaMigrationDowngrade
    let result = client.try_migrate_config_to_persistent(&admin);
    
    // Expected behavior: downgrade is rejected
    assert_eq!(
        result,
        Err(Ok(Error::SchemaMigrationDowngrade)),
        "Downgrade from v3 to v2 should be rejected with SchemaMigrationDowngrade (9101)"
    );

    // Verify state is unchanged
    env.as_contract(&contract_id, || {
        assert_eq!(
            env.storage().persistent().get::<_, u32>(&DataKey::SchemaVersion),
            Some(3u32),
            "Schema version should remain at 3"
        );
    });
}

#[test]
fn test_v1_to_v3_migration_preserves_all_operational_data() {
    let env = Env::default();
    let contract_id = env.register(crate::SubscriptionVault, ());
    let client = crate::SubscriptionVaultClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let token = Address::generate(&env);
    let min_topup = 2_000_000i128;
    let grace_period = 172_800u64; // 2 days
    let next_id = 12345u32;
    let emergency_stop = false;

    // Simulate v1 state with all operational fields
    env.as_contract(&contract_id, || {
        let storage = env.storage();
        storage.instance().set(&DataKey::Token, &token);
        storage.instance().set(&DataKey::Admin, &admin);
        storage.instance().set(&DataKey::MinTopup, &min_topup);
        storage.instance().set(&DataKey::GracePeriod, &grace_period);
        storage.instance().set(&DataKey::NextId, &next_id);
        storage.instance().set(&DataKey::EmergencyStop, &emergency_stop);
    });

    env.mock_all_auths();
    client.migrate(&admin, &false);

    // Verify all data was preserved
    env.as_contract(&contract_id, || {
        let storage = env.storage();
        assert_eq!(storage.persistent().get::<_, u32>(&DataKey::SchemaVersion), Some(3u32));
        assert_eq!(storage.persistent().get::<_, Address>(&DataKey::Token), Some(token));
        assert_eq!(storage.persistent().get::<_, Address>(&DataKey::Admin), Some(admin));
        assert_eq!(storage.persistent().get::<_, i128>(&DataKey::MinTopup), Some(min_topup));
        assert_eq!(storage.persistent().get::<_, u64>(&DataKey::GracePeriod), Some(grace_period));
        assert_eq!(storage.persistent().get::<_, u32>(&DataKey::NextId), Some(next_id));
        assert_eq!(storage.persistent().get::<_, bool>(&DataKey::EmergencyStop), Some(emergency_stop));
    });

    // Verify contract is still operational
    assert_eq!(client.get_min_topup(), min_topup);
    assert_eq!(client.get_grace_period(), grace_period);
}

#[test]
fn test_migrate_config_to_persistent_rejects_far_above_version() {
    let env = Env::default();
    let contract_id = env.register(crate::SubscriptionVault, ());
    let client = crate::SubscriptionVaultClient::new(&env, &contract_id);
    let admin = Address::generate(&env);

    // Seed a version far above 3
    env.as_contract(&contract_id, || {
        env.storage().persistent().set(&DataKey::SchemaVersion, &99u32);
        env.storage().persistent().set(&DataKey::Admin, &admin);
    });
    env.mock_all_auths();

    // migrate_config_to_persistent should fail with SchemaMigrationDowngrade
    let err = client.try_migrate_config_to_persistent(&admin);
    assert_eq!(err, Err(Ok(Error::SchemaMigrationDowngrade)));

    // State must remain unchanged
    env.as_contract(&contract_id, || {
        assert_eq!(
            env.storage().persistent().get::<_, u32>(&DataKey::SchemaVersion),
            Some(99u32)
        );
    });
}

#[test]
fn test_migrate_from_version_zero_proceeds() {
    let env = Env::default();
    let contract_id = env.register(crate::SubscriptionVault, ());
    let client = crate::SubscriptionVaultClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let token = Address::generate(&env);

    // No schema version stored → get_schema_version returns 0
    env.as_contract(&contract_id, || {
        let storage = env.storage();
        storage.instance().set(&DataKey::Token, &token);
        storage.instance().set(&DataKey::Admin, &admin);
    });
    env.mock_all_auths();

    // Verify schema version is 0
    env.as_contract(&contract_id, || {
        assert!(!env.storage().instance().has(&DataKey::SchemaVersion));
        assert!(!env.storage().persistent().has(&DataKey::SchemaVersion));
    });

    // Migrate should succeed (upgrade ladder from v0 → v2 → v3)
    client.migrate(&admin, &false);

    // Verify final state
    env.as_contract(&contract_id, || {
        assert_eq!(
            env.storage().persistent().get::<_, u32>(&DataKey::SchemaVersion),
            Some(3u32)
        );
        assert_eq!(
            env.storage().persistent().get::<_, Address>(&DataKey::Token),
            Some(token)
        );
        assert_eq!(
            env.storage().persistent().get::<_, Address>(&DataKey::Admin),
            Some(admin)
        );
        assert!(!env.storage().instance().has(&DataKey::SchemaVersion));
    });
}
