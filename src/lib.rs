mod ordered_select_all;

use crate::ordered_select_all::ordered_select_all;
use futures::{future::BoxFuture, Future, FutureExt, StreamExt, TryFutureExt};
use std::{
    pin::{pin, Pin},
    task::{Context, Poll},
};
use tokio::signal;
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

/// A boxed error type for errors raised in calling code
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Error type returned from task operations
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    /// Error setting up signal listeners
    #[error("signal listener setup failed: {0}")]
    Signal(#[from] std::io::Error),
    /// Error from within the managed process
    #[error("task failed: {0}")]
    Process(#[source] BoxError),
}

impl SupervisorError {
    /// Creates a `SupervisorError::Process` from any error type that can be converted to [`BoxError`].
    ///
    /// This is useful in `map_err` calls to convert user process errors:
    ///
    /// ```ignore
    /// my_future.map_err(SupervisorError::from_err)
    /// ```
    pub fn from_err<E: Into<BoxError>>(err: E) -> Self {
        Self::Process(err.into())
    }
}

impl From<BoxError> for SupervisorError {
    fn from(err: BoxError) -> Self {
        Self::Process(err)
    }
}

/// Result type for supervised process operations
pub type ProcResult = Result<(), SupervisorError>;

/// The return type for managed processes
///
/// A boxed future that returns [`super_visor::Result`] when complete.
/// Processes should return `Ok(())` on successful shutdown or an error if something went wrong
pub type ManagedFuture = futures::future::BoxFuture<'static, ProcResult>;

/// An awaitable construct for signalling shutdown to supervised processes
/// to complete work and exit on the next available await point
/// Lazily instantiates the future when the future is first polled to allow cloning the underlying
/// token and propagating shutdown signals from a single source in a  one-to-many relationship
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
        // Lazy init the future on first poll
        if self.future.is_none() {
            self.future = Some(Box::pin(self.token.clone().cancelled_owned()));
        }

        // Poll the cached future
        self.future.as_mut().unwrap().as_mut().poll(cx)
    }
}

impl Clone for ShutdownSignal {
    fn clone(&self) -> Self {
        Self {
            token: self.token.clone(),
            future: None,
        }
    }
}

impl Unpin for ShutdownSignal {}

/// Spawns a future ito its own Tokio task.
///
/// Use this in [`ManagedProc::start_task`] implementations when your future
/// is `Send + 'static`. This is the preferred approach as it allows the process
/// to run independently on the Tokio runtime.
///
/// # Example
///
/// ```ignore
/// impl ManagedProc for MyDaemon {
///     fn run_proc(self: Box<Self>, shutdown: ShutdownSignal) -> ManagedFuture {
///         super_visor::spawn(self.run(shutdown))
///     }
/// }
/// ```
pub fn spawn<F, E>(fut: F) -> ManagedFuture
where
    F: Future<Output = Result<(), E>> + Send + 'static,
    E: Into<BoxError> + Send + 'static,
{
    // tokio::spawn returns Result<Result<(), E>, JoinError>
    Box::pin(tokio::spawn(fut).map(|result| match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(SupervisorError::from_err(e)),
        Err(e) => Err(SupervisorError::from_err(e)),
    }))
}

/// Boxes a future without spawning a separate managed task
///
/// Use this in [`ManagedProc::start_task`] impls when you want to run the future
/// directly rather than spawning it. Prefer [`spawn`] when possible, as spawned
/// tasks can run more efficiently on the Tokio runtime
///
/// # Example
///
/// ```ignore
/// impl ManagedProc for MyLocalDaemon {
///     fn start_task(self: Box<Self>, shutdown: ShutdownSignal) -> ManagedFuture {
///         super_visor::run(self.run(shutdown))
///     }
/// }
/// ```
pub fn run<F, E>(fut: F) -> ManagedFuture
where
    F: Future<Output = Result<(), E>> + Send + 'static,
    E: Into<BoxError> + 'static,
{
    Box::pin(fut.map_err(SupervisorError::from_err))
}

