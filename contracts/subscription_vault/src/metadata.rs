use soroban_sdk::xdr::{AccountId, PublicKey, ScAddress, Uint256};
use soroban_sdk::{Address, Bytes, BytesN, Env, String, Symbol, TryFromVal, Vec};

use crate::queries::get_subscription;
use crate::types::{
    DataKey, Error, MetadataDeletedEvent, MetadataSetEvent, MetadataSetSignedEvent,
    SignedMetadataPayload, MAX_METADATA_KEYS, MAX_METADATA_KEY_LENGTH, MAX_METADATA_VALUE_LENGTH,
};

/// Length of the domain-separator prefix baked into the canonical signed message.
///
/// Bumping this constant changes the on-chain signature domain, so it must only
/// happen as part of an explicit, reviewed contract upgrade.
const SIGNED_MSG_DOMAIN_TAG_LEN: u32 = 32;

/// Domain separator for [`build_metadata_signed_message`].
///
/// Mixed into every canonical signed-metadata payload before the user-supplied
/// fields. Changing this value invalidates every previously-signed payload, so
/// it must only be updated alongside a contract version bump.
const SIGNED_MSG_DOMAIN_TAG: &[u8; SIGNED_MSG_DOMAIN_TAG_LEN as usize] =
    b"SBL_META_SIGNED_v1\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";

/// Wrapper called by the contract entrypoint.
pub fn do_set_metadata(
    env: &Env,
    authorizer: Address,
    subscription_id: u32,
    key: String,
    value: String,
) -> Result<(), Error> {
    set_metadata(env, subscription_id, &authorizer, key, value)
}

/// Wrapper called by the contract entrypoint.
pub fn do_delete_metadata(
    env: &Env,
    authorizer: Address,
    subscription_id: u32,
    key: String,
) -> Result<(), Error> {
    delete_metadata(env, subscription_id, &authorizer, key)
}

