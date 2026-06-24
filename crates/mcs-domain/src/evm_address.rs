//! Ethereum address value object.
//!
//! [`EvmAddress`] wraps a validated, lowercased `0x`-prefixed 40-hex-character
//! Ethereum address. No on-chain interaction or checksumming (EIP-55) is
//! performed — this is purely a format-validation value object.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::DomainError;

/// A validated `0x`-prefixed Ethereum address stored in lowercase.
///
/// # Validation rules
///
/// - Must begin with `0x` or `0X`.
/// - The remaining 40 characters must all be ASCII hex digits (`0-9`, `a-f`,
///   `A-F`).
/// - Mixed-case EIP-55 checksum addresses are accepted; they are stored
///   normalised to lowercase.
///
/// # Serde
///
/// Serialises and deserialises as a plain string (e.g.
/// `"0xabcdef1234567890abcdef1234567890abcdef12"`).
///
/// # Examples
///
/// ```
/// use mcs_domain::EvmAddress;
///
/// let addr: EvmAddress = "0xAbCdEf1234567890AbCdEf1234567890AbCdEf12"
///     .parse()
///     .unwrap();
/// assert_eq!(addr.to_string(), "0xabcdef1234567890abcdef1234567890abcdef12");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EvmAddress(String);

impl EvmAddress {
    /// Returns the validated address string (always lowercase, `0x`-prefixed).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parses and validates an Ethereum address from a string slice.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::InvalidAddress`] when the input does not satisfy
    /// the validation rules.
    fn parse(s: &str) -> Result<Self, DomainError> {
        let hex = s
            .strip_prefix("0x")
            .or_else(|| s.strip_prefix("0X"))
            .ok_or_else(|| DomainError::InvalidAddress(s.to_owned()))?;

        if hex.len() != 40 {
            return Err(DomainError::InvalidAddress(s.to_owned()));
        }

        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(DomainError::InvalidAddress(s.to_owned()));
        }

        Ok(Self(format!("0x{}", hex.to_ascii_lowercase())))
    }
}

impl fmt::Display for EvmAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for EvmAddress {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<&str> for EvmAddress {
    type Error = DomainError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Self::parse(s)
    }
}

impl Serialize for EvmAddress {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for EvmAddress {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "0xAbCdEf1234567890AbCdEf1234567890AbCdEf12";
    const VALID_LOWER: &str = "0xabcdef1234567890abcdef1234567890abcdef12";

    #[test]
    fn valid_address_is_normalised_to_lowercase() {
        let addr: EvmAddress = VALID.parse().unwrap();
        assert_eq!(addr.to_string(), VALID_LOWER);
    }

    #[test]
    fn lowercase_input_accepted() {
        let addr: EvmAddress = VALID_LOWER.parse().unwrap();
        assert_eq!(addr.as_str(), VALID_LOWER);
    }

    #[test]
    fn try_from_str_works() {
        let addr = EvmAddress::try_from(VALID).unwrap();
        assert_eq!(addr.to_string(), VALID_LOWER);
    }

    #[test]
    fn missing_0x_prefix_is_rejected() {
        let err = "abcdef1234567890abcdef1234567890abcdef12"
            .parse::<EvmAddress>()
            .unwrap_err();
        assert!(matches!(err, DomainError::InvalidAddress(_)));
    }

    #[test]
    fn wrong_length_is_rejected() {
        // 39 hex chars after 0x
        let err = "0xabcdef1234567890abcdef1234567890abcde"
            .parse::<EvmAddress>()
            .unwrap_err();
        assert!(matches!(err, DomainError::InvalidAddress(_)));

        // 41 hex chars after 0x
        let err = "0xabcdef1234567890abcdef1234567890abcdef123"
            .parse::<EvmAddress>()
            .unwrap_err();
        assert!(matches!(err, DomainError::InvalidAddress(_)));
    }

    #[test]
    fn non_hex_chars_are_rejected() {
        let err = "0xGGGGGG1234567890abcdef1234567890abcdef12"
            .parse::<EvmAddress>()
            .unwrap_err();
        assert!(matches!(err, DomainError::InvalidAddress(_)));
    }

    #[test]
    fn empty_string_is_rejected() {
        let err = "".parse::<EvmAddress>().unwrap_err();
        assert!(matches!(err, DomainError::InvalidAddress(_)));
    }

    #[test]
    fn serde_round_trip() {
        let addr: EvmAddress = VALID.parse().unwrap();
        let json = serde_json::to_string(&addr).unwrap();
        let back: EvmAddress = serde_json::from_str(&json).unwrap();
        assert_eq!(addr, back);
        // Must be stored as lowercase in the JSON string.
        assert_eq!(json, format!("\"{VALID_LOWER}\""));
    }

    #[test]
    fn deserialize_invalid_address_returns_error() {
        let bad = "\"not-an-address\"";
        assert!(serde_json::from_str::<EvmAddress>(bad).is_err());
    }
}
