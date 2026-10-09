mod common;

use common::*;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use super_visor::{
    AutoShutdown, Backoff, ChildSpec, EventKind, Intensity, Restart, Shutdown, Strategy,
    Supervisor, SupervisorError,
};
use tokio::time::Instant;

#[test]
fn supervisor_is_send_and_sync() {
    assert_send_sync::<Supervisor>();
}

// --- 0.3 behavior through add_proc ---

#[tokio::test(start_paused = true)]
async fn legacy_stops_when_all_tasks_have_completed() {
    let log = Log::default();
    let result = Supervisor::builder()
        .add_proc(log.once("1", Exit::Ok(50)))
        .add_proc(log.once("2", Exit::Ok(100)))
        .build()
        .run_proc_until_done()
        .await;

    assert!(result.is_ok());
    assert_eq!(
        log.entries(),
        [
            "start:1",
            "start:2",
            "exit:1",
            "dropped:1",
            "exit:2",
            "dropped:2"
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn legacy_stops_all_in_reverse_order_after_first_error() {
    let log = Log::default();
    let result = Supervisor::builder()
        .add_proc(log.once("1", Exit::Ok(1000)))
        .add_proc(log.once("2", Exit::Err(50)))
        .add_proc(log.once("3", Exit::Err(200)))
        .build()
        .run_proc_until_done()
        .await;

    let stops: Vec<_> = log
        .entries()
        .into_iter()
        .filter(|e| !e.starts_with("dropped"))
        .collect();
    assert_eq!(
        stops,
        ["start:1", "start:2", "start:3", "exit:2", "stop:3", "stop:1"]
    );
    match result {
        Err(SupervisorError::MaxIntensity { id, source }) => {
            assert!(id.as_str().ends_with("#1"), "{id}");
            assert_eq!(source_text(&source).as_deref(), Some("task failed: error"));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn legacy_nested_supervisor_stops_its_children_then_escalates() {
    let log = Log::default();
    let result = Supervisor::builder()
        .add_proc(log.once("1", Exit::Ok(500)))
        .add_proc(
            Supervisor::builder()
                .add_proc(log.once("2-1", Exit::Ok(500)))
                .add_proc(log.once("2-2", Exit::Err(100)))
                .add_proc(log.once("2-3", Exit::Ok(500)))
                .build(),
        )
        .add_proc(
            Supervisor::builder()
                .add_proc(log.once("3-1", Exit::Ok(1000)))
                .add_proc(log.once("3-2", Exit::Ok(1000)))
                .build(),
        )
        .build()
        .run_proc_until_done()
        .await;

    let order: Vec<_> = log
        .entries()
        .into_iter()
        .filter(|e| e.starts_with("stop") || e.starts_with("exit"))
        .collect();
    assert_eq!(
        order,
        ["exit:2-2", "stop:2-3", "stop:2-1", "stop:3-2", "stop:3-1", "stop:1"]
    );
    match result {
        Err(SupervisorError::MaxIntensity {
            source: Some(inner),
            ..
        }) => {
            assert!(matches!(*inner, SupervisorError::MaxIntensity { .. }))
        }
        other => panic!("unexpected {other:?}"),
    }
}

// --- restart types ---

#[tokio::test(start_paused = true)]
async fn permanent_child_restarts_after_normal_exit() {
    let log = Log::default();
    let (token, handle) = run(Supervisor::builder()
        .child(ChildSpec::new(
            "a",
            log.child("a", first_then_never(Exit::Ok(10))),
        ))
        .build());

    tokio::time::sleep(100 * MS).await;
    assert!(stop(token, handle).await.is_ok());
    assert_eq!(log.count("start:a"), 2);
}

#[tokio::test(start_paused = true)]
async fn transient_child_restarts_only_after_abnormal_exit() {
    let log = Log::default();
    let (token, handle) = run(Supervisor::builder()
        .auto_shutdown(AutoShutdown::Never)
        .child(
            ChildSpec::new("ok", log.child("ok", always(Exit::Ok(10)))).restart(Restart::Transient),
        )
        .child(
            ChildSpec::new("err", log.child("err", first_then_never(Exit::Err(10))))
                .restart(Restart::Transient),
        )
        .build());

    tokio::time::sleep(100 * MS).await;
    assert!(stop(token, handle).await.is_ok());
    assert_eq!(log.count("start:ok"), 1);
    assert_eq!(log.count("start:err"), 2);
}

#[tokio::test(start_paused = true)]
async fn temporary_child_is_not_restarted_and_siblings_keep_running() {
    let log = Log::default();
    let (token, handle) = run(Supervisor::builder()
        .child(
            ChildSpec::new("tmp", log.child("tmp", always(Exit::Err(10))))
                .restart(Restart::Temporary),
        )
        .child(ChildSpec::new("b", log.child("b", always(Exit::Never))))
        .build());

    tokio::time::sleep(100 * MS).await;
    assert!(!handle.is_finished());
    assert!(stop(token, handle).await.is_ok());
    assert_eq!(log.count("start:tmp"), 1);
    assert_eq!(log.count("stop:b"), 1);
}

// --- intensity ---

#[tokio::test(start_paused = true)]
async fn exceeding_intensity_stops_siblings_in_reverse_and_escalates() {
    let log = Log::default();
    let (_token, handle) = run(Supervisor::builder()
        .intensity(Intensity::new(2, Duration::from_secs(1)))
        .child(ChildSpec::new("a", log.child("a", always(Exit::Never))))
        .child(ChildSpec::new("b", log.child("b", always(Exit::Err(10)))))
        .child(ChildSpec::new("c", log.child("c", always(Exit::Never))))
        .build());

    let result = handle.await.unwrap();
    assert_eq!(log.count("start:b"), 3);
    let tail: Vec<_> = log
        .entries()
        .into_iter()
        .filter(|e| e.starts_with("stop"))
        .collect();
    assert_eq!(tail, ["stop:c", "stop:a"]);
    match result {
        Err(SupervisorError::MaxIntensity { id, source }) => {
            assert_eq!(id.as_str(), "b");
            assert_eq!(source_text(&source).as_deref(), Some("task failed: error"));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn restarts_outside_the_period_do_not_accumulate() {
    let log = Log::default();
    let (token, handle) = run(Supervisor::builder()
        .intensity(Intensity::new(1, Duration::from_millis(50)))
        .child(ChildSpec::new(
            "a",
            log.child("a", |n| if n <= 4 { Exit::Err(100) } else { Exit::Never }),
        ))
        .build());

    tokio::time::sleep(1000 * MS).await;
    assert!(stop(token, handle).await.is_ok());
    assert_eq!(log.count("start:a"), 5);
}

#[tokio::test(start_paused = true)]
async fn once_only_child_in_a_restart_group_escalates() {
    let log = Log::default();
    let (_token, handle) = run(Supervisor::builder()
        .strategy(Strategy::OneForAll)
        .intensity(Intensity::new(10, Duration::from_secs(1)))
        .child(ChildSpec::once("once", log.once("once", Exit::Never)))
        .child(ChildSpec::new("b", log.child("b", always(Exit::Err(10)))))
        .build());

    let result = handle.await.unwrap();
    assert!(
        matches!(result, Err(SupervisorError::MaxIntensity { ref id, .. }) if id.as_str() == "b")
    );
    assert_eq!(log.count("start:b"), 1);
}

// --- strategies ---

#[tokio::test(start_paused = true)]
async fn one_for_all_restarts_every_child_in_order() {
    let log = Log::default();
    let (token, handle) = run(Supervisor::builder()
        .strategy(Strategy::OneForAll)
        .child(ChildSpec::new("a", log.child("a", always(Exit::Never))))
        .child(ChildSpec::new(
            "b",
            log.child("b", first_then_never(Exit::Err(10))),
        ))
        .child(ChildSpec::new("c", log.child("c", always(Exit::Never))))
        .build());

    tokio::time::sleep(100 * MS).await;
    assert!(stop(token, handle).await.is_ok());
    let events: Vec<_> = log
        .entries()
        .into_iter()
        .filter(|e| !e.starts_with("dropped"))
        .collect();
    assert_eq!(
        events,
        [
            "start:a", "start:b", "start:c", "exit:b", "stop:c", "stop:a", "start:a", "start:b",
            "start:c", "stop:c", "stop:b", "stop:a"
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn rest_for_one_restarts_only_later_children() {
    let log = Log::default();
    let (token, handle) = run(Supervisor::builder()
        .strategy(Strategy::RestForOne)
        .child(ChildSpec::new("a", log.child("a", always(Exit::Never))))
        .child(ChildSpec::new(
            "b",
            log.child("b", first_then_never(Exit::Err(10))),
        ))
        .child(ChildSpec::new("c", log.child("c", always(Exit::Never))))
        .build());

    tokio::time::sleep(100 * MS).await;
    assert!(stop(token, handle).await.is_ok());
    assert_eq!(log.count("start:a"), 1);
    assert_eq!(log.count("start:b"), 2);
    assert_eq!(log.count("start:c"), 2);
}

#[tokio::test(start_paused = true)]
async fn temporary_sibling_is_stopped_but_not_restarted_by_a_group_restart() {
    let log = Log::default();
    let (token, handle) = run(Supervisor::builder()
        .strategy(Strategy::OneForAll)
        .child(
            ChildSpec::new("tmp", log.child("tmp", always(Exit::Never)))
                .restart(Restart::Temporary),
        )
        .child(ChildSpec::new(
            "b",
            log.child("b", first_then_never(Exit::Err(10))),
        ))
        .build());

    tokio::time::sleep(100 * MS).await;
    assert!(stop(token, handle).await.is_ok());
    assert_eq!(log.count("start:tmp"), 1);
    assert_eq!(log.count("stop:tmp"), 1);
    assert_eq!(log.count("start:b"), 2);
}

// --- shutdown ---

#[tokio::test(start_paused = true)]
async fn child_that_ignores_shutdown_is_killed_after_its_timeout() {
    let log = Log::default();
    let (token, handle) = run(Supervisor::builder()
        .child(
            ChildSpec::new("hang", log.child("hang", always(Exit::Hang)))
                .shutdown(Shutdown::Timeout(Duration::from_secs(2))),
        )
        .build());

    tokio::time::sleep(10 * MS).await;
    let started = Instant::now();
    let result = stop(token, handle).await;

    assert_eq!(started.elapsed(), Duration::from_secs(2));
    tokio::task::yield_now().await;
    assert_eq!(
        log.count("dropped:hang"),
        1,
        "the spawned task must be aborted"
    );
    match result {
        Err(SupervisorError::Child { id, source }) => {
            assert_eq!(id.as_str(), "hang");
            assert!(matches!(*source, SupervisorError::ShutdownTimeout(_)));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn kill_shutdown_does_not_wait() {
    let log = Log::default();
    let (token, handle) = run(Supervisor::builder()
        .child(
            ChildSpec::new("hang", log.child("hang", always(Exit::Hang))).shutdown(Shutdown::Kill),
        )
        .build());

    tokio::time::sleep(10 * MS).await;
    let started = Instant::now();
    assert!(stop(token, handle).await.is_ok());
    assert_eq!(started.elapsed(), Duration::ZERO);
    tokio::task::yield_now().await;
    assert_eq!(log.count("dropped:hang"), 1);
}

#[tokio::test(start_paused = true)]
async fn shutdown_during_backoff_returns_cleanly() {
    let log = Log::default();
    let (token, handle) = run(Supervisor::builder()
        .backoff(Backoff::Fixed(Duration::from_secs(60)))
        .child(ChildSpec::new("a", log.child("a", always(Exit::Err(10)))))
        .build());

    tokio::time::sleep(100 * MS).await;
    assert!(stop(token, handle).await.is_ok());
    assert_eq!(log.count("start:a"), 1);
}

// --- panics ---

#[tokio::test(start_paused = true)]
async fn panics_are_abnormal_exits_in_spawned_and_unspawned_children() {
    let log = Log::default();
    let errors = Arc::new(Mutex::new(Vec::new()));
    let unspawned = {
        let make = log.child("run", first_then_never(Exit::Panic(10)));
        move || TestProc {
            spawned: false,
            ..make()
        }
    };
    let (token, handle) = run(Supervisor::builder()
        .intensity(Intensity::new(5, Duration::from_secs(1)))
        .on_event({
            let errors = errors.clone();
            move |event| {
                if let EventKind::Exited { error: Some(error) } = event.kind {
                    errors.lock().unwrap().push(error.to_string());
                }
            }
        })
        .child(ChildSpec::new(
            "spawn",
            log.child("spawn", first_then_never(Exit::Panic(10))),
        ))
        .child(ChildSpec::new("run", unspawned))
        .build());

    tokio::time::sleep(100 * MS).await;
    assert!(stop(token, handle).await.is_ok());
    assert_eq!(log.count("start:spawn"), 2);
    assert_eq!(log.count("start:run"), 2);
    assert_eq!(
        *errors.lock().unwrap(),
        ["task panicked: boom", "task panicked: boom"]
    );
}

#[tokio::test(start_paused = true)]
async fn panicking_factory_is_a_failed_start() {
    let result = Supervisor::builder()
        .child(ChildSpec::new("bad", || -> TestProc { panic!("no proc") }))
        .build()
        .run_proc_until_done()
        .await;

    match result {
        Err(SupervisorError::StartFailed { id, source }) => {
            assert_eq!(id.as_str(), "bad");
            assert!(matches!(*source, SupervisorError::Panic(ref m) if m == "no proc"));
        }
        other => panic!("unexpected {other:?}"),
    }
}

// --- start order and init ---

#[tokio::test(start_paused = true)]
async fn next_child_starts_only_after_init_completes() {
    let log = Log::default();
    let init = |name: &'static str, log: Log| {
        let make = log.child(name, always(Exit::Never));
        move || {
            let log = log.clone();
            let proc = make();
            async move {
                log.push(format!("init:{name}"));
                tokio::time::sleep(50 * MS).await;
                log.push(format!("ready:{name}"));
                Ok::<_, TestError>(proc)
            }
        }
    };
    let (token, handle) = run(Supervisor::builder()
        .child(ChildSpec::with_init("a", init("a", log.clone())))
        .child(ChildSpec::with_init("b", init("b", log.clone())))
        .build());

    tokio::time::sleep(500 * MS).await;
    assert!(stop(token, handle).await.is_ok());
    let inits: Vec<_> = log
        .entries()
        .into_iter()
        .filter(|e| e.starts_with("init") || e.starts_with("ready"))
        .collect();
    assert_eq!(inits, ["init:a", "ready:a", "init:b", "ready:b"]);
}

#[tokio::test(start_paused = true)]
async fn failed_first_start_stops_started_children_and_does_not_restart() {
    let log = Log::default();
    let attempts = Arc::new(AtomicUsize::new(0));
    let result = Supervisor::builder()
        .child(ChildSpec::new("a", log.child("a", always(Exit::Never))))
        .child(ChildSpec::with_init("b", {
            let attempts = attempts.clone();
            move || {
                attempts.fetch_add(1, Ordering::SeqCst);
                async { Err::<TestProc, _>(TestError("no db")) }
            }
        }))
        .child(ChildSpec::new("c", log.child("c", always(Exit::Never))))
        .build()
        .run_proc_until_done()
        .await;

    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(log.count("start:c"), 0);
    assert_eq!(log.count("stop:a"), 1);
    assert!(
        matches!(result, Err(SupervisorError::StartFailed { ref id, .. }) if id.as_str() == "b")
    );
}

#[tokio::test(start_paused = true)]
async fn failed_restart_counts_toward_intensity() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let log = Log::default();
    let result = Supervisor::builder()
        .intensity(Intensity::new(3, Duration::from_secs(10)))
        .child(ChildSpec::with_init("a", {
            let attempts = attempts.clone();
            let make = log.child("a", always(Exit::Err(10)));
            move || {
                let first = attempts.fetch_add(1, Ordering::SeqCst) == 0;
                let proc = make();
                async move {
                    if first {
                        Ok(proc)
                    } else {
                        Err(TestError("still down"))
                    }
                }
            }
        }))
        .build()
        .run_proc_until_done()
        .await;

    assert_eq!(attempts.load(Ordering::SeqCst), 4);
    match result {
        Err(SupervisorError::MaxIntensity { source, .. }) => {
            assert_eq!(
                source_text(&source).as_deref(),
                Some("task failed: still down")
            )
        }
        other => panic!("unexpected {other:?}"),
    }
}

// --- backoff ---

#[tokio::test(start_paused = true)]
async fn exponential_backoff_delays_each_restart() {
    let started = Arc::new(Mutex::new(Vec::new()));
    let origin = Instant::now();
    let make = Log::default().child("a", |n| if n <= 3 { Exit::Err(0) } else { Exit::Never });
    let (token, handle) = run(Supervisor::builder()
        .intensity(Intensity::new(10, Duration::from_secs(60)))
        .backoff(Backoff::Exponential {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(3),
        })
        .on_event({
            let started = started.clone();
            move |event| {
                if matches!(event.kind, EventKind::Started) {
                    started.lock().unwrap().push(origin.elapsed().as_secs());
                }
            }
        })
        .child(ChildSpec::new("a", make))
        .build());

    tokio::time::sleep(Duration::from_secs(30)).await;
    assert!(stop(token, handle).await.is_ok());
    assert_eq!(*started.lock().unwrap(), [0, 1, 3, 6]);
}

// --- auto shutdown and validation ---

#[tokio::test(start_paused = true)]
async fn any_significant_exit_stops_the_supervisor() {
    let log = Log::default();
    let result = Supervisor::builder()
        .auto_shutdown(AutoShutdown::AnySignificant)
        .child(
            ChildSpec::new("job", log.child("job", always(Exit::Ok(10))))
                .restart(Restart::Transient)
                .significant(true),
        )
        .child(ChildSpec::new(
            "helper",
            log.child("helper", always(Exit::Never)),
        ))
        .build()
        .run_proc_until_done()
        .await;

    assert!(result.is_ok());
    assert_eq!(log.count("stop:helper"), 1);
}

#[tokio::test(start_paused = true)]
async fn never_keeps_running_with_no_children_left() {
    let log = Log::default();
    let (token, handle) = run(Supervisor::builder()
        .auto_shutdown(AutoShutdown::Never)
        .child(
            ChildSpec::new("a", log.child("a", always(Exit::Ok(10))))
                .restart(Restart::Temporary)
                .significant(true),
        )
        .build());

    tokio::time::sleep(100 * MS).await;
    assert!(!handle.is_finished());
    assert!(stop(token, handle).await.is_ok());
}

#[tokio::test]
async fn invalid_specs_are_rejected_before_any_child_starts() {
    let log = Log::default();
    let duplicate = Supervisor::builder()
        .child(ChildSpec::new("a", log.child("a", always(Exit::Never))))
        .child(ChildSpec::new("a", log.child("a", always(Exit::Never))))
        .build()
        .run_proc_until_done()
        .await;
    let significant_permanent = Supervisor::builder()
        .child(ChildSpec::new("p", log.child("p", always(Exit::Never))).significant(true))
        .build()
        .run_proc_until_done()
        .await;

    assert!(matches!(
        duplicate,
        Err(SupervisorError::InvalidSpec {
            reason: "duplicate child id",
            ..
        })
    ));
    assert!(matches!(
        significant_permanent,
        Err(SupervisorError::InvalidSpec { .. })
    ));
    assert!(log.entries().is_empty());
}

// --- nesting and events ---

#[tokio::test(start_paused = true)]
async fn parent_restarts_a_nested_supervisor_that_gave_up() {
    let log = Log::default();
    let inner = {
        let make = log.child(
            "worker",
            |n| if n <= 2 { Exit::Err(10) } else { Exit::Never },
        );
        let make = Arc::new(make);
        move || {
            let make = make.clone();
            Supervisor::builder()
                .intensity(Intensity::new(0, Duration::from_secs(1)))
                .child(ChildSpec::new("worker", move || make()))
                .build()
        }
    };
    let (token, handle) = run(Supervisor::builder()
        .intensity(Intensity::new(5, Duration::from_secs(1)))
        .child(ChildSpec::supervisor("inner", inner))
        .build());

    tokio::time::sleep(100 * MS).await;
    assert!(stop(token, handle).await.is_ok());
    assert_eq!(log.count("start:worker"), 3);
    assert_eq!(log.count("stop:worker"), 1);
}

#[tokio::test(start_paused = true)]
async fn events_report_each_decision_with_supervisor_and_child() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let log = Log::default();
    let (_token, handle) = run(Supervisor::builder()
        .name("root")
        .intensity(Intensity::new(1, Duration::from_secs(1)))
        .on_event({
            let events = events.clone();
            move |event| {
                let kind = match event.kind {
                    EventKind::Started => "started",
                    EventKind::Exited { .. } => "exited",
                    EventKind::Restarting { .. } => "restarting",
                    EventKind::MaxIntensity => "max_intensity",
                    _ => "other",
                };
                events.lock().unwrap().push(format!(
                    "{}/{}:{kind}",
                    event.supervisor.unwrap(),
                    event.child
                ));
            }
        })
        .child(ChildSpec::new("a", log.child("a", always(Exit::Err(10)))))
        .build());

    assert!(handle.await.unwrap().is_err());
    assert_eq!(
        *events.lock().unwrap(),
        [
            "root/a:started",
            "root/a:exited",
            "root/a:restarting",
            "root/a:started",
            "root/a:exited",
            "root/a:max_intensity"
        ]
    );
}
