use crate::{
    handle::{ChildInfo, ChildStatus, Command, HandleError, Reply, SupervisorHandle},
    report::Reporter,
    runtime::{launch, needs_restart, os_shutdown_signal, start_proc, RestartWindow},
    Backoff, ChildId, ChildSpec, EventKind, Intensity, ManagedFuture, ManagedProc, ProcResult,
    Shutdown, ShutdownSignal, SupervisorError, SupervisorEvent,
};
use futures::future::BoxFuture;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::mpsc,
    task::{AbortHandle, Id, JoinError, JoinSet},
    time::{sleep_until, Instant},
};
use tokio_util::sync::CancellationToken;

/// Supervises children added at runtime through its [`SupervisorHandle`] (Elixir `DynamicSupervisor`).
/// Each child runs in its own task and restarts on its own (one-for-one); shutdown is concurrent.
pub struct DynamicSupervisor {
    config: Config,
    handle: SupervisorHandle,
    commands: mpsc::Receiver<Command>,
}

#[derive(Default)]
struct Config {
    reporter: Reporter,
    intensity: Intensity,
    backoff: Backoff,
    max_children: Option<usize>,
}

/// Builder for a [`DynamicSupervisor`]
pub struct DynamicSupervisorBuilder {
    supervisor: DynamicSupervisor,
}

impl Default for DynamicSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl DynamicSupervisor {
    pub fn new() -> Self {
        let handle = SupervisorHandle::new();
        Self {
            config: Config::default(),
            commands: handle.bind(),
            handle,
        }
    }

    pub fn builder() -> DynamicSupervisorBuilder {
        DynamicSupervisorBuilder {
            supervisor: Self::new(),
        }
    }

    /// A handle to this supervisor; calls made before it starts wait until it is running
    pub fn handle(&self) -> SupervisorHandle {
        self.handle.clone()
    }

    /// Runs until SIGTERM or Ctrl+C, or until the restart intensity is exceeded
    pub async fn start(self) -> ProcResult {
        let shutdown = os_shutdown_signal()?;
        self.run_until(shutdown).await
    }

    async fn run_until(self, shutdown: BoxFuture<'static, ()>) -> ProcResult {
        Runtime::new(self).supervise(shutdown).await
    }
}

impl ManagedProc for DynamicSupervisor {
    fn run_proc(self: Box<Self>, shutdown: ShutdownSignal) -> ManagedFuture {
        Box::pin(self.run_until(Box::pin(shutdown)))
    }
}

impl DynamicSupervisorBuilder {
    /// Names the supervisor in its events
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.supervisor.config.reporter.name = Some(name.into());
        self
    }

    pub fn intensity(mut self, intensity: Intensity) -> Self {
        self.supervisor.config.intensity = intensity;
        self
    }

    pub fn backoff(mut self, backoff: Backoff) -> Self {
        self.supervisor.config.backoff = backoff;
        self
    }

    /// Refuses `start_child` with [`HandleError::MaxChildren`] at this many children
    pub fn max_children(mut self, max_children: usize) -> Self {
        self.supervisor.config.max_children = Some(max_children);
        self
    }

    /// Calls `hook` for each [`SupervisorEvent`]; keep it fast, it runs in the supervisor's task
    pub fn on_event<F>(mut self, hook: F) -> Self
    where
        F: Fn(&SupervisorEvent<'_>) + Send + Sync + 'static,
    {
        self.supervisor.config.reporter.hook = Some(Arc::new(hook));
        self
    }

    /// Serves `handle` from this supervisor; a factory passes the same handle to each instance
    pub fn handle(mut self, handle: SupervisorHandle) -> Self {
        self.supervisor.commands = handle.bind();
        self.supervisor.handle = handle;
        self
    }

    pub fn build(self) -> DynamicSupervisor {
        self.supervisor
    }
}