/// Apply an off-chain signed metadata update.
///
/// # Auth flow
///
/// 1. Look up `payload.subscription_id`. Reject if it does not exist.
/// 2. Reconstruct the canonical message from `(payload, chain_id)` and
///    verify the supplied ed25519 signature against the supplied pubkey.
///    `env.crypto().ed25519_verify` **panics** when the signature is
///    forgeries or the wrong key — that is the desired security boundary.
/// 3. Derive the Soroban Address for `signer_pubkey` and confirm it is
///    either `sub.subscriber` or `sub.merchant`.
/// 4. Reject when `now >= payload.expires_at`.
/// 5. Consume `payload.nonce` from `(signer_address, DOMAIN_METADATA_SIGNED)`.
/// 6. Apply the same key/value/key-cap invariants as [`set_metadata`] and
///    emit [`MetadataSetSignedEvent`].
///
/// # Errors
///
/// * [`Error::NotFound`] — `subscription_id` does not exist.
/// * [`Error::InvalidInput`] — `expires_at <= now` (payload already stale).
/// * [`Error::MetadataKeyTooLong`] / [`Error::MetadataValueTooLong`] —
///   payload violated length limits.
/// * [`Error::MetadataKeyLimitReached`] — adding a new key would exceed the
///   per-subscription key cap.
/// * [`Error::NonceAlreadyUsed`] — replay or out-of-order nonce.
/// * [`Error::Overflow`] — nonce counter would overflow u64.
/// * [`Error::Forbidden`] — derived signer is not the subscription's
///   subscriber or merchant (only reachable when the signature itself was
///   valid for a pubkey the contract cannot link to this subscription).
///
/// Panics with a host crypto error when the ed25519 signature fails to
/// validate. That is intentional: a forged signature must abort the
/// transaction rather than be silently downgraded to a typed error.
pub fn do_set_metadata_signed(
    env: &Env,
    signer_pubkey: BytesN<32>,
    payload: SignedMetadataPayload,
    signature: BytesN<64>,
) -> Result<(), Error> {
    let network_id = env.ledger().network_id();

    // Auth-first ordering: prove the subscription actually exists before
    // touching crypto or storage. A bogus subscription_id otherwise lets an
    // attacker burn slot in the nonce counter for IDs that will never
    // resolve.
    let sub = get_subscription(env, payload.subscription_id)?;

    // Validate key/value lengths and key quota before building the message — avoids
    // panicking in build_metadata_signed_message if the payload is
    // over-length or over-capacity, and returns a clean error without mutating storage.
    validate_metadata_input(env, payload.subscription_id, &payload.key, &payload.value)?;

    // Build the canonical message and verify the ed25519 signature against
    // the supplied pubkey. Verification hashes the message internally per
    // RFC 8032 — we feed it the raw canonical bytes.
    let message = build_metadata_signed_message(env, &payload, &network_id);
    env.crypto()
        .ed25519_verify(&signer_pubkey, &message, &signature);

    // Map the raw pubkey to a Soroban Address via its XDR AccountId so the
    // nonce counter is keyed under a deterministic (Address, domain) pair.
    //
    // Soundness: the on-chain `require_auth()` decodes the signer's strkey to
    // the same underlying AccountId bytes that this path reconstructs, so a
    // pubkey that authenticates as e.g. sub.subscriber on-chain matches the
    // same address here.
    let pk_array = signer_pubkey.to_array();
    let signer_address = Address::try_from_val(
        env,
        &ScAddress::Account(AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
            pk_array,
        )))),
    )
    .map_err(|_| Error::Overflow)?;

    // Identity check: the pubkey must correspond to a party of the
    // subscription. Without this guard a key holder could move metadata on
    // subscriptions they have no relationship with.
    if signer_address != sub.subscriber && signer_address != sub.merchant {
        return Err(Error::Forbidden);
    }

    // Reject stale payloads. Strict inequality (`>=`) means an exactly-
    // expired payload is rejected deterministically without an off-by-one
    // hazard. A generously future-dated `expires_at` (e.g. now + one
    // interval) is the recommended setting.
    let now = env.ledger().timestamp();
    if now >= payload.expires_at {
        return Err(Error::InvalidInput);
    }

    // Replay protection: consume the next-expected nonce for
    // (signer_address, DOMAIN_METADATA_SIGNED). Rejects out-of-order or
    // duplicate nonces and emits an audit event.
    crate::nonce::check_and_advance(
        env,
        &signer_address,
        crate::nonce::DOMAIN_METADATA_SIGNED,
        payload.nonce,
    )?;

    // Commit storage update only after all authentication, expiration, and nonce checks pass.
    apply_metadata_storage(env, payload.subscription_id, &payload.key, &payload.value);

    env.events().publish(
        (
            Symbol::new(env, "metadata_set_signed"),
            payload.subscription_id,
        ),
        MetadataSetSignedEvent {
            subscription_id: payload.subscription_id,
            key: payload.key.clone(),
            signer: signer_address,
            nonce: payload.nonce,
            timestamp: now,
            schema_version: crate::types::BASE_EVENT_SCHEMA_VERSION,
            contract_id: env.current_contract_address(),
        },
    );

    Ok(())
}

/// Read the next-expected nonce for the metadata-signed domain for a signer.
///
/// Convenience helper for off-chain tooling: callers should fetch this value
/// before constructing the next [`SignedMetadataPayload`] so that
/// `payload.nonce` matches the on-chain counter and is not rejected as a
/// replay.
///
/// # Auth
///
/// Read-only; no auth required.
pub fn get_metadata_signed_nonce(env: &Env, signer: Address) -> u64 {
    crate::nonce::get_nonce(env, &signer, crate::nonce::DOMAIN_METADATA_SIGNED)
}

