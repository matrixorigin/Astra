use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Statement(String, f64);

impl tracing::field::Visit for Statement {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "db.statement" && !value.trim().is_empty() {
            self.0 = value.split_whitespace().collect::<Vec<_>>().join(" ");
        } else if field.name() == "summary" && self.0.is_empty() {
            self.0 = value.to_owned();
        }
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        if field.name() == "elapsed_secs" {
            self.1 = value;
        }
    }

    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}

pub struct ObservedSql {
    pub sql: String,
    pub elapsed: std::time::Duration,
    pub finished_at: std::time::Instant,
}

#[derive(Clone, Default)]
pub struct SqlCapture(pub Arc<Mutex<Option<Vec<ObservedSql>>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for SqlCapture {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() != "sqlx::query" {
            return;
        }
        let mut guard = self.0.lock().unwrap();
        if let Some(statements) = guard.as_mut() {
            let mut statement = Statement::default();
            event.record(&mut statement);
            statements.push(ObservedSql {
                sql: statement.0,
                elapsed: std::time::Duration::from_secs_f64(statement.1),
                finished_at: std::time::Instant::now(),
            });
        }
    }
}
