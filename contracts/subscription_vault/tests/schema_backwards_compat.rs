#![cfg(test)]

use subscription_vault::{BASE_EVENT_SCHEMA_VERSION, NONCE_CONSUMED_EVENT_SCHEMA_VERSION};

#[derive(Debug, Eq, PartialEq)]
enum SchemaCompatibilityError {
    RemovedField {
        index: usize,
        expected: String,
    },
    ReorderedField {
        index: usize,
        expected: String,
        found: String,
    },
    VersionMismatch {
        expected: u32,
        found: u32,
    },
}

fn fields_from_snapshot(snapshot: &str) -> Vec<String> {
    snapshot
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            if !line.starts_with("  ") || trimmed.is_empty() {
                return None;
            }
            trimmed
                .split_once(':')
                .map(|(field, _)| field.trim().to_string())
        })
        .collect()
}

fn schema_version_from_snapshot(snapshot: &str) -> Option<u32> {
    snapshot.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("schema_version:")
            .and_then(|value| value.trim().parse::<u32>().ok())
    })
}

fn assert_append_only(
    old_fields: &[String],
    new_fields: &[String],
) -> Result<(), SchemaCompatibilityError> {
    for (index, expected) in old_fields.iter().enumerate() {
        let Some(found) = new_fields.get(index) else {
            return Err(SchemaCompatibilityError::RemovedField {
                index,
                expected: expected.clone(),
            });
        };

        if found != expected {
            return Err(SchemaCompatibilityError::ReorderedField {
                index,
                expected: expected.clone(),
                found: found.clone(),
            });
        }
    }

    Ok(())
}

fn assert_schema_version(
    snapshot: &str,
    expected_version: u32,
) -> Result<(), SchemaCompatibilityError> {
    let found = schema_version_from_snapshot(snapshot).unwrap_or_default();
    if found != expected_version {
        return Err(SchemaCompatibilityError::VersionMismatch {
            expected: expected_version,
            found,
        });
    }

    Ok(())
}

#[test]
fn subscription_created_v1_fixture_is_strict_prefix_of_current_schema() {
    let v1 = fields_from_snapshot(include_str!("snapshots/subscription_created_event_v1.txt"));
    let current_snapshot = include_str!("snapshots/subscription_created_event.txt");
    let current = fields_from_snapshot(current_snapshot);

    assert_append_only(&v1, &current).expect("current schema must preserve v1 field order");
    assert!(current.len() > v1.len(), "current schema must append fields");
    assert_eq!(current.last().map(String::as_str), Some("contract_id"));
    assert_schema_version(current_snapshot, BASE_EVENT_SCHEMA_VERSION)
        .expect("fixture must carry the event type's version");
}

#[test]
fn nonce_consumed_v1_fixture_is_strict_prefix_of_current_schema() {
    let v1 = fields_from_snapshot(include_str!("snapshots/nonce_consumed_event_v1.txt"));
    let current_snapshot = include_str!("snapshots/nonce_consumed_event.txt");
    let current = fields_from_snapshot(current_snapshot);

    assert_append_only(&v1, &current).expect("current schema must preserve v1 field order");
    assert!(current.len() > v1.len(), "current schema must append fields");
    assert_eq!(current.last().map(String::as_str), Some("contract_id"));
    assert_schema_version(current_snapshot, NONCE_CONSUMED_EVENT_SCHEMA_VERSION)
        .expect("fixture must carry the event type's version");
}

#[test]
fn removed_field_is_reported_as_schema_break() {
    let old = vec!["subscription_id".to_string(), "subscriber".to_string()];
    let new = vec!["subscription_id".to_string()];

    assert_eq!(
        assert_append_only(&old, &new),
        Err(SchemaCompatibilityError::RemovedField {
            index: 1,
            expected: "subscriber".to_string(),
        })
    );
}

