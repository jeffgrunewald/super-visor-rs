use crate::{
    handle::{ChildInfo, ChildStatus, Command, HandleError, SupervisorHandle},
    report::Reporter,
    runtime::{launch, needs_restart, os_shutdown_signal, start_proc, RestartWindow},
    AutoShutdown, Backoff, ChildId, ChildSpec, EventKind, Intensity, ManagedFuture, ManagedProc,
    ProcResult, Restart, Shutdown, ShutdownSignal, Strategy, SupervisorError, SupervisorEvent,
};
use futures::{future::BoxFuture, FutureExt};
use std::{collections::HashSet, future::poll_fn, sync::Arc, task::Poll};
use tokio::{
    sync::mpsc,
    time::{sleep_until, timeout, Instant},
};
use tokio_util::sync::CancellationToken;

/// Starts children in order, restarts them by policy, and stops them in reverse order.
/// A `Supervisor` is itself a [`ManagedProc`], so supervisors nest into a tree.
pub struct Supervisor {
    config: Config,
    children: Vec<ChildSpec>,
    handle: SupervisorHandle,
    commands: mpsc::Receiver<Command>,
}

#[derive(Default)]
struct Config {
    reporter: Reporter,
    strategy: Strategy,
    intensity: Intensity,
    backoff: Backoff,
    auto_shutdown: AutoShutdown,
}

/// Builder for a [`Supervisor`]; see the crate docs for an example
pub struct SupervisorBuilder {
    supervisor: Supervisor,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor {
    pub fn new() -> Self {
        let handle = SupervisorHandle::new();
        Self {
            config: Config::default(),
            children: Vec::new(),
            commands: handle.bind(),
            handle,
        }
    }

    pub fn builder() -> SupervisorBuilder {
        SupervisorBuilder {
            supervisor: Self::new(),
        }
    }

    /// Adds a once-only child with 0.3 behavior: see [`SupervisorBuilder::add_proc`]
    pub fn add(&mut self, proc: impl ManagedProc + 'static) {
        let spec = legacy_spec(self.children.len(), proc);
        self.children.push(spec);
    }

    pub fn add_child(&mut self, spec: ChildSpec) {
        self.children.push(spec);
    }

    /// A handle to this supervisor; calls made before it starts wait until it is running
    pub fn handle(&self) -> SupervisorHandle {
        self.handle.clone()
    }

    /// Runs until SIGTERM or Ctrl+C, a significant-child auto shutdown, or escalation
    pub async fn start(self) -> ProcResult {
        let shutdown = os_shutdown_signal()?;
        self.run_until(shutdown).await
    }

    async fn run_until(self, mut shutdown: BoxFuture<'static, ()>) -> ProcResult {
        validate(&self.children)?;
        let mut runtime = Runtime::new(self);
        for idx in 0..runtime.children.len() {
            tokio::select! {
                biased;
                _ = &mut shutdown => return runtime.stop_all().await,
                result = runtime.start(idx) => if let Err(error) = result {
                    runtime.emit(idx, EventKind::StartFailed { error: &error });
                    let _ = runtime.stop_all().await;
                    return Err(SupervisorError::StartFailed {
                        id: runtime.id(idx).clone(),
                        source: Box::new(error),
                    });
                }
            }
        }
        runtime.supervise(shutdown).await
    }
}

impl ManagedProc for Supervisor {
    fn run_proc(self: Box<Self>, shutdown: ShutdownSignal) -> ManagedFuture {
        Box::pin(self.run_until(Box::pin(shutdown)))
    }
}

impl SupervisorBuilder {
    /// Names the supervisor in its events
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.supervisor.config.reporter.name = Some(name.into());
        self
    }