/// A trait for types that can be managed as long-running async tasks.
///
/// Implement this trait to make your type usable with [`Supervisor`].
/// The trait is also automatically implemented for closures of the form
/// `FnOnce(ShutdownSignal) -> Future<Output = ProcResult>`.
///
/// # Example
///
/// ```ignore
/// use super_visor::{ManagedProc, ManagedFuture};
///
/// struct MyDaemon { /* ... */ }
///
/// impl ManagedProc for MyDaemon {
///     fn start_task(self: Box<Self>, shutdown: ShutdownSignal) -> ManaagedFuture {
///         super_visor::spawn(self.run_task_logic_in_some_loop(shutdown))
///     }
/// }
/// ```
pub trait ManagedProc: Send + Sync {
    /// Starts the process and returns a future that completes when the work is complete
    /// or runs indefinitely in a continual loop.
    ///
    /// The `shutdown` listener will be triggered when the supervisor wants to shut down
    /// the process. Implementations should listen for this signal and clean up gracefully.
    /// Listening typically involves awaiting the shutdown signal alongside the primary operational
    /// logic of the managed task in a select function or macro, or checking the signal has completed
    /// or been cancelled at await points in the control loop
    fn run_proc(self: Box<Self>, shutdown: ShutdownSignal) -> ManagedFuture;
}

/// Manages the lifecycle of multiple async workers with coordinated, ordered shutdown.
///
/// `Supervisor` runs all registered processes concurrently and handles graceful shutdown
/// when receiving SIGTERM or Ctrl+C. Tasks are shutdown in reverse order of their initial
/// registration (LIFO), allowing dependent tasks to stop their dependencies. This mimics the
/// Patterns of process management hierarchy and dependency familiar in the OTP Erlang runtime.
///
/// # Example
///
/// ```ignore
/// use super_visor::Supervisor;
///
/// // Using the builder pattern
/// Supervisor::builder()
///     .add_proc(server)
///     .add_proc(worker)
///     .build()
///     .start()
///     .await?;
///
/// // Or using direct construction
/// let mut supervisor = Supervisor::new();
/// supervisor.add(server);
/// supervisor.add(worker);
/// supervisor.start().await?;
/// ```
pub struct Supervisor {
    procs: Vec<Box<dyn ManagedProc>>,
}

impl ManagedProc for Supervisor {
    fn run_proc(self: Box<Self>, shutdown: ShutdownSignal) -> ManagedFuture {
        crate::run(self.do_run(Box::pin(shutdown)))
    }
}

/// Builder for constructing a [`Supervisor`].
///
/// # Example
///
/// ```ignore
/// use super_visor::Supervisor;
///
/// let supervisor = Supervisor::builder()
///     .add_proc(server)
///     .add_proc(worker)
///     .add_proc(sink)
///     .build();
/// ```
pub struct SupervisorBuilder {
    procs: Vec<Box<dyn ManagedProc>>,
}

struct CancelableLocalFuture {
    cancel_token: CancellationToken,
    future: ManagedFuture,
}

