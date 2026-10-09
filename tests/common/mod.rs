#![allow(dead_code)]
//! Test children, logs and runners shared by the integration tests

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use super_visor::{
    CancellationToken, ManagedFuture, ManagedProc, ProcResult, ShutdownSignal, SupervisorError,
};
use tokio::task::JoinHandle;

pub const MS: Duration = Duration::from_millis(1);

#[derive(Debug)]
pub struct TestError(pub &'static str);

impl std::fmt::Display for TestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for TestError {}

/// What a test child does after it starts
#[derive(Debug, Clone, Copy)]
pub enum Exit {
    /// Run until shutdown
    Never,
    Ok(u64),
    Err(u64),
    Panic(u64),
    /// Ignore the shutdown signal
    Hang,
}

/// Records child activity as "start:a", "exit:a" (own exit), "stop:a" (shutdown), "dropped:a"
#[derive(Clone, Default)]
pub struct Log(pub Arc<Mutex<Vec<String>>>);

impl Log {
    pub fn push(&self, entry: String) {
        self.0.lock().unwrap().push(entry);
    }

    pub fn entries(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }

    pub fn count(&self, entry: &str) -> usize {
        self.entries().iter().filter(|e| *e == entry).count()
    }

    /// A child that follows `plan(n)` on its n-th start
    pub fn child(
        &self,
        name: &'static str,
        plan: impl Fn(usize) -> Exit + Send + Sync + 'static,
    ) -> impl Fn() -> TestProc + Send + Sync + 'static {
        let log = self.clone();
        let starts = AtomicUsize::new(0);
        move || TestProc {
            name,
            log: log.clone(),
            exit: plan(starts.fetch_add(1, Ordering::SeqCst) + 1),
            spawned: true,
        }
    }

    pub fn once(&self, name: &'static str, exit: Exit) -> TestProc {
        TestProc {
            name,
            log: self.clone(),
            exit,
            spawned: true,
        }
    }
}

pub struct DropGuard(&'static str, Log);

impl Drop for DropGuard {
    fn drop(&mut self) {
        self.1.push(format!("dropped:{}", self.0));
    }
}

pub struct TestProc {
    pub name: &'static str,
    pub log: Log,
    pub exit: Exit,
    pub spawned: bool,
}

impl TestProc {
    async fn run(self, mut shutdown: ShutdownSignal) -> ProcResult {
        let name = self.name;
        let log = self.log;
        log.push(format!("start:{name}"));
        let _guard = DropGuard(name, log.clone());
        let after = |ms: u64| tokio::time::sleep(Duration::from_millis(ms));
        let outcome = match self.exit {
            Exit::Never => {
                (&mut shutdown).await;
                None
            }
            Exit::Hang => std::future::pending().await,
            Exit::Ok(ms) | Exit::Err(ms) | Exit::Panic(ms) => tokio::select! {
                _ = &mut shutdown => None,
                _ = after(ms) => Some(self.exit),
            },
        };
        match outcome {
            None => {
                log.push(format!("stop:{name}"));
                Ok(())
            }
            Some(exit) => {
                log.push(format!("exit:{name}"));
                match exit {
                    Exit::Err(_) => Err(SupervisorError::from_err(TestError("error"))),
                    Exit::Panic(_) => panic!("boom"),
                    _ => Ok(()),
                }
            }
        }
    }
}

impl ManagedProc for TestProc {
    fn run_proc(self: Box<Self>, shutdown: ShutdownSignal) -> ManagedFuture {
        if self.spawned {
            super_visor::spawn(self.run(shutdown))
        } else {
            super_visor::run(self.run(shutdown))
        }
    }
}

pub fn always(exit: Exit) -> impl Fn(usize) -> Exit + Send + Sync + 'static {
    move |_| exit
}

pub fn first_then_never(exit: Exit) -> impl Fn(usize) -> Exit + Send + Sync + 'static {
    move |n| if n == 1 { exit } else { Exit::Never }
}

/// Runs a supervisor as a child of a test-owned shutdown token
pub fn run(supervisor: impl ManagedProc + 'static) -> (CancellationToken, JoinHandle<ProcResult>) {
    let token = CancellationToken::new();
    let future = Box::new(supervisor).run_proc(ShutdownSignal::new(token.clone()));
    (token, tokio::spawn(future))
}

pub async fn stop(token: CancellationToken, handle: JoinHandle<ProcResult>) -> ProcResult {
    token.cancel();
    handle.await.expect("supervisor task panicked")
}

pub fn source_text(error: &Option<Box<SupervisorError>>) -> Option<String> {
    error.as_ref().map(|e| e.to_string())
}

pub fn assert_send_sync<T: Send + Sync>() {}

/// Runs a supervisor that is expected to end on its own
#[allow(async_fn_in_trait)]
pub trait RunUntilDone {
    async fn run_proc_until_done(self) -> ProcResult;
}

impl<T: ManagedProc + 'static> RunUntilDone for T {
    async fn run_proc_until_done(self) -> ProcResult {
        let (_token, handle) = run(self);
        handle.await.expect("supervisor task panicked")
    }
}