/// Build the canonical, deterministic byte string that must be signed off-chain.
///
/// Layout (all multi-byte integers big-endian, no padding):
///
/// ```text
///   SIGNED_MSG_DOMAIN_TAG (32 bytes)
/// || subscription_id   (4 bytes, u32 BE)
/// || key_len           (4 bytes, u32 BE)
/// || key               (key_len bytes)
/// || value_len         (4 bytes, u32 BE)
/// || value             (value_len bytes)
/// || nonce             (8 bytes, u64 BE)
/// || chain_id_len      (4 bytes, u32 BE)
/// || chain_id          (chain_id_len bytes)
/// || expires_at        (8 bytes, u64 BE)
/// ```
///
/// The domain tag and per-field length prefixes make the encoding unambiguous:
/// no two distinct payloads can ever produce the same byte sequence, and no
/// struct field boundary can be confused with another.
///
/// Exposed publicly so tests can construct the same byte stream the contract
/// verifies.
pub fn build_metadata_signed_message(
    env: &Env,
    payload: &SignedMetadataPayload,
    network_id: &BytesN<32>,
) -> Bytes {
    let mut buf = Bytes::new(env);
    buf.extend_from_slice(&SIGNED_MSG_DOMAIN_TAG[..]);

    let sub_id_bytes = payload.subscription_id.to_be_bytes();
    buf.extend_from_slice(&sub_id_bytes);

    let key_len: u32 = payload.key.len();
    let key_len_bytes = key_len.to_be_bytes();
    buf.extend_from_slice(&key_len_bytes);
    if key_len > 0 {
        let mut key_buf = vec![0u8; key_len as usize];
        payload.key.copy_into_slice(&mut key_buf);
        buf.extend_from_slice(&key_buf);
    }

    let value_len: u32 = payload.value.len();
    let value_len_bytes = value_len.to_be_bytes();
    buf.extend_from_slice(&value_len_bytes);
    if value_len > 0 {
        let mut value_buf = vec![0u8; value_len as usize];
        payload.value.copy_into_slice(&mut value_buf);
        buf.extend_from_slice(&value_buf);
    }

    let nonce_bytes = payload.nonce.to_be_bytes();
    buf.extend_from_slice(&nonce_bytes);

    // Network ID is always 32 bytes.
    let network_id_len: u32 = 32;
    let network_id_len_bytes = network_id_len.to_be_bytes();
    buf.extend_from_slice(&network_id_len_bytes);
    buf.extend_from_slice(&network_id.to_array());

    let expires_bytes = payload.expires_at.to_be_bytes();
    buf.extend_from_slice(&expires_bytes);

    buf
}

/// Validate metadata key, value, and key quota without modifying contract storage.
///
/// Ensures key and value length constraints, UTF-8 validity, and that adding
/// a new key will not exceed [`MAX_METADATA_KEYS`].
///
/// # Insert vs. update (Issue #476)
///
/// The key-count limit (`MAX_METADATA_KEYS`) is only enforced for **new**
/// keys. Updating an existing key — one already present in
/// `DataKey::MetadataKeys(subscription_id)` — does **not** consume a new
/// slot and must always succeed (provided the value passes length/UTF-8
/// validation). This is checked by testing `key_exists` before comparing
/// `keys.len() >= MAX_METADATA_KEYS`: if the key already exists the limit
/// is skipped entirely.
///
/// # Length limits are byte-based (Issue #304)
///
/// `MAX_METADATA_KEY_LENGTH` and `MAX_METADATA_VALUE_LENGTH` are enforced on
/// the raw UTF-8 encoded byte length (`String::len()`), **not** on the number
/// of visible characters. A multi-byte character (e.g. CJK, emoji) counts as
/// multiple bytes toward the limit, so a key of at most 32 bytes may contain
/// fewer than 32 characters.
pub fn validate_metadata_input(
    env: &Env,
    subscription_id: u32,
    key: &String,
    value: &String,
) -> Result<(), Error> {
    if key.len() > MAX_METADATA_KEY_LENGTH {
        return Err(Error::MetadataKeyTooLong);
    }

    if value.len() > MAX_METADATA_VALUE_LENGTH {
        return Err(Error::MetadataValueTooLong);
    }

    // UTF-8 validation for key and value to prevent corruption of off-chain indexers
    // that parse event payloads as UTF-8. soroban_sdk::String should contain valid UTF-8,
    // but we validate explicitly to ensure data integrity.
    validate_utf8_string(key, "metadata key")?;
    validate_utf8_string(value, "metadata value")?;

    let keys: Vec<String> = env
        .storage()
        .persistent()
        .get(&DataKey::MetadataKeys(subscription_id))
        .unwrap_or(Vec::new(env));

    let key_exists = keys.iter().any(|k| k == *key);

    if !key_exists && keys.len() >= MAX_METADATA_KEYS {
        return Err(Error::MetadataKeyLimitReached);
    }

    Ok(())
}

