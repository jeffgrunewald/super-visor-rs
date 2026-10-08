#![doc = include_str!("../README.md")]

mod error;
mod event;
mod spec;
mod supervisor;

pub use error::{BoxError, ProcResult, SupervisorError};
pub use event::{EventKind, SupervisorEvent};
pub use spec::{AutoShutdown, Backoff, ChildId, ChildSpec, Intensity, Restart, Shutdown, Strategy};
pub use supervisor::{Supervisor, SupervisorBuilder};
pub use tokio_util::sync::CancellationToken;

use futures::{Future, FutureExt, TryFutureExt};
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tokio_util::{sync::WaitForCancellationFutureOwned, task::AbortOnDropHandle};

/// The future a managed process returns; `Ok(())` is a normal exit, `Err` an abnormal one
pub type ManagedFuture = futures::future::BoxFuture<'static, ProcResult>;

/// Resolves when the supervisor asks the process to stop; clones share one source.
/// The inner future is created on first poll, so a clone is cheap.
pub struct ShutdownSignal {
    token: CancellationToken,
    future: Option<Pin<Box<WaitForCancellationFutureOwned>>>,
}

impl ShutdownSignal {
    pub fn new(token: CancellationToken) -> Self {
        Self {
            token,
            future: None,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    pub fn token(&self) -> &CancellationToken {
        &self.token
    }
}

impl Future for ShutdownSignal {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        let token = &this.token;
        this.future
            .get_or_insert_with(|| Box::pin(token.clone().cancelled_owned()))
            .as_mut()
            .poll(cx)
    }
}

impl Clone for ShutdownSignal {
    fn clone(&self) -> Self {
        Self::new(self.token.clone())
    }
}

impl Unpin for ShutdownSignal {}

/// Spawns `fut` as its own Tokio task; the preferred body for [`ManagedProc::run_proc`].
/// Dropping the returned future aborts the task, so a supervisor can kill it.
pub fn spawn<F, E>(fut: F) -> ManagedFuture
where
    F: Future<Output = Result<(), E>> + Send + 'static,
    E: Into<BoxError> + Send + 'static,
{
    Box::pin(
        AbortOnDropHandle::new(tokio::spawn(fut)).map(|result| match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(SupervisorError::from_err(err)),
            Err(err) if err.is_panic() => Err(SupervisorError::from_panic(err.into_panic())),
            Err(err) => Err(SupervisorError::from_err(err)),
        }),
    )
}

/// Boxes `fut` to run inside the supervisor's own task instead of a spawned one.
/// Prefer [`spawn`]: an unspawned child only runs while the supervisor polls it.
pub fn run<F, E>(fut: F) -> ManagedFuture
where
    F: Future<Output = Result<(), E>> + Send + 'static,
    E: Into<BoxError> + 'static,
{
    Box::pin(fut.map_err(SupervisorError::from_err))
}

/// A long-running async process that a [`Supervisor`] can start and stop.
/// Implemented for closures `FnOnce(ShutdownSignal) -> impl Future<Output = ProcResult>`.
pub trait ManagedProc: Send + Sync {
    /// Starts the process; it must stop and return soon after `shutdown` resolves
    fn run_proc(self: Box<Self>, shutdown: ShutdownSignal) -> ManagedFuture;
}

impl<O, P> ManagedProc for P
where
    O: Future<Output = ProcResult> + Send + 'static,
    P: FnOnce(ShutdownSignal) -> O + Send + Sync,
{
    fn run_proc(self: Box<Self>, shutdown: ShutdownSignal) -> ManagedFuture {
        Box::pin(self(shutdown))
    }
}
