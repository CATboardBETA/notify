//! Debouncer & access code
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use notify::{Error, ErrorKind, Event, RecommendedWatcher, Watcher};

/// The set of requirements for watcher debounce event handling functions.
///
/// # Example implementation
///
/// ```no_run
/// use notify::{Event, Result, EventHandler};
///
/// /// Prints received events
/// struct EventPrinter;
///
/// impl EventHandler for EventPrinter {
///     fn handle_event(&mut self, event: Result<Event>) {
///         if let Ok(event) = event {
///             println!("Event: {:?}", event);
///         }
///     }
/// }
/// ```
pub trait DebounceEventHandler: Send + 'static {
    /// Handles an event.
    fn handle_event(&mut self, event: DebouncedEvents);
}

impl<F> DebounceEventHandler for F
where
    F: FnMut(DebouncedEvents) + Send + 'static,
{
    fn handle_event(&mut self, event: DebouncedEvents) {
        (self)(event);
    }
}

#[cfg(feature = "crossbeam")]
impl DebounceEventHandler for crossbeam_channel::Sender<DebouncedEvents> {
    fn handle_event(&mut self, event: DebouncedEvents) {
        let _ = self.send(event);
    }
}

impl DebounceEventHandler for std::sync::mpsc::Sender<DebouncedEvents> {
    fn handle_event(&mut self, event: DebouncedEvents) {
        let _ = self.send(event);
    }
}

/// Deduplicate event data entry
struct EventData {
    /// Insertion Time
    insert: Instant,
    /// Last Update
    update: Instant,
}

impl EventData {
    fn new_any() -> Self {
        let time = Instant::now();
        Self {
            insert: time.clone(),
            update: time,
        }
    }
}

type DebouncedEvents = Result<Vec<DebouncedEvent>, Vec<Error>>;

/// A debounced event kind.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub enum DebouncedEventKind {
    /// When precise events are disabled for files
    Any,
    /// Event but debounce timed out (for example continuous writes)
    AnyContinuous,
}

/// A debounced event.
///
/// Does not emit any specific event type on purpose, only distinguishes between an any event and a continuous any event.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct DebouncedEvent {
    /// Event path
    pub path: PathBuf,
    /// Event kind
    pub kind: DebouncedEventKind,
}

impl DebouncedEvent {
    fn new(path: PathBuf, kind: DebouncedEventKind) -> Self {
        Self { path, kind }
    }
}

type DebounceData = Arc<Mutex<DebounceDataInner>>;

#[derive(Default)]
struct DebounceDataInner {
    d: HashMap<PathBuf, EventData>,
    timeout: Duration,
    e: Vec<crate::Error>,
}

impl DebounceDataInner {
    /// Retrieve a vec of debounced events, removing them if not continuous
    pub fn debounced_events(&mut self) -> Vec<DebouncedEvent> {
        let mut events_expired = Vec::with_capacity(self.d.len());
        let mut data_back = HashMap::with_capacity(self.d.len());
        // TODO: perfect fit for drain_filter https://github.com/rust-lang/rust/issues/59618
        for (k, v) in self.d.drain() {
            if v.update.elapsed() >= self.timeout {
                println!("normal timeout");
                events_expired.push(DebouncedEvent::new(k, DebouncedEventKind::Any));
            } else if v.insert.elapsed() >= self.timeout {
                println!("continuous");
                data_back.insert(k.clone(), v);
                events_expired.push(DebouncedEvent::new(k, DebouncedEventKind::AnyContinuous));
            } else {
                data_back.insert(k, v);
            }
        }
        self.d = data_back;
        events_expired
    }

    /// Returns all currently stored errors
    pub fn errors(&mut self) -> Vec<Error> {
        let mut v = Vec::new();
        std::mem::swap(&mut v, &mut self.e);
        v
    }

    /// Add an error entry to re-send later on
    pub fn add_error(&mut self, e: crate::Error) {
        self.e.push(e);
    }

    /// Add new event to debouncer cache
    pub fn add_event(&mut self, e: Event) {
        for path in e.paths.into_iter() {
            if let Some(v) = self.d.get_mut(&path) {
                v.update = Instant::now();
                println!("Exists");
            } else {
                self.d.insert(path, EventData::new_any());
            }
        }
    }
}

