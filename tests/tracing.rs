#![cfg(feature = "tracing")]

mod common;

use common::*;
use std::{
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};
use super_visor::{ChildSpec, Intensity, Supervisor};
use tracing::{
    field::{Field, Visit},
    span, Event, Level, Metadata, Subscriber,
};

/// (level, message, supervisor, child, error)
type Record = (Level, String, String, String, Option<String>);

/// Records each event's level, message and the supervisor fields
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<Record>>>);

#[derive(Default)]
struct Fields {
    message: String,
    supervisor: String,
    child: String,
    error: Option<String>,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "supervisor" => self.supervisor = value.to_string(),
            "child" => self.child = value.to_string(),
            _ => {}
        }
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        if field.name() == "error" {
            self.error = Some(value.to_string());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }
}

impl Subscriber for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }

    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}

    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        self.0.lock().unwrap().push((
            *event.metadata().level(),
            fields.message,
            fields.supervisor,
            fields.child,
            fields.error,
        ));
    }

    fn enter(&self, _: &span::Id) {}

    fn exit(&self, _: &span::Id) {}
}

#[tokio::test(start_paused = true)]
async fn each_decision_is_traced_with_a_fixed_message() {
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());
    let log = Log::default();

    let result = Supervisor::builder()
        .name("root")
        .intensity(Intensity::new(1, Duration::from_secs(1)))
        .child(ChildSpec::new("a", log.child("a", always(Exit::Err(10)))))
        .build()
        .run_proc_until_done()
        .await;

    assert!(result.is_err());
    let records = capture.0.lock().unwrap().clone();
    let summary: Vec<_> = records
        .iter()
        .map(|(level, message, _, _, _)| (*level, message.as_str()))
        .collect();
    assert_eq!(
        summary,
        [
            (Level::INFO, "child started"),
            (Level::WARN, "child exited"),
            (Level::INFO, "child restarting"),
            (Level::INFO, "child started"),
            (Level::WARN, "child exited"),
            (Level::ERROR, "supervisor reached max restart intensity"),
        ]
    );
    assert!(records
        .iter()
        .all(|(_, _, supervisor, child, _)| supervisor == "root" && child == "a"));
    assert_eq!(records[1].4.as_deref(), Some("task failed: error"));
}
