use std::fmt::{Debug, Display, Formatter, Write};
use std::sync::Arc;

use chrono::{DateTime, Local};
use parking_lot::{RwLock, RwLockReadGuard};
use ringbuf::HeapRb;
use ringbuf::traits::consumer::Consumer;
use ringbuf::traits::{Observer, RingBuffer};
use tracing::{Event, Level, Metadata, Subscriber};
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

use crate::NonExhaustive;

/// Log record for a [Console].
///
/// Generally constructed automatically from tracing events, but can also be constructed manually
/// via [ConsoleLogRecord::new()].
///
/// [Console]: crate::console::Console
#[derive(Clone, Debug)]
pub struct ConsoleLogRecord {
    pub date_time: DateTime<Local>,
    pub level: Level,
    pub message: String,
    pub _ne: NonExhaustive,
}

impl Display for ConsoleLogRecord {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {} {}", self.date_time.format("%H:%M:%S"), self.level, self.message)
    }
}

impl ConsoleLogRecord {
    /// Constructs a new [ConsoleLogRecord].
    ///
    /// The date and time of the record are automatically recorded as the current date and time in
    /// the local timezone when the record is constructed.
    pub fn new(level: Level, message: String) -> Self {
        Self {
            date_time: Local::now(),
            level,
            message,
            _ne: NonExhaustive(()),
        }
    }

    /// Constructs a new [ConsoleLogRecord] from a tracing [Event].
    ///
    /// The log record's message is pulled from the event's "message" field if there is one, or left
    /// blank if there isn't. All additional fields are appended to the log record's message in
    /// "key=value" format.
    pub fn for_event(event: &Event) -> Self {
        let mut visitor = LogEntryVisitor::default();
        event.record(&mut visitor);
        visitor.build(event.metadata())
    }
}

struct LogEntryVisitor {
    message: Option<String>,
    data: String,
}

impl Default for LogEntryVisitor {
    fn default() -> Self {
        Self {
            message: None,
            data: "".into(),
        }
    }
}

impl Visit for LogEntryVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        match field.name() {
            "message" => {
                self.message = Some(format!("{value:?}"));
            },
            _ => {
                write!(&mut self.data, " {} = {:?}", field.name(), value).unwrap();
            }
        }
    }
}

impl LogEntryVisitor {
    fn build(self, metadata: &'static Metadata<'static>) -> ConsoleLogRecord {
        let mut message = self.message.unwrap_or("".into());
        message += &self.data;
        ConsoleLogRecord::new(metadata.level().clone(), message)
    }
}

/// Log buffer for a [Console].
///
/// Stores a maximum count of [log records][record] in a buffer, and the oldest record is erased in
/// favor of the newest when the buffer is at maximum capacity.
///
/// The default capacity is 1000 records.
///
/// [Console]: crate::console::Console
/// [record]: ConsoleLogRecord
pub struct ConsoleLog {
    buffer: RwLock<HeapRb<ConsoleLogRecord>>,
}

impl ConsoleLog {
    /// Create a new [ConsoleLog].
    ///
    /// The capacity determines how many [log records][record] can be held at once by this
    /// [ConsoleLog]. After the maximum capacity is reached, any further log records will replace
    /// the oldest record.
    ///
    /// [record]: ConsoleLogRecord
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: RwLock::new(HeapRb::new(capacity)),
        }
    }

    /// Push a new [ConsoleLogRecord].
    ///
    /// Will replace the oldest record if this [ConsoleLog] is already at maximum capacity.
    #[inline]
    pub fn push(&self, entry: ConsoleLogRecord) {
        self.buffer.write().push_overwrite(entry);
    }

    /// Iterate over all [log records][record] in this [ConsoleLog].
    ///
    /// Log records will be cloned when iterated over.
    ///
    /// NOTE: This iterator holds a read lock on the internal record collection, and will prevent
    /// write operations (i.e. new records being added) as long as the iterator is in scope, so
    /// it is highly advised to not store this iterator anywhere, and only hold it for as long as
    /// is needed to perform the iteration.
    ///
    /// [record]: ConsoleLogRecord
    pub fn iter(&self) -> ConsoleLogIter<'_> {
        let lock = self.buffer.read();
        ConsoleLogIter {
            index_front: 0,
            index_back: lock.occupied_len(),
            inner: lock,
        }
    }
}

impl Default for ConsoleLog {
    #[inline]
    fn default() -> Self {
        // Put the default capacity in a const somewhere
        Self::new(1000)
    }
}

pub struct ConsoleLogIter<'a> {
    index_front: usize,
    index_back: usize,
    inner: RwLockReadGuard<'a, HeapRb<ConsoleLogRecord>>,
}

impl ConsoleLogIter<'_> {
    fn get(&self, index: usize) -> Option<ConsoleLogRecord> {
        let (s_first, s_second) = self.inner.as_slices();
        let s_first_len = s_first.len();
        if index < s_first_len {
            Some(s_first[index].clone())
        } else if (index - s_first_len) < s_second.len() {
            Some(s_second[index - s_first_len].clone())
        } else {
            None
        }
    }
}

impl Iterator for ConsoleLogIter<'_> {
    type Item = ConsoleLogRecord;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index_front >= self.index_back {
            return None
        }
        let val = self.get(self.index_front);
        self.index_front += 1;
        val
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let size = self.index_back - self.index_front;
        (size, Some(size))
    }
}

impl DoubleEndedIterator for ConsoleLogIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.index_back <= self.index_front {
            return None
        }
        self.index_back -= 1;
        self.get(self.index_back)
    }
}

impl ExactSizeIterator for ConsoleLogIter<'_> {}

/// Tracing subscriber layer for a [ConsoleLog].
///
/// Used to consume tracing events and convert them into [log records][record].
///
/// # Examples
///
/// ```
/// use tracing_subscriber::prelude::*;
///
/// use gtether::console::Console;
/// use gtether::console::log::ConsoleLogLayer;
///
/// let console = Console::builder().build();
///
/// tracing_subscriber::fmt()
///     .finish()
///     .with(ConsoleLogLayer::new(console.log()))
///     .init();
/// ```
///
/// [record]: ConsoleLogRecord
pub struct ConsoleLogLayer {
    log: Arc<ConsoleLog>,
}

impl ConsoleLogLayer {
    /// Create a [ConsoleLogLayer] from a [ConsoleLog].
    pub fn new(log: &Arc<ConsoleLog>) -> Self {
        Self {
            log: log.clone(),
        }
    }
}

impl<S: Subscriber> Layer<S> for ConsoleLogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        self.log.push(ConsoleLogRecord::for_event(event));
    }
}