/// Debouncer guard, stops the debouncer on drop
pub struct Debouncer<T: Watcher> {
    stop: Arc<AtomicBool>,
    watcher: T,
    debouncer_thread: Option<std::thread::JoinHandle<()>>,
}

impl<T: Watcher> Debouncer<T> {
    /// Stop the debouncer, waits for the event thread to finish.
    /// May block for the duration of one tick_rate.
    pub fn stop(mut self) {
        self.set_stop();
        if let Some(t) = self.debouncer_thread.take() {
            let _ = t.join();
        }
    }

    /// Stop the debouncer, does not wait for the event thread to finish.
    pub fn stop_nonblocking(self) {
        self.set_stop();
    }

    fn set_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Access to the internally used notify Watcher backend
    pub fn watcher(&mut self) -> &mut dyn Watcher {
        &mut self.watcher
    }
}

impl<T: Watcher> Drop for Debouncer<T> {
    fn drop(&mut self) {
        // don't imitate c++ async futures and block on drop
        self.set_stop();
    }
}

/// Creates a new debounced watcher with custom configuration.
///
/// Timeout is the amount of time after which a debounced event is emitted or a Continuous event is send, if there still are events incoming for the specific path.
///
/// If tick_rate is None, notify will select a tick rate that is less than the provided timeout.
pub fn new_debouncer_opt<F: DebounceEventHandler, T: Watcher>(
    timeout: Duration,
    tick_rate: Option<Duration>,
    mut event_handler: F,
) -> Result<Debouncer<T>, Error> {
    let data = DebounceData::default();

    let stop = Arc::new(AtomicBool::new(false));

    let tick_div = 4;
    let tick = match tick_rate {
        Some(v) => {
            if v > timeout {
                return Err(Error::new(ErrorKind::Generic(format!(
                    "Invalid tick_rate, tick rate {:?} > {:?} timeout!",
                    v, timeout
                ))));
            }
            v
        }
        None => timeout.checked_div(tick_div).ok_or_else(|| {
            Error::new(ErrorKind::Generic(format!(
                "Failed to calculate tick as {:?}/{}!",
                timeout, tick_div
            )))
        })?,
    };

    {
        let mut data_w = data.lock().unwrap();
        data_w.timeout = timeout;
    }

    let data_c = data.clone();
    let stop_c = stop.clone();
    let thread = std::thread::Builder::new()
        .name("notify-rs debouncer loop".to_string())
        .spawn(move || loop {
            if stop_c.load(Ordering::Acquire) {
                break;
            }
            std::thread::sleep(tick);
            let send_data;
            let errors: Vec<crate::Error>;
            {
                let mut lock = data_c.lock().expect("Can't lock debouncer data!");
                send_data = lock.debounced_events();
                errors = lock.errors();
            }
            if send_data.len() > 0 {
                event_handler.handle_event(Ok(send_data));
            }
            if errors.len() > 0 {
                event_handler.handle_event(Err(errors));
            }
        })?;

    let watcher = T::new(move |e: Result<Event, Error>| {
        let mut lock = data.lock().expect("Can't lock debouncer data!");

        match e {
            Ok(e) => lock.add_event(e),
            // can't have multiple TX, so we need to pipe that through our debouncer
            Err(e) => lock.add_error(e),
        }
    })?;

    let guard = Debouncer {
        watcher,
        debouncer_thread: Some(thread),
        stop,
    };

    Ok(guard)
}

/// Short function to create a new debounced watcher with the recommended debouncer.
///
/// Timeout is the amount of time after which a debounced event is emitted or a Continuous event is send, if there still are events incoming for the specific path.
///
/// If tick_rate is None, notify will select a tick rate that is less than the provided timeout.
pub fn new_debouncer<F: DebounceEventHandler>(
    timeout: Duration,
    tick_rate: Option<Duration>,
    event_handler: F,
) -> Result<Debouncer<RecommendedWatcher>, Error> {
    new_debouncer_opt::<F, RecommendedWatcher>(timeout, tick_rate, event_handler)
}