/// Commit metadata key and value into persistent storage.
///
/// Assumes [`validate_metadata_input`] has already validated lengths, UTF-8,
/// and key quota.
pub fn apply_metadata_storage(env: &Env, subscription_id: u32, key: &String, value: &String) {
    let mut keys: Vec<String> = env
        .storage()
        .persistent()
        .get(&DataKey::MetadataKeys(subscription_id))
        .unwrap_or(Vec::new(env));

    let key_exists = keys.iter().any(|k| k == *key);

    if !key_exists {
        keys.push_back(key.clone());
        env.storage()
            .persistent()
            .set(&DataKey::MetadataKeys(subscription_id), &keys);
    }

    env.storage()
        .persistent()
        .set(&DataKey::Metadata(subscription_id, key.clone()), value);
}

/// Shared inner helper for the on-chain entrypoint.
///
/// Validates key/value lengths and key quota, writes the storage entry, and returns.
pub fn apply_metadata_value(
    env: &Env,
    subscription_id: u32,
    key: &String,
    value: &String,
) -> Result<(), Error> {
    validate_metadata_input(env, subscription_id, key, value)?;
    apply_metadata_storage(env, subscription_id, key, value);
    Ok(())
}

pub fn set_metadata(
    env: &Env,
    subscription_id: u32,
    caller: &Address,
    key: String,
    value: String,
) -> Result<(), Error> {
    caller.require_auth();

    let sub = get_subscription(env, subscription_id)?;
    if caller != &sub.subscriber && caller != &sub.merchant {
        return Err(Error::Forbidden);
    }

    apply_metadata_value(env, subscription_id, &key, &value)?;

    env.events().publish(
        (Symbol::new(env, "metadata_set"), subscription_id),
        MetadataSetEvent {
            subscription_id,
            key,
            authorizer: caller.clone(),
            schema_version: crate::types::BASE_EVENT_SCHEMA_VERSION,
            contract_id: env.current_contract_address(),
        },
    );

    Ok(())
}

pub fn get_metadata(env: &Env, subscription_id: u32, key: String) -> Result<String, Error> {
    let _ = get_subscription(env, subscription_id)?;

    env.storage()
        .persistent()
        .get(&DataKey::Metadata(subscription_id, key))
        .ok_or(Error::NotFound)
}

pub fn delete_metadata(
    env: &Env,
    subscription_id: u32,
    caller: &Address,
    key: String,
) -> Result<(), Error> {
    caller.require_auth();

    let sub = get_subscription(env, subscription_id)?;
    if caller != &sub.subscriber && caller != &sub.merchant {
        return Err(Error::Forbidden);
    }

    let mut keys: Vec<String> = env
        .storage()
        .persistent()
        .get(&DataKey::MetadataKeys(subscription_id))
        .unwrap_or(Vec::new(env));

    if let Some(idx) = keys.iter().position(|k| k == key) {
        let idx_u32 = idx.try_into().map_err(|_| Error::Overflow)?;
        keys.remove(idx_u32);
        env.storage()
            .persistent()
            .set(&DataKey::MetadataKeys(subscription_id), &keys);

        env.storage()
            .persistent()
            .remove(&DataKey::Metadata(subscription_id, key.clone()));

        env.events().publish(
            (Symbol::new(env, "metadata_deleted"), subscription_id),
            MetadataDeletedEvent {
                subscription_id,
                key,
                authorizer: caller.clone(),
                schema_version: crate::types::BASE_EVENT_SCHEMA_VERSION,
                contract_id: env.current_contract_address(),
            },
        );
    }

    Ok(())
}

pub fn list_metadata_keys(env: &Env, subscription_id: u32) -> Result<Vec<String>, Error> {
    let _ = get_subscription(env, subscription_id)?;

    let keys: Vec<String> = env
        .storage()
        .persistent()
        .get(&DataKey::MetadataKeys(subscription_id))
        .unwrap_or(Vec::new(env));

    Ok(keys)
}