    pub fn strategy(mut self, strategy: Strategy) -> Self {
        self.supervisor.config.strategy = strategy;
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

    pub fn auto_shutdown(mut self, auto_shutdown: AutoShutdown) -> Self {
        self.supervisor.config.auto_shutdown = auto_shutdown;
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

    pub fn child(mut self, spec: ChildSpec) -> Self {
        self.supervisor.add_child(spec);
        self
    }

    /// Adds a once-only, significant, transient child that stops with [`Shutdown::Infinity`].
    /// Any restart that needs it escalates, so the first `Err` stops all children (0.3 behavior).
    pub fn add_proc(mut self, proc: impl ManagedProc + 'static) -> Self {
        self.supervisor.add(proc);
        self
    }

    pub fn build(self) -> Supervisor {
        self.supervisor
    }
}

fn legacy_spec<P: ManagedProc + 'static>(position: usize, proc: P) -> ChildSpec {
    let id = format!("{}#{position}", std::any::type_name::<P>());
    ChildSpec::once(id, proc)
        .shutdown(Shutdown::Infinity)
        .significant(true)
}

fn validate(children: &[ChildSpec]) -> ProcResult {
    let mut seen = HashSet::new();
    children.iter().try_for_each(|child| {
        if !seen.insert(&child.id) {
            return Err(SupervisorError::InvalidSpec {
                id: child.id.clone(),
                reason: "duplicate child id",
            });
        }
        validate_spec(child)
    })
}

fn validate_spec(child: &ChildSpec) -> ProcResult {
    if child.significant && child.restart == Restart::Permanent {
        return Err(SupervisorError::InvalidSpec {
            id: child.id.clone(),
            reason: "a significant child must not be permanent",
        });
    }
    Ok(())
}

struct Running {
    token: CancellationToken,
    future: ManagedFuture,
}

enum State {
    Idle,
    Running(Running),
    Pending(Instant),
    /// Exited and not restarted; counts for auto shutdown
    Stopped,
    /// Stopped by `terminate_child`; does not count for auto shutdown
    Terminated,
}

struct Child {
    spec: ChildSpec,
    state: State,
}

impl Child {
    fn info(&self) -> ChildInfo {
        ChildInfo {
            id: self.spec.id.clone(),
            status: match self.state {
                State::Idle => ChildStatus::Starting,
                State::Running(_) => ChildStatus::Running,
                State::Pending(_) => ChildStatus::Restarting,
                State::Stopped | State::Terminated => ChildStatus::Stopped,
            },
            restart: self.spec.restart,
        }
    }
}

struct Runtime {
    config: Config,
    children: Vec<Child>,
    window: RestartWindow,
    commands: mpsc::Receiver<Command>,
}

/// What the main loop does after a step that can end the supervisor
type Step = Option<ProcResult>;

impl Runtime {
    fn new(supervisor: Supervisor) -> Self {
        Self {
            window: RestartWindow::new(supervisor.config.intensity),
            config: supervisor.config,
            children: supervisor
                .children
                .into_iter()
                .map(|spec| Child {
                    spec,
                    state: State::Idle,
                })
                .collect(),
            commands: supervisor.commands,
        }
    }

    fn id(&self, idx: usize) -> &ChildId {
        &self.children[idx].spec.id
    }

    fn position(&self, id: &ChildId) -> Result<usize, HandleError> {
        self.children
            .iter()
            .position(|child| &child.spec.id == id)
            .ok_or_else(|| HandleError::NotFound(id.clone()))
    }

    fn emit(&self, idx: usize, kind: EventKind<'_>) {
        self.config.reporter.emit(self.id(idx), kind);
    }

