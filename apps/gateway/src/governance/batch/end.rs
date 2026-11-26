//! How a batch envelope's execution ends when it is closed at zero.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvelopeEnd {
    /// Its lines ran (a gateway-run batch's runner closes it).
    Succeeded,
    /// The client cancelled it before anything was sent.
    Cancelled,
    /// It ended before anything was sent, without a client's cancellation
    /// (a native batch whose submission expired).
    Failed,
}
impl EnvelopeEnd {
    pub(super) fn state(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}