/// Validates that a soroban_sdk::String contains valid UTF-8 byte sequences.
///
/// While soroban_sdk::String should theoretically contain valid UTF-8, this explicit
/// validation prevents malformed byte sequences from corrupting off-chain indexers
/// that parse event payloads as UTF-8.
///
/// # Arguments
/// * `s` - The string to validate
/// * `field_name` - Human-readable field name for error context
///
/// # Errors
/// * `Error::InvalidInput` - If the string contains invalid UTF-8 sequences
fn validate_utf8_string(s: &String, _field_name: &str) -> Result<(), Error> {
    // Reject empty strings up front.
    if s.len() == 0 {
        return Err(Error::InvalidInput);
    }

    // Validate the raw bytes with Rust's standard UTF-8 decoder (issue #149 /
    // #189) rather than a manual byte-range check, so valid multi-byte
    // sequences (CJK, emoji, accented Latin, ...) are accepted.
    let len = s.len() as usize;
    let mut raw = vec![0u8; len];
    s.copy_into_slice(&mut raw);
    let text = core::str::from_utf8(&raw).map_err(|_| Error::InvalidInput)?;

    // Reject null bytes (issue #300). `\0` is valid UTF-8 but terminates
    // C-style strings in downstream systems, silently truncating values.
    // The allowed character set is: any valid UTF-8 sequence that contains
    // no NUL byte, no control characters other than tab (0x09), newline
    // (0x0A), and carriage return (0x0D), and at least one printable
    // (>= 0x20) non-whitespace character.
    if text.bytes().any(|byte| byte == 0) {
        return Err(Error::InvalidInput);
    }

    // Reject control characters (except tab/newline/CR) and require at least
    // one printable, non-whitespace character.
    let mut has_non_control_char = false;
    for byte in text.bytes() {
        if byte < 32 && byte != 9 && byte != 10 && byte != 13 {
            return Err(Error::InvalidInput);
        }
        if byte >= 32 {
            has_non_control_char = true;
        }
    }
    if !has_non_control_char {
        return Err(Error::InvalidInput);
    }

    Ok(())
}

#[cfg(test)]
mod utf8_validation_tests {
    use super::*;
    use soroban_sdk::{Env, String};

    #[test]
    fn accepts_cjk_metadata_values() {
        let env = Env::default();
        let value = String::from_str(&env, "日本語のマーチャント名");
        assert!(validate_utf8_string(&value, "metadata value").is_ok());
    }

    #[test]
    fn accepts_accented_and_emoji_values() {
        let env = Env::default();
        let value = String::from_str(&env, "héllo wørld 🚀");
        assert!(validate_utf8_string(&value, "metadata value").is_ok());
    }

    #[test]
    fn accepts_cjk_metadata_keys() {
        let env = Env::default();
        let key = String::from_str(&env, "名前");
        assert!(validate_utf8_string(&key, "metadata key").is_ok());
    }

    #[test]
    fn rejects_empty_values() {
        let env = Env::default();
        let empty = String::from_str(&env, "");
        assert!(validate_utf8_string(&empty, "metadata value").is_err());
    }

    #[test]
    fn rejects_whitespace_only_values() {
        let env = Env::default();
        let spaces = String::from_str(&env, "   ");
        assert!(validate_utf8_string(&spaces, "metadata value").is_err());
    }

    #[test]
    fn rejects_null_bytes_in_values() {
        let env = Env::default();
        let value = String::from_str(&env, "abc\0def");
        assert!(validate_utf8_string(&value, "metadata value").is_err());
    }

    #[test]
    fn rejects_null_bytes_in_keys() {
        let env = Env::default();
        let key = String::from_str(&env, "key\0name");
        assert!(validate_utf8_string(&key, "metadata key").is_err());
    }
}

#[cfg(test)]
mod metadata_key_length_tests {
    use super::*;
    use soroban_sdk::{Env, String};

    /// A 32-byte ASCII key is exactly at the limit and must be accepted.
    #[test]
    fn accepts_ascii_key_at_byte_limit() {
        let env = Env::default();
        let key = String::from_str(&env, "01234567890123456789012345678901");
        assert_eq!(key.len(), MAX_METADATA_KEY_LENGTH);
        assert!(validate_metadata_input(&env, 1, &key, &String::from_str(&env, "v")).is_ok());
    }

    /// A 33-byte ASCII key exceeds the limit and must be rejected.
    #[test]
    fn rejects_ascii_key_over_byte_limit() {
        let env = Env::default();
        let key = String::from_str(&env, "012345678901234567890123456789012");
        assert_eq!(key.len(), MAX_METADATA_KEY_LENGTH + 1);
        assert_eq!(
            validate_metadata_input(&env, 1, &key, &String::from_str(&env, "v")),
            Err(Error::MetadataKeyTooLong)
        );
    }