impl Future for CancelableLocalFuture {
    type Output = ProcResult;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        pin!(&mut self.future).poll(ctx)
    }
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

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor {
    /// Creates a new empty supervisor.
    pub fn new() -> Self {
        Self { procs: Vec::new() }
    }

    /// Creates a new [`SupervisorBuilder`] for fluent task registration.
    pub fn builder() -> SupervisorBuilder {
        SupervisorBuilder { procs: Vec::new() }
    }

    /// Adds a task to the supervisor
    ///
    /// Tasks are started in the order they are added and shutdown in the reverse order.
    pub fn add(&mut self, proc: impl ManagedProc + 'static) {
        self.procs.push(Box::new(proc));
    }

    /// Starts all registered processes and waits for completion or shutdown.
    ///
    /// This method:
    /// 1. Starts all processes concurrently
    /// 2. Listens for SIGTERM or Ctrl+C signals
    /// 3. On signal or error, shuts down all running processes in reverse order (LIFO)
    /// 4. Returns the first error encountered or `Ok(())` if all tasks complete successfully
    pub async fn start(self) -> ProcResult {
        let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())?;
        let shutdown = Box::pin(
            futures::future::select(
                Box::pin(async move { sigterm.recv().await }),
                Box::pin(signal::ctrl_c()),
            )
            .map(|_| ()),
        );
        self.do_run(shutdown).await
    }

    async fn do_run(self, mut shutdown: BoxFuture<'static, ()>) -> ProcResult {
        let mut futures = start_futures(self.procs);

        loop {
            if futures.is_empty() {
                break;
            }

            let mut select = ordered_select_all(futures);

            tokio::select! {
                biased;
                _ = &mut shutdown => return stop_all(select.into_inner()).await,
                (result, _index, remaining) = &mut select => match result {
                    Ok(_) => futures = remaining,
                    Err(err) => {
                        let _ = stop_all(remaining).await;
                        return Err(err);
                    }
                }
            }
        }

        Ok(())
    }
}

impl SupervisorBuilder {
    /// Adds a process to the builder
    ///
    /// Processes are started in the order they are registered and shut down in reverse order.
    pub fn add_proc(mut self, proc: impl ManagedProc + 'static) -> Self {
        self.procs.push(Box::new(proc));
        self
    }

    /// Consumes the builder and returns a configured [`Supervisor`].
    pub fn build(self) -> Supervisor {
        Supervisor { procs: self.procs }
    }
}

fn start_futures(procs: Vec<Box<dyn ManagedProc>>) -> Vec<CancelableLocalFuture> {
    procs
        .into_iter()
        .map(|proc| {
            let cancel_token = CancellationToken::new();
            let child_token = cancel_token.child_token();
            CancelableLocalFuture {
                cancel_token,
                future: proc.run_proc(ShutdownSignal::new(child_token)),
            }
        })
        .collect()
}

