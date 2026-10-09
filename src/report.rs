use crate::{event::EventHook, ChildId, EventKind, SupervisorEvent};

/// Sends supervisor events to the `on_event` hook and, with the `tracing` feature, to `tracing`
#[derive(Default, Clone)]
pub(crate) struct Reporter {
    pub(crate) name: Option<String>,
    pub(crate) hook: Option<EventHook>,
}

impl Reporter {
    pub(crate) fn emit(&self, child: &ChildId, kind: EventKind<'_>) {
        let event = SupervisorEvent {
            supervisor: self.name.as_deref(),
            child,
            kind,
        };
        #[cfg(feature = "tracing")]
        trace(&event);
        if let Some(hook) = &self.hook {
            hook(&event);
        }
    }
}

/// Fixed message text per event kind, so log-based alerts can match on it
#[cfg(feature = "tracing")]
fn trace(event: &SupervisorEvent<'_>) {
    let supervisor = event.supervisor;
    let child = event.child.as_str();
    match event.kind {
        EventKind::Started => tracing::info!(supervisor, child, "child started"),
        EventKind::Exited { error: None } => tracing::info!(supervisor, child, "child exited"),
        EventKind::Exited { error: Some(error) } => tracing::warn!(
            supervisor,
            child,
            error = error as &dyn std::error::Error,
            "child exited"
        ),
        EventKind::StartFailed { error } => tracing::error!(
            supervisor,
            child,
            error = error as &dyn std::error::Error,
            "child failed to start"
        ),
        EventKind::Restarting { delay } => tracing::info!(
            supervisor,
            child,
            delay_ms = delay.as_millis() as u64,
            "child restarting"
        ),
        EventKind::Killed { timeout } => tracing::warn!(
            supervisor,
            child,
            timeout_ms = timeout.as_millis() as u64,
            "child killed after shutdown timeout"
        ),
        EventKind::MaxIntensity => {
            tracing::error!(
                supervisor,
                child,
                "supervisor reached max restart intensity"
            )
        }
    }
}
