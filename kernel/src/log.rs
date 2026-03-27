extern crate alloc;

use alloc::string::String;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use concurrent_queue::{ConcurrentQueue, PopError, PushError};
use nu_ansi_term::{Color, Style};
use objectpool::{Pool, ReusableObject};
use spinning_top::Spinlock;
use tracing::Subscriber;

pub struct KernelConsoleSubscriber<Console> {
    console: Spinlock<Console>,
    queue: ConcurrentQueue<ReusableObject<String>>,
    buffers: Pool<String>,
    flushing: AtomicBool,
}

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
        let line = format_event(event, &self.buffers);
        match self.queue.push(line) {
            Ok(()) => self.try_flush(),
            Err(PushError::Full(_)) => unreachable!("unbounded log queue reported full"),
            Err(PushError::Closed(_)) => panic!("log queue was closed unexpectedly"),
        }
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

impl<Console: Write> KernelConsoleSubscriber<Console> {
    fn try_flush(&self) {
        if self
            .flushing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        loop {
            self.flush_queue();
            self.flushing.store(false, Ordering::Release);

            if self.queue.is_empty() {
                return;
            }

            if self
                .flushing
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return;
            }
        }
    }

    fn flush_queue(&self) {
        let mut console = self.console.lock();

        loop {
            match self.queue.pop() {
                Ok(line) => {
                    let _ = console.write_str(&line);
                }
                Err(PopError::Empty | PopError::Closed) => return,
            }
        }
    }
}

struct ConsoleVisitor<'a> {
    line: &'a mut String,
}

impl tracing::field::Visit for ConsoleVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn core::fmt::Debug) {
        if is_message_field(field) {
            let _ = write!(self.line, "{value:?} ");
            return;
        }
        let _ = write!(self.line, "{}={:?} ", field.name(), value);
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if is_message_field(field) {
            let _ = write!(self.line, "{value} ");
            return;
        }
        let _ = write!(self.line, "{}={} ", field.name(), value);
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        let _ = write!(self.line, "{}={} ", field.name(), value);
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        let _ = write!(self.line, "{}={} ", field.name(), value);
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        let _ = write!(self.line, "{}={} ", field.name(), value);
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        let _ = write!(self.line, "{}={} ", field.name(), value);
    }
}

pub fn init_logger(console: impl Write + Send + 'static) {
    tracing::subscriber::set_global_default(KernelConsoleSubscriber {
        console: Spinlock::new(console),
        queue: ConcurrentQueue::unbounded(),
        buffers: Pool::unbounded(|| String::with_capacity(256), String::clear),
        flushing: AtomicBool::new(false),
    })
    .expect("Failed to set global logger");
}

fn format_event(event: &tracing::Event<'_>, buffers: &Pool<String>) -> ReusableObject<String> {
    let metadata = event.metadata();
    let (style, level) = level_style(metadata.level());
    let mut line = buffers.get_owned();
    let _ = write!(line, "{} [{}] ", style.paint(level), metadata.target());
    let mut visitor = ConsoleVisitor { line: &mut line };
    event.record(&mut visitor);
    line.push('\n');
    line
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