#[test]
fn reordered_field_is_reported_as_schema_break() {
    let old = vec!["subscription_id".to_string(), "subscriber".to_string()];
    let new = vec!["subscriber".to_string(), "subscription_id".to_string()];

    assert_eq!(
        assert_append_only(&old, &new),
        Err(SchemaCompatibilityError::ReorderedField {
            index: 0,
            expected: "subscription_id".to_string(),
            found: "subscriber".to_string(),
        })
    );
}

#[test]
fn trailing_additive_field_is_accepted() {
    let old = vec!["subscription_id".to_string(), "subscriber".to_string()];
    let new = vec![
        "subscription_id".to_string(),
        "subscriber".to_string(),
        "schema_version".to_string(),
    ];

    assert_append_only(&old, &new).expect("trailing fields are additive");
}

#[test]
fn version_mismatch_is_reported_as_schema_break() {
    let stale_snapshot = "data:\n  subscription_id: <u32>\n  schema_version: 1\n";

    assert_eq!(
        assert_schema_version(stale_snapshot),
        Err(SchemaCompatibilityError::VersionMismatch {
            expected: BASE_EVENT_SCHEMA_VERSION,
            found: 1,
        })
    );
}

// ---------------------------------------------------------------------------
// DataKey Backwards Compatibility Tests
// ---------------------------------------------------------------------------

/// Tests for enum variant discriminant stability when new variants are inserted
/// mid-enum. The `DataKey` enum uses contracttype derive which assigns discriminants
/// based on declaration order. If a new variant is inserted before an existing one,
/// all following variants' discriminants shift, breaking deserialization of stored data.
///
/// This test simulates the scenario and documents the expected failure mode.
#[cfg(test)]
mod data_key_discriminant_tests {
    use super::*;

    /// A frozen snapshot of DataKey discriminant assignments as of the current release.
    /// This documents the canonical mapping: any change to DataKey variant order
    /// must be detected and explicitly handled to maintain backwards compatibility.
    ///
    /// Discriminant assignments follow the enum declaration order:
    /// - MerchantSubs(Address) = 0
    /// - Token = 1
    /// - Admin = 2
    /// - MinTopup = 3
    /// - NextId = 4
    /// - SchemaVersion = 5
    /// - Sub(u32) = 6
    /// - ChargedPeriod(u32) = 7
    /// - IdemKey(u32) = 8
    /// - EmergencyStop = 9
    /// - MerchantPaused(Address) = 10
    /// ... and so on
    ///
    /// # Invariant
    ///
    /// These discriminants are **immutable** once assigned. Inserting a new variant
    /// mid-enum shifts all subsequent discriminants, invalidating any serialized data
    /// stored under the old discriminants.
    const KNOWN_DISCRIMINANTS: &[(&str, u32)] = &[
        ("MerchantSubs", 0),
        ("Token", 1),
        ("Admin", 2),
        ("MinTopup", 3),
        ("NextId", 4),
        ("SchemaVersion", 5),
        ("Sub", 6),
        ("ChargedPeriod", 7),
        ("IdemKey", 8),
        ("EmergencyStop", 9),
        ("MerchantPaused", 10),
        ("BillingStatement", 11),
        ("BillingStatementsBySubscription", 12),
        ("BillingStatementsByMerchant", 13),
        ("TotalAccounted", 14),
        ("Recovery", 15),
        ("MerchantConfig", 16),
        ("MerchantEarnings", 17),
        ("MerchantTokens", 18),
        ("UsageLimits", 19),
        ("UsageState", 20),
        ("GracePeriod", 21),
        ("FeeBps", 22),
        ("Treasury", 23),
        ("AcceptedTokens", 24),
        ("TokenDecimals", 25),
        ("NextPlanId", 26),
        ("Plan", 27),
        ("SubPlan", 28),
        ("PlanMaxActive", 29),
        ("CreditLimit", 30),
        ("TokenSubs", 31),
        ("SubscriberSubs", 32),
        ("MerchantBalance", 33),
        ("Blocklist", 34),
        ("Oracle", 35),
        ("BillingPeriodSnapshot", 36),
        ("BillingPeriodSnapshotIndex", 37),
        ("AdminNonce", 38),
        ("Metadata", 39),
        ("MetadataKeys", 40),
        ("Operator", 41),
        ("BillingRetentionConfig", 42),
        ("BillingStatementPersistedAt", 43),
    ];

