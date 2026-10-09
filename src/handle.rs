use crate::{ChildId, ChildSpec, Restart, SupervisorError};
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::{mpsc, oneshot};

const MAILBOX_CAPACITY: usize = 32;

/// Error from a [`SupervisorHandle`] call (Erlang `{error, Reason}`)
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HandleError {
    /// The supervisor has stopped, or a restart is replacing it
    #[error("supervisor is not running")]
    NotRunning,
    #[error("no child `{0}`")]
    NotFound(ChildId),
    #[error("child `{0}` already exists")]
    AlreadyPresent(ChildId),
    #[error("child `{0}` is running")]
    Running(ChildId),
    #[error("child `{0}` is waiting to restart")]
    Restarting(ChildId),
    #[error("child `{0}` is once-only and has already run")]
    NotRestartable(ChildId),
    #[error("supervisor already has its maximum of {0} children")]
    MaxChildren(usize),
    /// The operation does not apply to this kind of supervisor
    #[error("not supported by this supervisor")]
    NotSupported,
    /// The child spec is not valid, or its start failed; the spec was not kept
    #[error("child failed to start")]
    Start(#[source] SupervisorError),
}

/// What a child is doing now
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChildStatus {
    Starting,
    Running,
    /// Waiting for its restart (backoff), or its restart is in progress
    Restarting,
    /// Shutting down after `terminate_child` (dynamic supervisors only)
    Stopping,
    /// Exited for good or terminated; the spec remains
    Stopped,
}

/// One child in a [`SupervisorHandle::which_children`] answer
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildInfo {
    pub id: ChildId,
    pub status: ChildStatus,
    pub restart: Restart,
}

/// A [`SupervisorHandle::count_children`] answer
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChildCounts {
    /// All child specs the supervisor holds
    pub specs: usize,
    /// Children that are running now
    pub active: usize,
}

pub(crate) type Reply<T> = oneshot::Sender<Result<T, HandleError>>;

pub(crate) enum Command {
    StartChild(ChildSpec, Reply<()>),
    TerminateChild(ChildId, Reply<()>),
    RestartChild(ChildId, Reply<()>),
    DeleteChild(ChildId, Reply<()>),
    WhichChildren(Reply<Vec<ChildInfo>>),
}

/// Changes and inspects a running supervisor's children (Erlang `supervisor:*_child`).
/// Clones share one target; attach it to each instance a factory builds to survive restarts.
#[derive(Clone)]
pub struct SupervisorHandle {
    mailbox: Arc<Mutex<Mailbox>>,
}

struct Mailbox {
    sender: mpsc::Sender<Command>,
    /// The first channel's receiver, kept until a supervisor binds, so early calls wait
    unclaimed: Option<mpsc::Receiver<Command>>,
}

impl Default for SupervisorHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SupervisorHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupervisorHandle")
            .field("open", &!self.sender().is_closed())
            .finish()
    }
}

impl SupervisorHandle {
    /// A handle for a supervisor built later with it; calls made before then wait.
    /// After that supervisor stops, calls fail with [`HandleError::NotRunning`].
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel(MAILBOX_CAPACITY);
        Self {
            mailbox: Arc::new(Mutex::new(Mailbox {
                sender,
                unclaimed: Some(receiver),
            })),
        }
    }

    /// Starts `spec` after the existing children; the call waits for its init
    pub async fn start_child(&self, spec: ChildSpec) -> Result<(), HandleError> {
        self.call(|reply| Command::StartChild(spec, reply)).await
    }

    /// Stops the child by its shutdown policy; a static supervisor keeps the spec
    pub async fn terminate_child(&self, id: impl Into<ChildId>) -> Result<(), HandleError> {
        let id = id.into();
        self.call(|reply| Command::TerminateChild(id, reply)).await
    }

    /// Starts a terminated child again; does not count toward the intensity
    pub async fn restart_child(&self, id: impl Into<ChildId>) -> Result<(), HandleError> {
        let id = id.into();
        self.call(|reply| Command::RestartChild(id, reply)).await
    }

    /// Removes the spec of a child that is not running
    pub async fn delete_child(&self, id: impl Into<ChildId>) -> Result<(), HandleError> {
        let id = id.into();
        self.call(|reply| Command::DeleteChild(id, reply)).await
    }

    pub async fn which_children(&self) -> Result<Vec<ChildInfo>, HandleError> {
        self.call(Command::WhichChildren).await
    }

    pub async fn count_children(&self) -> Result<ChildCounts, HandleError> {
        let children = self.which_children().await?;
        Ok(ChildCounts {
            specs: children.len(),
            active: children
                .iter()
                .filter(|child| child.status == ChildStatus::Running)
                .count(),
        })
    }

    /// Points the handle at a supervisor instance and returns that instance's mailbox.
    /// The first bind takes the queue of early calls; later binds (restarts) start empty.
    pub(crate) fn bind(&self) -> mpsc::Receiver<Command> {
        let mut mailbox = self.lock();
        mailbox.unclaimed.take().unwrap_or_else(|| {
            let (sender, receiver) = mpsc::channel(MAILBOX_CAPACITY);
            mailbox.sender = sender;
            receiver
        })
    }

    fn sender(&self) -> mpsc::Sender<Command> {
        self.lock().sender.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Mailbox> {
        self.mailbox.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn call<T>(&self, command: impl FnOnce(Reply<T>) -> Command) -> Result<T, HandleError> {
        let sender = self.sender();
        let (reply, answer) = oneshot::channel();
        sender
            .send(command(reply))
            .await
            .map_err(|_| HandleError::NotRunning)?;
        answer.await.map_err(|_| HandleError::NotRunning)?
    }
}
