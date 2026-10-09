//! Pieces shared by `Supervisor` and `DynamicSupervisor`

use crate::{
    ChildSpec, Intensity, ManagedFuture, ManagedProc, Restart, ShutdownSignal, SupervisorError,
};
use futures::{future::BoxFuture, FutureExt};
use std::{
    collections::VecDeque,
    panic::{catch_unwind, AssertUnwindSafe},
};
use tokio::time::Instant;

/// Whether a child that exited with `error` must restart under `restart`
pub(crate) fn needs_restart(restart: Restart, error: Option<&SupervisorError>) -> bool {
    match restart {
        Restart::Permanent => true,
        Restart::Transient => error.is_some(),
        Restart::Temporary => false,
    }
}

/// Runs the child's factory or init to completion; a panic is a failed start
pub(crate) async fn start_proc(
    spec: &mut ChildSpec,
) -> Result<Box<dyn ManagedProc>, SupervisorError> {
    let start = spec
        .start_future()
        .ok_or_else(|| SupervisorError::InvalidSpec {
            id: spec.id.clone(),
            reason: "a once-only child cannot start again",
        })?;
    AssertUnwindSafe(start)
        .catch_unwind()
        .await
        .unwrap_or_else(|payload| Err(SupervisorError::from_panic(payload)))
}

/// Runs the process, treating a panic at any point as an abnormal exit
pub(crate) fn launch(proc: Box<dyn ManagedProc>, shutdown: ShutdownSignal) -> ManagedFuture {
    match catch_unwind(AssertUnwindSafe(|| proc.run_proc(shutdown))) {
        Ok(future) => AssertUnwindSafe(future)
            .catch_unwind()
            .map(|result| {
                result.unwrap_or_else(|payload| Err(SupervisorError::from_panic(payload)))
            })
            .boxed(),
        Err(payload) => futures::future::ready(Err(SupervisorError::from_panic(payload))).boxed(),
    }
}

/// Restart timestamps inside the intensity period (MaxR within MaxT)
pub(crate) struct RestartWindow {
    intensity: Intensity,
    restarts: VecDeque<Instant>,
}

impl RestartWindow {
    pub(crate) fn new(intensity: Intensity) -> Self {
        Self {
            intensity,
            restarts: VecDeque::new(),
        }
    }

    /// Records a restart at `now`; returns the number of restarts in the period
    pub(crate) fn record(&mut self, now: Instant) -> usize {
        self.restarts.push_back(now);
        let period = self.intensity.period;
        while self
            .restarts
            .front()
            .is_some_and(|at| now.duration_since(*at) > period)
        {
            self.restarts.pop_front();
        }
        self.restarts.len()
    }

    pub(crate) fn exceeded(&self) -> bool {
        self.restarts.len() > self.intensity.max_restarts as usize
    }
}

pub(crate) fn os_shutdown_signal() -> Result<BoxFuture<'static, ()>, SupervisorError> {
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        Ok(async move {
            tokio::select! {
                _ = sigterm.recv() => (),
                _ = tokio::signal::ctrl_c() => (),
            }
        }
        .boxed())
    }
    #[cfg(not(unix))]
    {
        Ok(async {
            let _ = tokio::signal::ctrl_c().await;
        }
        .boxed())
    }
}