async fn stop_all(procs: Vec<CancelableLocalFuture>) -> ProcResult {
    futures::stream::iter(procs.into_iter().rev())
        .then(|proc| async move {
            proc.cancel_token.cancel();
            proc.future.await
        })
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[allow(dead_code)]
    fn assert_send_sync() {
        fn is_send<T: Send>() {}
        fn is_sync<T: Sync>() {}
        is_send::<Supervisor>();
        is_sync::<Supervisor>();
    }

    #[derive(Debug)]
    struct TestError(&'static str);

    impl std::fmt::Display for TestError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    impl std::error::Error for TestError {}

    fn test_err(msg: &'static str) -> SupervisorError {
        SupervisorError::from_err(TestError(msg))
    }

    struct TestProc {
        name: &'static str,
        delay: u64,
        result: ProcResult,
        sender: mpsc::Sender<&'static str>,
    }

    impl ManagedProc for TestProc {
        fn run_proc(self: Box<Self>, shutdown: ShutdownSignal) -> ManagedFuture {
            let handle = tokio::spawn(async move {
                tokio::select! {
                    _ = shutdown => (),
                    _ = tokio::time::sleep(std::time::Duration::from_millis(self.delay)) => (),
                }
                self.sender.send(self.name).await.expect("unable to send");
                self.result
            });

            Box::pin(handle.map(|result| match result {
                Ok(inner) => inner,
                Err(e) => Err(SupervisorError::from_err(e)),
            }))
        }
    }

    #[tokio::test]
    async fn stop_when_all_tasks_have_completed() {
        let (sender, mut receiver) = mpsc::channel(5);

        let result = Supervisor::builder()
            .add_proc(TestProc {
                name: "1",
                delay: 50,
                result: Ok(()),
                sender: sender.clone(),
            })
            .add_proc(TestProc {
                name: "2",
                delay: 100,
                result: Ok(()),
                sender: sender.clone(),
            })
            .build()
            .start()
            .await;

        assert_eq!(Some("1"), receiver.recv().await);
        assert_eq!(Some("2"), receiver.recv().await);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn will_stop_all_in_reverse_order_after_error() {
        let (sender, mut receiver) = mpsc::channel(5);

        let result = Supervisor::builder()
            .add_proc(TestProc {
                name: "1",
                delay: 1000,
                result: Ok(()),
                sender: sender.clone(),
            })
            .add_proc(TestProc {
                name: "2",
                delay: 50,
                result: Err(test_err("error")),
                sender: sender.clone(),
            })
            .add_proc(TestProc {
                name: "3",
                delay: 1000,
                result: Ok(()),
                sender: sender.clone(),
            })
            .build()
            .start()
            .await;

        assert_eq!(Some("2"), receiver.recv().await);
        assert_eq!(Some("3"), receiver.recv().await);
        assert_eq!(Some("1"), receiver.recv().await);
        assert_eq!("task failed: error", result.unwrap_err().to_string());
    }

    #[tokio::test]
    async fn will_return_first_error_returned() {
        let (sender, mut receiver) = mpsc::channel(5);

        let result = Supervisor::builder()
            .add_proc(TestProc {
                name: "1",
                delay: 1000,
                result: Ok(()),
                sender: sender.clone(),
            })
            .add_proc(TestProc {
                name: "2",
                delay: 50,
                result: Err(test_err("error")),
                sender: sender.clone(),
            })
            .add_proc(TestProc {
                name: "3",
                delay: 200,
                result: Err(test_err("second error")),
                sender: sender.clone(),
            })
            .build()
            .start()
            .await;

        assert_eq!(Some("2"), receiver.recv().await);
        assert_eq!(Some("3"), receiver.recv().await);
        assert_eq!(Some("1"), receiver.recv().await);
        assert_eq!("task failed: error", result.unwrap_err().to_string());
    }

    #[tokio::test]
    async fn nested_procs_will_stop_parent_then_move_up() {
        let (sender, mut receiver) = mpsc::channel(10);

        let result = Supervisor::builder()
            .add_proc(TestProc {
                name: "proc-1",
                delay: 500,
                result: Ok(()),
                sender: sender.clone(),
            })
            .add_proc(
                Supervisor::builder()
                    .add_proc(TestProc {
                        name: "proc-2-1",
                        delay: 500,
                        result: Ok(()),
                        sender: sender.clone(),
                    })
                    .add_proc(TestProc {
                        name: "proc-2-2",
                        delay: 100,
                        result: Err(test_err("error")),
                        sender: sender.clone(),
                    })
                    .add_proc(TestProc {
                        name: "proc-2-3",
                        delay: 500,
                        result: Ok(()),
                        sender: sender.clone(),
                    })
                    .add_proc(TestProc {
                        name: "proc-2-4",
                        delay: 500,
                        result: Ok(()),
                        sender: sender.clone(),
                    })
                    .build(),
            )
            .add_proc(
                Supervisor::builder()
                    .add_proc(TestProc {
                        name: "proc-3-1",
                        delay: 1000,
                        result: Ok(()),
                        sender: sender.clone(),
                    })
                    .add_proc(TestProc {
                        name: "proc-3-2",
                        delay: 1000,
                        result: Ok(()),
                        sender: sender.clone(),
                    })
                    .add_proc(TestProc {
                        name: "proc-3-3",
                        delay: 1000,
                        result: Ok(()),
                        sender: sender.clone(),
                    })
                    .build(),
            )
            .build()
            .start()
            .await;

        assert_eq!(Some("proc-2-2"), receiver.recv().await);
        assert_eq!(Some("proc-2-4"), receiver.recv().await);
        assert_eq!(Some("proc-2-3"), receiver.recv().await);
        assert_eq!(Some("proc-2-1"), receiver.recv().await);
        assert_eq!(Some("proc-3-3"), receiver.recv().await);
        assert_eq!(Some("proc-3-2"), receiver.recv().await);
        assert_eq!(Some("proc-3-1"), receiver.recv().await);
        assert_eq!(Some("proc-1"), receiver.recv().await);
        assert!(result.is_err());
    }
}
