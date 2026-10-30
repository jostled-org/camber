/// Why a service turn was not granted.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum TurnRefused {
    /// The caller's absolute wait deadline expired.
    #[error("turn wait deadline expired")]
    Deadline,
    /// The caller's cancellation future completed.
    #[error("turn wait cancelled")]
    Cancelled,
    /// The turn owner closed admission.
    #[error("turn admission closed")]
    Closed,
}
