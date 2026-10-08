use crate::{
    event::EventHook, AutoShutdown, Backoff, ChildId, ChildSpec, EventKind, Intensity,
    ManagedFuture, ManagedProc, ProcResult, Restart, Shutdown, ShutdownSignal, Strategy,
    SupervisorError, SupervisorEvent,
};
use futures::{future::BoxFuture, FutureExt};
use std::{
    collections::{HashSet, VecDeque},
    future::poll_fn,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::Arc,
    task::Poll,
};
use tokio::time::{sleep_until, timeout, Instant};
use tokio_util::sync::CancellationToken;

/// Starts children in order, restarts them by policy, and stops them in reverse order.
/// A `Supervisor` is itself a [`ManagedProc`], so supervisors nest into a tree.
pub struct Supervisor {
    config: Config,
    children: Vec<ChildSpec>,
}

#[derive(Default)]
struct Config {
    name: Option<String>,
    strategy: Strategy,
    intensity: Intensity,
    backoff: Backoff,
    auto_shutdown: AutoShutdown,
    on_event: Option<EventHook>,
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
        Self {
            config: Config::default(),
            children: Vec::new(),
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
        self.supervisor.config.name = Some(name.into());
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
        self.supervisor.config.on_event = Some(Arc::new(hook));
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
        let invalid = |reason| SupervisorError::InvalidSpec {
            id: child.id.clone(),
            reason,
        };
        if !seen.insert(&child.id) {
            return Err(invalid("duplicate child id"));
        }
        if child.significant && child.restart == Restart::Permanent {
            return Err(invalid("a significant child must not be permanent"));
        }
        Ok(())
    })
}

fn os_shutdown_signal() -> Result<BoxFuture<'static, ()>, SupervisorError> {
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

struct Running {
    token: CancellationToken,
    future: ManagedFuture,
}

enum State {
    Idle,
    Running(Running),
    Pending(Instant),
    Stopped,
}

struct Child {
    spec: ChildSpec,
    state: State,
}

struct Runtime {
    config: Config,
    children: Vec<Child>,
    restarts: VecDeque<Instant>,
}

impl Runtime {
    fn new(supervisor: Supervisor) -> Self {
        Self {
            config: supervisor.config,
            children: supervisor
                .children
                .into_iter()
                .map(|spec| Child {
                    spec,
                    state: State::Idle,
                })
                .collect(),
            restarts: VecDeque::new(),
        }
    }

    fn id(&self, idx: usize) -> &ChildId {
        &self.children[idx].spec.id
    }

    fn emit(&self, idx: usize, kind: EventKind<'_>) {
        if let Some(hook) = &self.config.on_event {
            hook(&SupervisorEvent {
                supervisor: self.config.name.as_deref(),
                child: self.id(idx),
                kind,
            });
        }
    }

    async fn supervise(mut self, mut shutdown: BoxFuture<'static, ()>) -> ProcResult {
        loop {
            if self.auto_shutdown_due() {
                return self.stop_all().await;
            }
            let next_due = self.next_pending();
            tokio::select! {
                biased;
                _ = &mut shutdown => return self.stop_all().await,
                (idx, result) = next_exit(&mut self.children) => {
                    self.children[idx].state = State::Stopped;
                    let error = result.err();
                    self.emit(idx, EventKind::Exited { error: error.as_ref() });
                    self.on_exit(idx, error).await?;
                }
                _ = sleep_until(next_due.unwrap_or_else(Instant::now)), if next_due.is_some() => {
                    if let Some(result) = self.start_due(&mut shutdown).await {
                        return result;
                    }
                }
            }
        }
    }

    /// Starts the children whose restart is due, in start order
    async fn start_due(&mut self, shutdown: &mut BoxFuture<'static, ()>) -> Option<ProcResult> {
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
        let start = self.children[idx].spec.start_future().ok_or_else(|| {
            SupervisorError::MaxIntensity {
                id: self.id(idx).clone(),
                source: None,
            }
        })?;
        let proc = AssertUnwindSafe(start)
            .catch_unwind()
            .await
            .unwrap_or_else(|payload| Err(SupervisorError::from_panic(payload)))?;
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
        let restart = match self.children[idx].spec.restart {
            Restart::Permanent => true,
            Restart::Transient => error.is_some(),
            Restart::Temporary => false,
        };
        if !restart {
            return Ok(());
        }

        let now = Instant::now();
        self.restarts.push_back(now);
        let period = self.config.intensity.period;
        while self
            .restarts
            .front()
            .is_some_and(|at| now.duration_since(*at) > period)
        {
            self.restarts.pop_front();
        }

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
        let exceeded = self.restarts.len() > self.config.intensity.max_restarts as usize
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

        let delay = self.config.backoff.delay(self.restarts.len());
        restart_group.into_iter().for_each(|i| {
            self.children[i].state = State::Pending(now + delay);
            self.emit(i, EventKind::Restarting { delay });
        });
        Ok(())
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

/// Runs the process, treating a panic at any point as an abnormal exit
fn launch(proc: Box<dyn ManagedProc>, shutdown: ShutdownSignal) -> ManagedFuture {
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