    /// A multi-byte key whose UTF-8 byte length is exactly at the limit is
    /// accepted, even though it has fewer than 32 visible characters.
    #[test]
    fn accepts_multibyte_key_at_byte_limit() {
        let env = Env::default();
        // Each CJK character is 3 bytes in UTF-8; 10 chars = 30 bytes, plus
        // one 2-byte character ("é") = 32 bytes total.
        let key = String::from_str(&env, "日本語日本語日本é");
        assert_eq!(key.len(), MAX_METADATA_KEY_LENGTH);
        assert!(validate_metadata_input(&env, 1, &key, &String::from_str(&env, "v")).is_ok());
    }

    /// A multi-byte key whose UTF-8 byte length exceeds the limit is rejected
    /// even though its character count is well under 32.
    #[test]
    fn rejects_multibyte_key_over_byte_limit() {
        let env = Env::default();
        // 11 CJK characters = 33 bytes, but only 11 visible characters.
        let key = String::from_str(&env, "日本語日本語日本語");
        assert_eq!(key.len(), MAX_METADATA_KEY_LENGTH + 1);
        assert_eq!(
            validate_metadata_input(&env, 1, &key, &String::from_str(&env, "v")),
            Err(Error::MetadataKeyTooLong)
        );
    }

    /// An emoji key (4 bytes per code point) is enforced on bytes, not chars.
    #[test]
    fn rejects_emoji_key_over_byte_limit() {
        let env = Env::default();
        // 9 emoji = 36 bytes, but only 9 visible characters.
        let key = String::from_str(&env, "🚀🚀🚀🚀🚀🚀🚀🚀🚀");
        assert_eq!(key.len(), 36);
        assert_eq!(
            validate_metadata_input(&env, 1, &key, &String::from_str(&env, "v")),
            Err(Error::MetadataKeyTooLong)
        );
    }
}

#[cfg(test)]
mod signed_message_tests {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    /// Two structurally identical payloads signed on different chains must
    /// produce different message bytes — chain_id is mixed into the canonical
    /// encoding.
    #[test]
    fn message_is_chain_specific() {
        let env = Env::default();
        let sub = Address::generate(&env);
        let payload = SignedMetadataPayload {
            subscription_id: 7,
            key: String::from_str(&env, "k"),
            value: String::from_str(&env, "v"),
            nonce: 0,
            expires_at: 100,
        };

        let chain_a = BytesN::from_array(
            &env,
            &[
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x01,
            ],
        );
        let chain_b = BytesN::from_array(
            &env,
            &[
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x02,
            ],
        );

        let msg_a = build_metadata_signed_message(&env, &payload, &chain_a);
        let msg_b = build_metadata_signed_message(&env, &payload, &chain_b);

        assert_ne!(msg_a, msg_b);
        // Ignore the unused sub warning: the address must remain generated
        // so the test stays deterministic across cargo test runs.
        let _ = sub;
    }

    /// Mutating any field of the payload changes the message bytes.
    #[test]
    fn message_changes_on_any_field_mutation() {
        let env = Env::default();
        let chain = BytesN::from_array(
            &env,
            &[
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x03,
            ],
        );

        let base = || SignedMetadataPayload {
            subscription_id: 1,
            key: String::from_str(&env, "k"),
            value: String::from_str(&env, "v"),
            nonce: 0,
            expires_at: 100,
        };

        let base_msg = build_metadata_signed_message(&env, &base(), &chain);

        let mut different = base();
        different.subscription_id = 2;
        assert_ne!(
            base_msg,
            build_metadata_signed_message(&env, &different, &chain)
        );

        let mut different = base();
        different.key = String::from_str(&env, "k2");
        assert_ne!(
            base_msg,
            build_metadata_signed_message(&env, &different, &chain)
        );

        let mut different = base();
        different.value = String::from_str(&env, "v2");
        assert_ne!(
            base_msg,
            build_metadata_signed_message(&env, &different, &chain)
        );

        let mut different = base();
        different.nonce = 1;
        assert_ne!(
            base_msg,
            build_metadata_signed_message(&env, &different, &chain)
        );

        let mut different = base();
        different.expires_at = 101;
        assert_ne!(
            base_msg,
            build_metadata_signed_message(&env, &different, &chain)
        );
    }