    async fn supervise(mut self, mut shutdown: BoxFuture<'static, ()>) -> ProcResult {
        loop {
            if self.auto_shutdown_due() {
                return self.stop_all().await;
            }
            let next_due = self.next_pending();
            let step = tokio::select! {
                biased;
                _ = &mut shutdown => return self.stop_all().await,
                (idx, result) = next_exit(&mut self.children) => {
                    self.children[idx].state = State::Stopped;
                    let error = result.err();
                    self.emit(idx, EventKind::Exited { error: error.as_ref() });
                    self.on_exit(idx, error).await.err().map(Err)
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

    /// Starts the children whose restart is due, in start order
    async fn start_due(&mut self, shutdown: &mut BoxFuture<'static, ()>) -> Step {
        let now = Instant::now();
        for idx in 0..self.children.len() {
            if !matches!(self.children[idx].state, State::Pending(at) if at <= now) {
                continue;
            }
            tokio::select! {
                biased;
                _ = &mut *shutdown => return Some(self.stop_all().await),
                result = self.start(idx) => if let Err(error) = result {
                    self.children[idx].state = State::Stopped;
                    self.emit(idx, EventKind::StartFailed { error: &error });
                    if let Err(escalation) = self.on_exit(idx, Some(error)).await {
                        return Some(Err(escalation));
                    }
                }
            }
        }
        None
    }

    async fn start(&mut self, idx: usize) -> ProcResult {
        let proc = start_proc(&mut self.children[idx].spec).await?;
        let token = CancellationToken::new();
        self.children[idx].state = State::Running(Running {
            future: launch(proc, ShutdownSignal::new(token.clone())),
            token,
        });
        self.emit(idx, EventKind::Started);
        Ok(())
    }

    /// Applies the restart policy after child `idx` exited without a shutdown signal
    async fn on_exit(&mut self, idx: usize, error: Option<SupervisorError>) -> ProcResult {
        if !needs_restart(self.children[idx].spec.restart, error.as_ref()) {
            return Ok(());
        }

        let now = Instant::now();
        let restarts = self.window.record(now);
        let (restart_group, removed): (Vec<usize>, Vec<usize>) = self
            .restart_scope(idx)
            .filter(|&i| {
                i == idx
                    || matches!(
                        self.children[i].state,
                        State::Running(_) | State::Pending(_)
                    )
            })
            .partition(|&i| i == idx || self.children[i].spec.restart != Restart::Temporary);
        let exceeded = self.window.exceeded()
            || restart_group
                .iter()
                .any(|&i| !self.children[i].spec.can_restart());
        if exceeded {
            self.emit(idx, EventKind::MaxIntensity);
            let _ = self.stop_all().await;
            return Err(SupervisorError::MaxIntensity {
                id: self.id(idx).clone(),
                source: error.map(Box::new),
            });
        }

        let mut to_stop: Vec<usize> = restart_group.iter().chain(&removed).copied().collect();
        to_stop.sort_unstable_by(|a, b| b.cmp(a));
        for i in to_stop.into_iter().filter(|&i| i != idx) {
            let _ = self.stop_child(i).await;
        }

        let delay = self.config.backoff.delay(restarts);
        restart_group.into_iter().for_each(|i| {
            self.children[i].state = State::Pending(now + delay);
            self.emit(i, EventKind::Restarting { delay });
        });
        Ok(())
    }

    async fn on_command(
        &mut self,
        command: Command,
        shutdown: &mut BoxFuture<'static, ()>,
    ) -> Step {
        match command {
            Command::StartChild(spec, reply) => {
                if self.position(&spec.id).is_ok() {
                    let _ = reply.send(Err(HandleError::AlreadyPresent(spec.id.clone())));
                    return None;
                }
                if let Err(error) = validate_spec(&spec) {
                    let _ = reply.send(Err(HandleError::Start(error)));
                    return None;
                }
                self.children.push(Child {
                    spec,
                    state: State::Idle,
                });
                let idx = self.children.len() - 1;
                let (step, result) = self.start_by_command(idx, shutdown).await;
                if result.is_err() {
                    self.children.remove(idx);
                }
                let _ = reply.send(result);
                step
            }
            Command::RestartChild(id, reply) => {
                let idx = match self.position(&id) {
                    Ok(idx) => idx,
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        return None;
                    }
                };
                let refusal = match self.children[idx].state {
                    State::Running(_) | State::Idle => Some(HandleError::Running(id)),
                    State::Pending(_) => Some(HandleError::Restarting(id)),
                    _ if !self.children[idx].spec.can_restart() => {
                        Some(HandleError::NotRestartable(id))
                    }
                    _ => None,
                };
                if let Some(refusal) = refusal {
                    let _ = reply.send(Err(refusal));
                    return None;
                }
                let (step, result) = self.start_by_command(idx, shutdown).await;
                let _ = reply.send(result);
                step
            }
            Command::TerminateChild(id, reply) => {
                let result = match self.position(&id) {
                    Ok(idx) => {
                        let _ = self.stop_child(idx).await;
                        self.children[idx].state = State::Terminated;
                        if self.children[idx].spec.restart == Restart::Temporary {
                            self.children.remove(idx);
                        }
                        Ok(())
                    }
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
                None
            }
            Command::DeleteChild(id, reply) => {
                let result = self
                    .position(&id)
                    .and_then(|idx| match self.children[idx].state {
                        State::Running(_) | State::Idle => Err(HandleError::Running(id)),
                        State::Pending(_) => Err(HandleError::Restarting(id)),
                        State::Stopped | State::Terminated => {
                            self.children.remove(idx);
                            Ok(())
                        }
                    });
                let _ = reply.send(result);
                None
            }
            Command::WhichChildren(reply) => {
                let _ = reply.send(Ok(self.children.iter().map(Child::info).collect()));
                None
            }
        }
    }

    /// Starts a child for a handle call; a failed start leaves it terminated
    async fn start_by_command(
        &mut self,
        idx: usize,
        shutdown: &mut BoxFuture<'static, ()>,
    ) -> (Step, Result<(), HandleError>) {
        tokio::select! {
            biased;
            _ = &mut *shutdown => (Some(self.stop_all().await), Err(HandleError::NotRunning)),
            result = self.start(idx) => match result {
                Ok(()) => (None, Ok(())),
                Err(error) => {
                    self.children[idx].state = State::Terminated;
                    self.emit(idx, EventKind::StartFailed { error: &error });
                    (None, Err(HandleError::Start(error)))
                }
            }
        }
    }

    fn restart_scope(&self, idx: usize) -> std::ops::Range<usize> {
        match self.config.strategy {
            Strategy::OneForOne => idx..idx + 1,
            Strategy::OneForAll => 0..self.children.len(),
            Strategy::RestForOne => idx..self.children.len(),
        }
    }

    fn next_pending(&self) -> Option<Instant> {
        self.children
            .iter()
            .filter_map(|child| match child.state {
                State::Pending(at) => Some(at),
                _ => None,
            })
            .min()
    }

    fn auto_shutdown_due(&self) -> bool {
        let mut significant = self
            .children
            .iter()
            .filter(|child| child.spec.significant)
            .map(|child| matches!(child.state, State::Stopped))
            .peekable();
        match self.config.auto_shutdown {
            AutoShutdown::Never => false,
            AutoShutdown::AnySignificant => significant.any(|stopped| stopped),
            AutoShutdown::AllSignificant => {
                significant.peek().is_some() && significant.all(|stopped| stopped)
            }
        }
    }

    /// Stops all children in reverse start order; returns the first error
    async fn stop_all(&mut self) -> ProcResult {
        let mut first_error = None;
        for idx in (0..self.children.len()).rev() {
            if let Err(error) = self.stop_child(idx).await {
                first_error.get_or_insert(SupervisorError::Child {
                    id: self.id(idx).clone(),
                    source: Box::new(error),
                });
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    async fn stop_child(&mut self, idx: usize) -> ProcResult {
        let State::Running(running) =
            std::mem::replace(&mut self.children[idx].state, State::Stopped)
        else {
            return Ok(());
        };
        match self.children[idx].spec.shutdown {
            Shutdown::Kill => Ok(()),
            Shutdown::Infinity => {
                running.token.cancel();
                running.future.await
            }
            Shutdown::Timeout(limit) => {
                running.token.cancel();
                match timeout(limit, running.future).await {
                    Ok(result) => result,
                    Err(_) => {
                        self.emit(idx, EventKind::Killed { timeout: limit });
                        Err(SupervisorError::ShutdownTimeout(limit))
                    }
                }
            }
        }
    }
}

/// Resolves with the first running child that exits; children keep their positions
fn next_exit(
    children: &mut [Child],
) -> impl std::future::Future<Output = (usize, ProcResult)> + '_ {
    poll_fn(move |cx| {
        children
            .iter_mut()
            .enumerate()
            .find_map(|(idx, child)| match &mut child.state {
                State::Running(running) => match running.future.poll_unpin(cx) {
                    Poll::Ready(result) => Some((idx, result)),
                    Poll::Pending => None,
                },
                _ => None,
            })
            .map_or(Poll::Pending, Poll::Ready)
    })
}
