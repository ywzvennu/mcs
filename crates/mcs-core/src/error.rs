//! Error types shared by every variant and by the core machinery.

use thiserror::Error;

/// The error type returned by core operations and by variant implementations.
///
/// Variants are expected to map their own internal errors onto these cases.
/// When no existing case fits, [`GameError::Other`] carries a free-form
/// message so a variant can surface a domain-specific failure without the core
/// crate needing to know about it.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum GameError {
    /// No variant is registered under the requested identifier.
    #[error("unknown variant: {0}")]
    UnknownVariant(String),

    /// A player attempted to act when it was not their turn.
    #[error("it is not your turn to act")]
    NotYourTurn,

    /// The submitted action is well-formed but not legal in the current
    /// position (e.g. moving into check, or sensing an off-board square).
    #[error("illegal action")]
    IllegalAction,

    /// An action was submitted to a game that has already finished.
    #[error("the game is already finished")]
    Finished,

    /// An action payload could not be interpreted as the action type the
    /// variant expected. The string describes what went wrong.
    #[error("invalid action payload: {0}")]
    InvalidActionPayload(String),

    /// A type-erased payload could not be serialized to or deserialized from
    /// its strongly typed representation.
    #[error("serialization error: {0}")]
    Serialization(String),

    /// A variant-specific failure that does not fit any other case.
    #[error("{0}")]
    Other(String),
}
