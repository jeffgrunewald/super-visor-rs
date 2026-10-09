use crate::{BoxError, ManagedProc, SupervisorError};
use futures::{future::BoxFuture, FutureExt, TryFutureExt};
use std::{borrow::Cow, fmt, future::Future, sync::Arc, time::Duration};

/// Unique name of a child within its supervisor
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChildId(Cow<'static, str>);

impl ChildId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ChildId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&'static str> for ChildId {
    fn from(id: &'static str) -> Self {
        Self(Cow::Borrowed(id))
    }
}

impl From<String> for ChildId {
    fn from(id: String) -> Self {
        Self(Cow::Owned(id))
    }
}

/// When a child is restarted after it exits (Erlang `restart` child spec key)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Restart {
    /// Always restarted, also after a normal exit
    #[default]
    Permanent,
    /// Restarted only after an abnormal exit (`Err` or panic)
    Transient,
    /// Never restarted
    Temporary,
}

/// How a child is stopped (Erlang `shutdown` child spec key)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shutdown {
    /// Signal shutdown, then kill the child if it has not stopped within the duration
    Timeout(Duration),
    /// Kill the child at once without a shutdown signal (Erlang `brutal_kill`)
    Kill,
    /// Signal shutdown and wait without a limit; use for nested supervisors
    Infinity,
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::Timeout(Duration::from_secs(5))
    }
}

/// Which children restart when one child must restart
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Strategy {
    /// Restart only the child that exited
    #[default]
    OneForOne,
    /// Stop all other children, then restart all of them
    OneForAll,
    /// Stop the children started after the exited child, then restart them in order
    RestForOne,
}

/// Max restarts allowed within a period before the supervisor gives up (MaxR within MaxT)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Intensity {
    pub max_restarts: u32,
    pub period: Duration,
}

impl Intensity {
    pub fn new(max_restarts: u32, period: Duration) -> Self {
        Self {
            max_restarts,
            period,
        }
    }
}

impl Default for Intensity {
    /// Erlang's default: 1 restart in 5 seconds
    fn default() -> Self {
        Self::new(1, Duration::from_secs(5))
    }
}

/// Delay before a restart; an extension, Erlang restarts at once
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backoff {
    #[default]
    None,
    Fixed(Duration),
    /// Doubles with each restart in the current intensity period, up to `max`
    Exponential {
        initial: Duration,
        max: Duration,
    },
}

impl Backoff {
    pub(crate) fn delay(&self, restarts_in_period: usize) -> Duration {
        match *self {
            Self::None => Duration::ZERO,
            Self::Fixed(delay) => delay,
            Self::Exponential { initial, max } => {
                let exponent = restarts_in_period.saturating_sub(1).min(31) as u32;
                initial.saturating_mul(1u32 << exponent).min(max)
            }
        }
    }
}

/// When the supervisor stops on its own after significant children exit
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutoShutdown {
    /// Run until shutdown is signaled, even with no children left (Erlang's default)
    Never,
    /// Stop all children and return when any significant child has exited for good
    AnySignificant,
    /// Stop all children and return when all significant children have exited for good
    #[default]
    AllSignificant,
}

pub(crate) type StartFuture = BoxFuture<'static, Result<Box<dyn ManagedProc>, SupervisorError>>;
type Factory = Arc<dyn Fn() -> StartFuture + Send + Sync>;

pub(crate) enum Start {
    Factory(Factory),
    Once(Option<Box<dyn ManagedProc>>),
}

/// Describes how to start, restart and stop one child (Erlang child spec)
pub struct ChildSpec {
    pub(crate) id: ChildId,
    pub(crate) start: Start,
    pub(crate) restart: Restart,
    pub(crate) shutdown: Shutdown,
    pub(crate) significant: bool,
}

impl ChildSpec {
    /// A restartable child; the supervisor calls `factory` for each start
    pub fn new<F, P>(id: impl Into<ChildId>, factory: F) -> Self
    where
        F: Fn() -> P + Send + Sync + 'static,
        P: ManagedProc + 'static,
    {
        Self::from_start(
            id,
            Start::Factory(Arc::new(move || {
                let proc: Box<dyn ManagedProc> = Box::new(factory());
                futures::future::ready(Ok(proc)).boxed()
            })),
        )
    }

    /// A restartable child with async setup (Erlang `init/1`). The supervisor waits
    /// for `init` before it starts the next child; an `Err` is a failed start.
    pub fn with_init<F, Fut, P, E>(id: impl Into<ChildId>, init: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<P, E>> + Send + 'static,
        P: ManagedProc + 'static,
        E: Into<BoxError> + 'static,
    {
        Self::from_start(
            id,
            Start::Factory(Arc::new(move || {
                init()
                    .map_ok(|proc| Box::new(proc) as Box<dyn ManagedProc>)
                    .map_err(SupervisorError::from_err)
                    .boxed()
            })),
        )
    }

    /// A nested supervisor (static or dynamic); it stops with [`Shutdown::Infinity`]
    /// so its own children have time to stop by their own policies
    pub fn supervisor<F, S>(id: impl Into<ChildId>, factory: F) -> Self
    where
        F: Fn() -> S + Send + Sync + 'static,
        S: ManagedProc + 'static,
    {
        Self::new(id, factory).shutdown(Shutdown::Infinity)
    }

    /// A child that cannot restart; a restart that needs it exceeds the intensity
    pub fn once(id: impl Into<ChildId>, proc: impl ManagedProc + 'static) -> Self {
        Self::from_start(id, Start::Once(Some(Box::new(proc)))).restart(Restart::Transient)
    }

    fn from_start(id: impl Into<ChildId>, start: Start) -> Self {
        Self {
            id: id.into(),
            start,
            restart: Restart::default(),
            shutdown: Shutdown::default(),
            significant: false,
        }
    }

    pub fn restart(mut self, restart: Restart) -> Self {
        self.restart = restart;
        self
    }

    pub fn shutdown(mut self, shutdown: Shutdown) -> Self {
        self.shutdown = shutdown;
        self
    }

    /// Mark the child significant for [`AutoShutdown`]; it must not be [`Restart::Permanent`]
    pub fn significant(mut self, significant: bool) -> Self {
        self.significant = significant;
        self
    }

    pub fn id(&self) -> &ChildId {
        &self.id
    }

    pub(crate) fn can_restart(&self) -> bool {
        matches!(self.start, Start::Factory(_))
    }

    /// `None` when a once-only child has already started
    pub(crate) fn start_future(&mut self) -> Option<StartFuture> {
        match &mut self.start {
            Start::Factory(factory) => {
                let factory = Arc::clone(factory);
                let start = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| factory()))
                    .unwrap_or_else(|payload| {
                        futures::future::ready(Err(SupervisorError::from_panic(payload))).boxed()
                    });
                Some(start)
            }
            Start::Once(proc) => proc
                .take()
                .map(|proc| futures::future::ready(Ok(proc)).boxed()),
        }
    }
}

impl fmt::Debug for ChildSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChildSpec")
            .field("id", &self.id)
            .field("restartable", &self.can_restart())
            .field("restart", &self.restart)
            .field("shutdown", &self.shutdown)
            .field("significant", &self.significant)
            .finish()
    }
}
