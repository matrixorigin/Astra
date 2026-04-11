//! Startup timing tracer for `--startup-trace`.

use crossterm::style::Stylize;

/// Simple tracer for measuring startup phase durations.
pub(crate) struct StartupTracer {
    enabled: bool,
    start: std::time::Instant,
    last: std::time::Instant,
    phases: Vec<(&'static str, std::time::Duration)>,
}

impl StartupTracer {
    pub(crate) fn new() -> Self {
        let enabled = std::env::var("ASTRA_STARTUP_TRACE")
            .map(|v| v == "1")
            .unwrap_or(false);
        let now = std::time::Instant::now();
        Self {
            enabled,
            start: now,
            last: now,
            phases: Vec::new(),
        }
    }

    pub(crate) fn phase(&mut self, name: &'static str) {
        if !self.enabled {
            return;
        }
        let now = std::time::Instant::now();
        let dur = now.duration_since(self.last);
        self.phases.push((name, dur));
        self.last = now;
    }

    pub(crate) fn finish(&self) {
        if !self.enabled {
            return;
        }
        let total = self.start.elapsed();
        eprintln!();
        eprintln!("  {} {}", "⏱".cyan(), "Startup Timing".bold().cyan());
        eprintln!("  {}", "─".repeat(50).dim());
        for (name, dur) in &self.phases {
            let ms = dur.as_millis();
            let bar = if ms > 100 {
                "█".repeat((ms / 20) as usize).yellow()
            } else {
                "█".repeat((ms / 20).max(1) as usize).dim()
            };
            eprintln!("  {:30} {:>6}ms {}", name, ms, bar);
        }
        eprintln!("  {}", "─".repeat(50).dim());
        let total_ms = total.as_millis();
        let status = if total_ms < 200 {
            "✓".green()
        } else if total_ms < 500 {
            "⚠".yellow()
        } else {
            "✗".red()
        };
        eprintln!("  {:30} {:>6}ms {}", "Total".bold(), total_ms, status);
        eprintln!();
    }
}
