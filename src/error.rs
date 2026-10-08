use crate::ChildId;
use std::time::Duration;

/// A boxed error type for errors raised in calling code
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Error type returned from supervised processes and supervisors
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SupervisorError {
    /// Error setting up signal listeners
    #[error("signal listener setup failed: {0}")]
    Signal(#[from] std::io::Error),
    /// Error from within the managed process
    #[error("task failed: {0}")]
    Process(#[source] BoxError),
    /// The managed process panicked
    #[error("task panicked: {0}")]
    Panic(String),
    /// The child did not stop within its shutdown timeout and was killed
    #[error("task did not stop within {0:?} and was killed")]
    ShutdownTimeout(Duration),
    /// A child returned an error while the supervisor was stopping it
    #[error("child `{id}` failed while stopping")]
    Child {
        id: ChildId,
        #[source]
        source: Box<SupervisorError>,
    },
    /// A child failed to start while the supervisor was starting up
    #[error("child `{id}` failed to start")]
    StartFailed {
        id: ChildId,
        #[source]
        source: Box<SupervisorError>,
    },
    /// Restarts exceeded the intensity, or a needed restart was not possible.
    /// `source` is the error of child `id`, or `None` after a normal exit.
    #[error("reached max restart intensity after child `{id}` exited")]
    MaxIntensity {
        id: ChildId,
        #[source]
        source: Option<Box<SupervisorError>>,
    },
    /// The supervisor configuration is not valid
    #[error("invalid child spec `{id}`: {reason}")]
    InvalidSpec { id: ChildId, reason: &'static str },
}

impl SupervisorError {
    /// Wraps any error as [`SupervisorError::Process`], e.g. `fut.map_err(SupervisorError::from_err)`.
    /// A `SupervisorError` passes through unchanged instead of being wrapped again.
    pub fn from_err<E: Into<BoxError>>(err: E) -> Self {
        match err.into().downcast::<SupervisorError>() {
            Ok(err) => *err,
            Err(other) => Self::Process(other),
        }
    }

    pub(crate) fn from_panic(payload: Box<dyn std::any::Any + Send>) -> Self {
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".to_string());
        Self::Panic(message)
    }
}

impl From<BoxError> for SupervisorError {
    fn from(err: BoxError) -> Self {
        Self::Process(err)
    }
}

/// Result type for supervised process operations
pub type ProcResult = Result<(), SupervisorError>;