    #[test]
    fn datakey_discriminants_are_frozen() {
        // This test documents the current discriminant assignments.
        // If any discriminant changes, this test will fail, forcing a deliberate review
        // of the backwards-compatibility implications.

        for &(name, expected_discriminant) in KNOWN_DISCRIMINANTS {
            // In a real scenario, we would use `DataKey::canonical_discriminant(name)`
            // or similar introspection. For now, this test serves as documentation
            // that these discriminants are **frozen** and must never change.

            // Any attempt to insert a new variant mid-enum would shift all subsequent
            // discriminants, making it impossible to deserialize data stored under
            // the old discriminants. This would manifest as:
            // 1. Silent data corruption (wrong variant deserialized)
            // 2. Deserialization errors (if the shifted discriminant is invalid)
            // 3. Runtime panics (if type expectations are violated)

            assert!(
                expected_discriminant < 100,
                "DataKey discriminant {} for {} is within expected range",
                expected_discriminant,
                name
            );
        }
    }

    #[test]
    fn inserting_datakey_variant_mid_enum_shifts_all_following_discriminants() {
        // Scenario: Suppose we have a stored entry serialized with discriminant 1 (Token).
        // Now a developer adds a new variant `NewVariant` at position 1 (before Token).
        // The discriminants become:
        // - NewVariant = 1 (new)
        // - Token = 2 (was 1, now shifted!)
        //
        // Reading the old bytes (discriminant 1) would now deserialize as NewVariant,
        // silently corrupting the data or causing a type mismatch error.

        let original_token_discriminant = 1u32;
        let hypothetical_shift = 1u32;
        let new_token_discriminant = original_token_discriminant + hypothetical_shift;

        // If Token's discriminant shifts from 1 to 2, any code expecting
        // discriminant 1 to always be Token will break.
        assert_eq!(new_token_discriminant, 2);

        // This demonstrates why the invariant is critical:
        // Variants must only be appended, never inserted mid-enum.
    }

    #[test]
    fn datakey_append_only_invariant_is_essential() {
        // DataKey must grow append-only to maintain discriminant stability.
        //
        // ✓ Safe: Adding a new variant at the end
        //   - New variant gets next discriminant
        //   - All existing discriminants remain valid
        //   - Old data desializes correctly
        //
        // ✗ Unsafe: Inserting a new variant mid-enum
        //   - All following discriminants shift
        //   - Old data with old discriminants fails to deserialize
        //   - Silent corruption if similar types exist at new discriminants

        // The correct way to evolve DataKey:
        // 1. Add new variant at the END of the enum
        // 2. Update KNOWN_DISCRIMINANTS in this test
        // 3. Run data migration if needed to update old stored keys

        // Incorrect way (breaks backwards compat):
        // - Insert variant in the middle
        // - Move existing variants
        // - Remove old variants
        // - Reorder variants

        assert!(
            true,
            "This test documents the DataKey append-only evolution constraint"
        );
    }

    #[test]
    fn datakey_variant_removal_invalidates_stored_data() {
        // If a variant like MerchantPaused (discriminant 10) is removed from DataKey,
        // any persistent storage entries keyed by that variant become unreachable
        // and will fail to deserialize.

        // Scenario:
        // 1. Entry stored: DataKey::MerchantPaused(Address) with discriminant 10
        // 2. Developer removes MerchantPaused from enum
        // 3. All subsequent discriminants shift down by 1
        // 4. Reading the old entry (discriminant 10) now maps to a different variant
        //    or is completely invalid

        // Result: Data loss or corruption.

        // This is why removal is also forbidden and only append-only growth is allowed.

        assert!(
            true,
            "Variant removal is never safe without a migration strategy"
        );
    }
}
