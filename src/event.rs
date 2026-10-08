use crate::{ChildId, SupervisorError};
use std::{sync::Arc, time::Duration};

/// A report of a supervisor decision (like Erlang's SASL supervisor reports)
#[derive(Debug)]
pub struct SupervisorEvent<'a> {
    /// The supervisor's name, if one was set with `SupervisorBuilder::name`
    pub supervisor: Option<&'a str>,
    pub child: &'a ChildId,
    pub kind: EventKind<'a>,
}

#[derive(Debug)]
#[non_exhaustive]
pub enum EventKind<'a> {
    /// The child started (after its init completed)
    Started,
    /// The child exited when no shutdown was signaled; `error` is `None` for a normal exit
    Exited { error: Option<&'a SupervisorError> },
    /// The child failed to start
    StartFailed { error: &'a SupervisorError },
    /// The child will start again after `delay`
    Restarting { delay: Duration },
    /// The child did not stop within its shutdown timeout and was killed
    Killed { timeout: Duration },
    /// The restart caused by this child exceeded the intensity; the supervisor gives up
    MaxIntensity,
}

pub(crate) type EventHook = Arc<dyn Fn(&SupervisorEvent<'_>) + Send + Sync>;