enum State {
    /// Placeholder while a start is in progress
    Starting,
    Running {
        token: CancellationToken,
        abort: AbortHandle,
    },
    Pending(Instant),
    Stopping {
        abort: AbortHandle,
        /// When to kill the child, and its shutdown timeout
        deadline: Option<(Instant, Duration)>,
        /// The timeout that ran out, once the child was killed
        killed: Option<Duration>,
        reply: Option<Reply<()>>,
    },
}

struct Child {
    spec: ChildSpec,
    state: State,
}

struct Runtime {
    config: Config,
    children: BTreeMap<ChildId, Child>,
    tasks: HashMap<Id, ChildId>,
    join_set: JoinSet<ProcResult>,
    window: RestartWindow,
    commands: mpsc::Receiver<Command>,
}

type Step = Option<ProcResult>;

impl Runtime {
    fn new(supervisor: DynamicSupervisor) -> Self {
        Self {
            window: RestartWindow::new(supervisor.config.intensity),
            config: supervisor.config,
            children: BTreeMap::new(),
            tasks: HashMap::new(),
            join_set: JoinSet::new(),
            commands: supervisor.commands,
        }
    }

    fn emit(&self, id: &ChildId, kind: EventKind<'_>) {
        self.config.reporter.emit(id, kind);
    }

