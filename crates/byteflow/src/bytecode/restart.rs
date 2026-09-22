//! Supervisor restart policy (shared by bytecode `SetRestartPolicy` and host
//! [`crate::Supervisor`] / [`crate::ChildSpec`]).

/// Restart policy consulted when a supervised flow terminates.
///
/// Bytecode [`super::Opcode::SetRestartPolicy`] encodes:
/// `0` = [`Self::Always`], `1` = [`Self::OnFailure`], `2` = [`Self::Never`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartPolicy {
    Always,
    OnFailure,
    Never,
}

impl RestartPolicy {
    /// Encode as the `SetRestartPolicy` immediate (`0..=2`).
    #[inline]
    pub fn as_u8(self) -> u8 {
        match self {
            RestartPolicy::Always => 0,
            RestartPolicy::OnFailure => 1,
            RestartPolicy::Never => 2,
        }
    }

    /// Decode a `SetRestartPolicy` immediate.
    #[inline]
    pub fn from_u8(byte: u8) -> Option<RestartPolicy> {
        match byte {
            0 => Some(RestartPolicy::Always),
            1 => Some(RestartPolicy::OnFailure),
            2 => Some(RestartPolicy::Never),
            _ => None,
        }
    }
}
