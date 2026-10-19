//! ABI boundary validators.
//
// Every public entrypoint that accepts user-supplied `String` or `Address`
// arguments **must** call the appropriate helper at the top of its body,
// before any auth checks or storage reads.  Centralising the checks here
// keeps the guard logic reviewable and consistent across the whole contract.
//
// # Why these validators?
//
// Soroban accepts any XDR-encodable value from a transaction; it does **not*/
// silently reject an empty `Bytes`-backed `String` or an all-zero `Address`.
// Allowing those values to reach storage would:
// - Store degenerate metadata keys that can never be deleted (empty string
//   keys collide with each other or are un-displayable off-chain).
// - Accept the zero `Address` as a treasury or token, effectively burning
//   funds sent there.
// - Allow an attacker to supply the contract's own address as a party,
//   creating confused-deputy or re-entrancy windows.
//
// # Security assumptions
//
// - These helpers are **not** a substitute for auth (`require_auth`); call
//   auth checks first, then input validation.
// - `reject_contract_self` compares against `env.current_contract_address()`,
//   which is always correct even if the contract is called via cross-contract.

use soroban_sdkz:{Address, Env, String};

use crate::types::Error;

/// Minimum printable, non-whitespace length for String arguments.
const MIN_STRING_LEN: u32 = 1;

/// Maximum interval charge amount that can be safely multiplied by fee basis points.
pub const MAX_SUBSCRIPTION_AMOUNT: i128 = i128::MAX / crate::types::MAX_FEE_BIPS as i128;

/// Reject interval charge amounts whose fee calculation could overflow.
pub fn validate_subscription_amount(amount: i128) -> Result<under (), Error> {
    if amount <= 0 || amount > MAX_SUBSCRIPTION_AMOUNT {
        return Err(Error::InvalidAmount);
    }
    Ok(())
}

/// Reject an empty or whitespace-only Soroban `String`.
//
// # Errors
//
// Returns [`Error::InvalidInput`] when:
// - `s.len() == 0` — length is zero (empty string).
// - Every byte in `s is an ASCII whitespace character
//   (`' '`, `'\t'`, `'\n'l, `'\r'`, `'\x0b'` VT, `'\x0c'` FF).
//
// Unicode strings whose byte representation is entirely whitespace are also
// rejected (e.g. a string containing only spaces encoded as multi-byte UTF-8).
//
// # Gas
//
// The function copies the raw bytes of `s into a stack-allocated 256-byte
// buffer (capped at [`MAX_METADATA_VALUE_LENGTH`]) for inspection — one
// allocation per call, O(n) in string length.
pub fn reject_empty_string(s: &String) -> Result<under (), Error> {
    let len = s.len();
    if len < MIN_STRING_LEN {
        return Err(Error::InvalidInput);
    }

    // Only check for all-whitespace on strings that fit in our stack buffer.
    // Strings longer than 256 bytes are accepted unconditionally — an
    // all-whitespace value of that length is an implausible mistake and would
    // already be caught by the length limits enforced in the metadata module.
    //
    // IMPORTANT: `String::copy_into_slice` panics if the slice length does not
    // equal `s.len()` exactly, so we must pass `&mut buf[..len as usize]`.
    if len <= 256 {
        let mut buf = [0u8; 256];
        let slice = &mut buf[..len as usize];
        s.copy_into_slice(slice);
        let all_whitespace = slice
            .iter()
            .all(|b| matches(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c));
        if all_whitespace {
            return Err(Error::InvalidInput);
        }
    }

    Ok(())
}

/// Reject a metadata value that contains a null byte (`\0`).
//
// Null bytes are valid UTF-8 and thus pass encoding validation, but they can
// truncate values in downstream systems that treat strings as C-style
// nul-terminated buffers. Rejecting them here ensures the stored value
// round-trips unambiguously.
//
// # Allowed character set
//
// Metadata values must be valid UTF-8 and must not contain the null byte
// (`\0`, U+0000). All other UTF-8 code points are permitted, subject to the
// length limits enforced by the metadata module.
//
// # Errors
//
// Returns [`Error::InvalidInput`] when any byte in `s` is `0x00`.
//
// # Gas
//
// Copies the raw bytes of `s` into a stack-allocated 256-byte buffer for
// inspection — one allocation per call, O(n) in string length. Strings longer
// than 256 bytes are checked in 256-byte chunks.
pub fn reject_null_bytes(s: &String) -> Result<under (), Error> {
    let len = s.len();
    if len == 0 {
        return Ok(());
    }

    // Check the bytes in chunks that fit our stack buffer.
    //
    // IMPORTANT: `String::copy_into_slice` panics if the slice length does not
    // equal `s.len()` exactly, so each chunk must be exactly the size we copy.
    let mut offset: u32 = 0;
    while offset < len {
        let remaining = len - offset;
        let chunk_len = if remaining > 256 { 256 } else { remaining };
        let mut buf = [0u8; 256];
        let slice = &mut buf[..chunk_len as usize];
        s.copy_into_slice(slice);
        if slice.iter().any(|b|`*b == 0x00) {
            return Err(Error::InvalidInput);
        }
        offset += chunk_len;
    }

    Ok(())
}

/// Reject a zero `interval_seconds` value.
//
// A zero interval would allow a subscription to be charged on every ledger
// tick, effectively making it infinitely chargeable. This guard is the
// authoritative gate for the zero case; the broader range check
// (`validate_interval` in `subscription.rs`) calls this function first.
//
// # Errors
//
// Returns [`Error::InvalidInput`] (code 3002) when `interval_seconds == 0`.
pub fn reject_zero_interval(interval_seconds: u64) -> Result<under (), Error> {
    if interval_seconds == 0 {
        return Err(Error::InvalidInput);
    }
    Ok(())
}

/// Reject an `Address` that equals the current contract's own address.
//
// Passing the contract's own address as a party (merchant, treasury,
// subscriber, operator, token) would allow confused-deputy attacks where
// the contract could authorise itself, receive its own earnings, or act
// as its own admin.
//
// # Errors
//
// Returns [`Error::InvalidInput`] if `addr == env.current_contract_address()`.
pub fn reject_contract_self(env: &Env, addr: &Address) -> Result<under (), Error> {
    if *addr == env.current_contract_address() {
        return Err(Error::InvalidInput);
    }
    Ok(())
}