    async fn supervise(mut self, mut shutdown: BoxFuture<'static, ()>) -> ProcResult {
        loop {
            let next_due = self.next_pending();
            let deadline = self.next_deadline();
            let step = tokio::select! {
                biased;
                _ = &mut shutdown => return self.stop_all().await,
                Some(joined) = self.join_set.join_next_with_id() => {
                    self.on_joined(joined).await.err().map(Err)
                }
                _ = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                    self.kill_overdue();
                    None
                }
                _ = sleep_until(next_due.unwrap_or_else(Instant::now)), if next_due.is_some() => {
                    self.start_due(&mut shutdown).await
                }
                Some(command) = self.commands.recv() => self.on_command(command, &mut shutdown).await,
            };
            if let Some(result) = step {
                return result;
            }
        }
    }

    async fn start(&mut self, id: &ChildId) -> ProcResult {
        let Some(child) = self.children.get_mut(id) else {
            return Ok(());
        };
        child.state = State::Starting;
        let proc = start_proc(&mut child.spec).await?;
        let token = CancellationToken::new();
        let abort = self
            .join_set
            .spawn(launch(proc, ShutdownSignal::new(token.clone())));
        self.tasks.insert(abort.id(), id.clone());
        child.state = State::Running { token, abort };
        self.emit(id, EventKind::Started);
        Ok(())
    }

    async fn start_due(&mut self, shutdown: &mut BoxFuture<'static, ()>) -> Step {
        let now = Instant::now();
        let due: Vec<ChildId> = self
            .children
            .iter()
            .filter(|(_, child)| matches!(child.state, State::Pending(at) if at <= now))
            .map(|(id, _)| id.clone())
            .collect();
        for id in due {
            tokio::select! {
                biased;
                _ = &mut *shutdown => return Some(self.stop_all().await),
                result = self.start(&id) => if let Err(error) = result {
                    self.emit(&id, EventKind::StartFailed { error: &error });
                    if let Err(escalation) = self.on_exit(id, Some(error)).await {
                        return Some(Err(escalation));
                    }
                }
            }
        }
        None
    }

    async fn on_joined(&mut self, joined: Result<(Id, ProcResult), JoinError>) -> ProcResult {
        let (task, result) = split_join(joined);
        let Some(id) = self.tasks.remove(&task) else {
            return Ok(());
        };
        match self.children.get_mut(&id).map(|child| &mut child.state) {
            Some(State::Stopping { reply, .. }) => {
                if let Some(reply) = reply.take() {
                    let _ = reply.send(Ok(()));
                }
                self.children.remove(&id);
                Ok(())
            }
            Some(State::Running { .. }) => {
                let error = result.err();
                self.emit(
                    &id,
                    EventKind::Exited {
                        error: error.as_ref(),
                    },
                );
                self.on_exit(id, error).await
            }
            _ => Ok(()),
        }
    }

    /// Applies the restart policy after child `id` exited or failed to restart
    async fn on_exit(&mut self, id: ChildId, error: Option<SupervisorError>) -> ProcResult {
        let Some(child) = self.children.get(&id) else {
            return Ok(());
        };
        if !needs_restart(child.spec.restart, error.as_ref()) {
            self.children.remove(&id);
            return Ok(());
        }
        let now = Instant::now();
        let restarts = self.window.record(now);
        if self.window.exceeded() || !child.spec.can_restart() {
            self.emit(&id, EventKind::MaxIntensity);
            let _ = self.stop_all().await;
            return Err(SupervisorError::MaxIntensity {
                id,
                source: error.map(Box::new),
            });
        }
        let delay = self.config.backoff.delay(restarts);
        if let Some(child) = self.children.get_mut(&id) {
            child.state = State::Pending(now + delay);
        }
        self.emit(&id, EventKind::Restarting { delay });
        Ok(())
    }

    async fn on_command(
        &mut self,
        command: Command,
        shutdown: &mut BoxFuture<'static, ()>,
    ) -> Step {
        match command {
            Command::StartChild(spec, reply) => {
                let id = spec.id.clone();
                if let Err(refusal) = self.admit(&spec) {
                    let _ = reply.send(Err(refusal));
                    return None;
                }
                self.children.insert(
                    id.clone(),
                    Child {
                        spec,
                        state: State::Starting,
                    },
                );
                tokio::select! {
                    biased;
                    _ = &mut *shutdown => {
                        let _ = reply.send(Err(HandleError::NotRunning));
                        return Some(self.stop_all().await);
                    }
                    result = self.start(&id) => {
                        let result = result.map_err(|error| {
                            self.children.remove(&id);
                            self.emit(&id, EventKind::StartFailed { error: &error });
                            HandleError::Start(error)
                        });
                        let _ = reply.send(result);
                    }
                }
                None
            }
            Command::TerminateChild(id, reply) => {
                if self.children.contains_key(&id) {
                    self.begin_stop(&id, Some(reply));
                } else {
                    let _ = reply.send(Err(HandleError::NotFound(id)));
                }
                None
            }
            Command::RestartChild(_, reply) | Command::DeleteChild(_, reply) => {
                let _ = reply.send(Err(HandleError::NotSupported));
                None
            }
            Command::WhichChildren(reply) => {
                let children = self
                    .children
                    .iter()
                    .map(|(id, child)| ChildInfo {
                        id: id.clone(),
                        status: match child.state {
                            State::Starting => ChildStatus::Starting,
                            State::Running { .. } => ChildStatus::Running,
                            State::Pending(_) => ChildStatus::Restarting,
                            State::Stopping { .. } => ChildStatus::Stopping,
                        },
                        restart: child.spec.restart,
                    })
                    .collect();
                let _ = reply.send(Ok(children));
                None
            }
        }
    }

    fn admit(&self, spec: &ChildSpec) -> Result<(), HandleError> {
        if self.children.contains_key(&spec.id) {
            return Err(HandleError::AlreadyPresent(spec.id.clone()));
        }
        if let Some(max) = self
            .config
            .max_children
            .filter(|max| self.children.len() >= *max)
        {
            return Err(HandleError::MaxChildren(max));
        }
        if spec.significant {
            return Err(HandleError::Start(SupervisorError::InvalidSpec {
                id: spec.id.clone(),
                reason: "a dynamic supervisor has no significant children",
            }));
        }
        Ok(())
    }

    /// Signals the child to stop and lets the main loop collect its exit
    fn begin_stop(&mut self, id: &ChildId, reply: Option<Reply<()>>) {
        let Some(child) = self.children.get_mut(id) else {
            return;
        };
        let now = Instant::now();
        match std::mem::replace(&mut child.state, State::Starting) {
            State::Running { token, abort } => {
                let deadline = match child.spec.shutdown {
                    Shutdown::Kill => {
                        abort.abort();
                        None
                    }
                    Shutdown::Infinity => {
                        token.cancel();
                        None
                    }
                    Shutdown::Timeout(limit) => {
                        token.cancel();
                        Some((now + limit, limit))
                    }
                };
                child.state = State::Stopping {
                    abort,
                    deadline,
                    killed: None,
                    reply,
                };
            }
            State::Stopping {
                abort,
                deadline,
                killed,
                reply: waiting,
            } => {
                child.state = State::Stopping {
                    abort,
                    deadline,
                    killed,
                    reply: waiting,
                };
                if let Some(reply) = reply {
                    let _ = reply.send(Ok(()));
                }
            }
            State::Pending(_) | State::Starting => {
                self.children.remove(id);
                if let Some(reply) = reply {
                    let _ = reply.send(Ok(()));
                }
            }
        }
    }

    fn kill_overdue(&mut self) {
        let now = Instant::now();
        let killed: Vec<(ChildId, Duration)> = self
            .children
            .iter_mut()
            .filter_map(|(id, child)| match &mut child.state {
                State::Stopping {
                    abort,
                    deadline: deadline @ Some(_),
                    killed,
                    ..
                } => {
                    let (at, limit) = (*deadline)?;
                    (at <= now).then(|| {
                        abort.abort();
                        *deadline = None;
                        *killed = Some(limit);
                        (id.clone(), limit)
                    })
                }
                _ => None,
            })
            .collect();
        killed
            .iter()
            .for_each(|(id, limit)| self.emit(id, EventKind::Killed { timeout: *limit }));
    }

    fn next_pending(&self) -> Option<Instant> {
        self.children
            .values()
            .filter_map(|child| match child.state {
                State::Pending(at) => Some(at),
                _ => None,
            })
            .min()
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.children
            .values()
            .filter_map(|child| match child.state {
                State::Stopping {
                    deadline: Some((at, _)),
                    ..
                } => Some(at),
                _ => None,
            })
            .min()
    }

    /// Stops all children at the same time; returns the first error
    async fn stop_all(&mut self) -> ProcResult {
        let ids: Vec<ChildId> = self.children.keys().cloned().collect();
        ids.iter().for_each(|id| self.begin_stop(id, None));
        let mut first_error = None;
        while !self.join_set.is_empty() {
            let deadline = self.next_deadline();
            tokio::select! {
                biased;
                Some(joined) = self.join_set.join_next_with_id() => {
                    let (task, result) = split_join(joined);
                    let stopped = self
                        .tasks
                        .remove(&task)
                        .and_then(|id| self.children.remove(&id).map(|child| (id, child)));
                    if let Some((id, child)) = stopped {
                        let error = match child.state {
                            State::Stopping { killed, reply, .. } => {
                                if let Some(reply) = reply {
                                    let _ = reply.send(Ok(()));
                                }
                                killed.map(SupervisorError::ShutdownTimeout).or(result.err())
                            }
                            _ => result.err(),
                        };
                        if let Some(error) = error {
                            first_error.get_or_insert(SupervisorError::Child {
                                id,
                                source: Box::new(error),
                            });
                        }
                    }
                }
                _ = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                    self.kill_overdue();
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

/// A cancelled task (killed) reports `Ok`; the caller decides from the child's state
fn split_join(joined: Result<(Id, ProcResult), JoinError>) -> (Id, ProcResult) {
    match joined {
        Ok((task, result)) => (task, result),
        Err(error) if error.is_cancelled() => (error.id(), Ok(())),
        Err(error) => {
            let task = error.id();
            let failure = match error.try_into_panic() {
                Ok(payload) => SupervisorError::from_panic(payload),
                Err(error) => SupervisorError::from_err(error),
            };
            (task, Err(failure))
        }
    }
}