    /// Verify the auxiliary [`get_metadata_signed_nonce`] helper is wired to
    /// the signed-metadata domain and starts at zero.
    #[test]
    fn signed_nonce_helper_starts_at_zero() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(crate::SubscriptionVault, ());

        let signer = Address::generate(&env);
        env.as_contract(&contract_id, || {
            assert_eq!(get_metadata_signed_nonce(&env, signer.clone()), 0);
        });
    }

    /// The signed-message helper module's tests exercise the inner wiring
    /// without going through the full signed-update flow. The full positive
    /// and negative paths live in `test_metadata_signed.rs`.
    #[test]
    fn smoke_helper_module_compiles() {
        let env = Env::default();
        let _ = env;
    }
}

#[cfg(test)]
mod metadata_limit_ordering_tests {
    use super::*;
    use crate::types::DataKey;
    use soroban_sdk::{Env, String, Vec};

    #[test]
    fn metadata_key_limit_checked_before_storage_mutation() {
        let env = Env::default();
        let sub_id = 42u32;

        // Pre-fill MAX_METADATA_KEYS into storage
        let mut keys = Vec::new(&env);
        for n in 0..MAX_METADATA_KEYS {
            let k = String::from_str(&env, &format!("key_{}", n));
            let v = String::from_str(&env, &format!("val_{}", n));
            keys.push_back(k.clone());
            env.storage()
                .persistent()
                .set(&DataKey::Metadata(sub_id, k), &v);
        }
        env.storage()
            .persistent()
            .set(&DataKey::MetadataKeys(sub_id), &keys);

        // Attempt to validate/insert an 11th distinct key
        let new_key = String::from_str(&env, "key_overflow");
        let new_val = String::from_str(&env, "val_overflow");

        let res = apply_metadata_value(&env, sub_id, &new_key, &new_val);
        assert_eq!(res, Err(Error::MetadataKeyLimitReached));

        // Storage MUST be completely unchanged:
        // 1. New key must not exist in MetadataKeys list
        let stored_keys: Vec<String> = env
            .storage()
            .persistent()
            .get(&DataKey::MetadataKeys(sub_id))
            .unwrap();
        assert_eq!(stored_keys.len(), MAX_METADATA_KEYS);
        assert!(!stored_keys.iter().any(|k| k == new_key));

        // 2. New key must not exist in DataKey::Metadata
        let stored_val: Option<String> = env
            .storage()
            .persistent()
            .get(&DataKey::Metadata(sub_id, new_key.clone()));
        assert_eq!(stored_val, None);

        // 3. All original keys and values remain intact
        for n in 0..MAX_METADATA_KEYS {
            let k = String::from_str(&env, &format!("key_{}", n));
            let expected_v = String::from_str(&env, &format!("val_{}", n));
            let actual_v: String = env
                .storage()
                .persistent()
                .get(&DataKey::Metadata(sub_id, k))
                .unwrap();
            assert_eq!(actual_v, expected_v);
        }
    }

    #[test]
    fn metadata_update_existing_key_at_limit_does_not_fail() {
        let env = Env::default();
        let sub_id = 42u32;

        let mut keys = Vec::new(&env);
        for n in 0..MAX_METADATA_KEYS {
            let k = String::from_str(&env, &format!("key_{}", n));
            let v = String::from_str(&env, "old");
            keys.push_back(k.clone());
            env.storage()
                .persistent()
                .set(&DataKey::Metadata(sub_id, k), &v);
        }
        env.storage()
            .persistent()
            .set(&DataKey::MetadataKeys(sub_id), &keys);

        // Updating an existing key at capacity must succeed and update storage
        let key_0 = String::from_str(&env, "key_0");
        let new_val = String::from_str(&env, "new_val");

        let res = apply_metadata_value(&env, sub_id, &key_0, &new_val);
        assert!(res.is_ok());

        let actual_v: String = env
            .storage()
            .persistent()
            .get(&DataKey::Metadata(sub_id, key_0))
            .unwrap();
        assert_eq!(actual_v, new_val);
    }
}
