extern crate alloc;

use alloc::string::String;
use core::cell::UnsafeCell;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use concurrent_queue::{ConcurrentQueue, PopError, PushError};
use nu_ansi_term::{Color, Style};
use objectpool::{Pool, ReusableObject};
use tracing::Subscriber;

const LOG_BUFFER_INITIAL_CAPACITY: usize = 256;
const LOG_BUFFER_RETAINED_CAPACITY: usize = 4096;
const LOG_BUFFER_POOL_SLOTS: usize = 64;
const LOG_QUEUE_CAPACITY: usize = LOG_BUFFER_POOL_SLOTS * 4;

pub struct KernelConsoleSubscriber<Console> {
    // The `flushing` flag serializes all writes into this single console value,
    // so we do not need an additional spinlock here.
    console: UnsafeCell<Console>,
    queue: ConcurrentQueue<ReusableObject<String>>,
    buffers: Pool<String>,
    flushing: AtomicBool,
    next_span_id: AtomicU64,
}

impl<Console: Write + Send + 'static> Subscriber for KernelConsoleSubscriber<Console> {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        matches!(
            *metadata.level(),
            tracing::Level::ERROR | tracing::Level::WARN | tracing::Level::INFO
        )
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let span_id = self.next_span_id.fetch_add(1, Ordering::Relaxed);
        assert!(span_id != 0, "kernel tracing span id counter overflowed");
        tracing::span::Id::from_u64(span_id)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let line = format_event(event, &self.buffers);
        match self.queue.push(line) {
            Ok(()) => self.try_flush(),
            Err(PushError::Full(_)) => {
                panic!("kernel log queue capacity {LOG_QUEUE_CAPACITY} exhausted")
            }
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
        // SAFETY: `try_flush` guarantees exactly one active flusher at a time.
        let console = unsafe { &mut *self.console.get() };

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
        console: UnsafeCell::new(console),
        queue: ConcurrentQueue::bounded(LOG_QUEUE_CAPACITY),
        buffers: Pool::bounded(LOG_BUFFER_POOL_SLOTS, new_log_buffer, reset_log_buffer),
        flushing: AtomicBool::new(false),
        next_span_id: AtomicU64::new(1),
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

fn new_log_buffer() -> String {
    String::with_capacity(LOG_BUFFER_INITIAL_CAPACITY)
}

fn reset_log_buffer(buffer: &mut String) {
    if buffer.capacity() > LOG_BUFFER_RETAINED_CAPACITY {
        *buffer = new_log_buffer();
    } else {
        buffer.clear();
    }
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

unsafe impl<Console: Send> Send for KernelConsoleSubscriber<Console> {}
unsafe impl<Console: Send> Sync for KernelConsoleSubscriber<Console> {}

#[cfg(test)]
mod tests {
    use alloc::string::String;
    use core::cell::UnsafeCell;
    use core::sync::atomic::{AtomicBool, AtomicU64};

    use concurrent_queue::ConcurrentQueue;
    use objectpool::Pool;

    use super::{
        KernelConsoleSubscriber, LOG_BUFFER_INITIAL_CAPACITY, LOG_BUFFER_POOL_SLOTS,
        LOG_BUFFER_RETAINED_CAPACITY, LOG_QUEUE_CAPACITY, new_log_buffer, reset_log_buffer,
    };

    #[test]
    fn log_buffer_reset_drops_oversized_capacity() {
        let mut buffer = String::with_capacity(LOG_BUFFER_RETAINED_CAPACITY + 1);
        buffer.push('x');

        reset_log_buffer(&mut buffer);

        assert!(buffer.is_empty());
        assert_eq!(buffer.capacity(), LOG_BUFFER_INITIAL_CAPACITY);
    }

    #[test]
    fn log_queue_is_bounded_to_kernel_capacity() {
        let logger = KernelConsoleSubscriber {
            console: UnsafeCell::new(String::new()),
            queue: ConcurrentQueue::bounded(LOG_QUEUE_CAPACITY),
            buffers: Pool::bounded(LOG_BUFFER_POOL_SLOTS, new_log_buffer, reset_log_buffer),
            flushing: AtomicBool::new(false),
            next_span_id: AtomicU64::new(1),
        };

        assert_eq!(logger.queue.capacity(), Some(LOG_QUEUE_CAPACITY));
    }
}
