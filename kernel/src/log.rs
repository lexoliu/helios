use core::fmt::Write;

use nu_ansi_term::{Color, Style};
use spinning_top::Spinlock;
use tracing::Subscriber;

pub struct KernelConsoleSubscriber<Console>(Spinlock<Console>);

impl<Console: Write + Send + 'static> Subscriber for KernelConsoleSubscriber<Console> {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(0)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut console = self.0.lock();
        let metadata = event.metadata();
        let (style, level) = level_style(metadata.level());
        let _ = write!(console, "{} [{}] ", style.paint(level), metadata.target());
        let mut visitor = ConsoleVisitor(&mut *console);
        event.record(&mut visitor);
        let _ = writeln!(console);
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

struct ConsoleVisitor<'a, Console>(&'a mut Console);

impl<Console: Write> tracing::field::Visit for ConsoleVisitor<'_, Console> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn core::fmt::Debug) {
        if is_message_field(field) {
            let _ = write!(self.0, "{value:?} ");
            return;
        }
        let _ = write!(self.0, "{}={:?} ", field.name(), value);
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if is_message_field(field) {
            let _ = write!(self.0, "{value} ");
            return;
        }
        let _ = write!(self.0, "{}={} ", field.name(), value);
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        let _ = write!(self.0, "{}={} ", field.name(), value);
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        let _ = write!(self.0, "{}={} ", field.name(), value);
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        let _ = write!(self.0, "{}={} ", field.name(), value);
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        let _ = write!(self.0, "{}={} ", field.name(), value);
    }
}

pub fn init_logger(console: impl Write + Send + 'static) {
    tracing::subscriber::set_global_default(KernelConsoleSubscriber(Spinlock::new(console)))
        .expect("Failed to set global logger");
}

fn level_style(level: &tracing::Level) -> (Style, &'static str) {
    match *level {
        tracing::Level::ERROR => (Color::Red.bold(), "ERROR"),
        tracing::Level::WARN => (Color::Yellow.bold(), "WARN "),
        tracing::Level::INFO => (Color::Green.bold(), "INFO "),
        tracing::Level::DEBUG => (Color::Blue.bold(), "DEBUG"),
        tracing::Level::TRACE => (Color::LightGray.normal(), "TRACE"),
    }
}

fn is_message_field(field: &tracing::field::Field) -> bool {
    field.name().contains("message")
}
