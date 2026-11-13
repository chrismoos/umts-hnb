// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{
    io::{self, Write},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tracing_appender::non_blocking::{ErrorCounter, NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::{
    EnvFilter, Registry, fmt::MakeWriter, layer::SubscriberExt, reload, util::SubscriberInitExt,
};

const QUEUED_RECORDS: usize = 64;
const MAXIMUM_RECORD_BYTES: usize = 64 * 1024;

#[derive(Clone)]
struct Writer {
    output: NonBlocking,
    oversized: Arc<AtomicU64>,
}

impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // Bound each queued write as well as queue length. Never block a radio worker on a log sink.
        if bytes.len() > MAXIMUM_RECORD_BYTES {
            self.oversized.fetch_add(1, Ordering::Relaxed);
            return Ok(bytes.len());
        }
        self.output.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Writer {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

pub struct Logging {
    filter: reload::Handle<EnvFilter, Registry>,
    dropped: ErrorCounter,
    oversized: Arc<AtomicU64>,
    _worker: WorkerGuard,
}

impl Logging {
    pub fn initialize(
        default_filter: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let filter = filter(default_filter)?;
        let (output, worker) = NonBlockingBuilder::default()
            .buffered_lines_limit(QUEUED_RECORDS)
            .lossy(true)
            .thread_name("hnb-log")
            .finish(io::stderr());
        let dropped = output.error_counter();
        let oversized = Arc::new(AtomicU64::new(0));
        let (layer, handle) = reload::Layer::new(filter);
        tracing_subscriber::registry()
            .with(layer)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_thread_names(true)
                    .with_writer(Writer {
                        output,
                        oversized: Arc::clone(&oversized),
                    }),
            )
            .try_init()?;
        Ok(Self {
            filter: handle,
            dropped,
            oversized,
            _worker: worker,
        })
    }

    pub fn set_filter(
        &self,
        configured: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.filter.reload(filter(configured)?)?;
        Ok(())
    }

    pub fn dropped_records(&self) -> u64 {
        (self.dropped.dropped_lines() as u64).saturating_add(self.oversized.load(Ordering::Relaxed))
    }
}

fn filter(configured: &str) -> Result<EnvFilter, Box<dyn std::error::Error + Send + Sync>> {
    let directive = match std::env::var("RUST_LOG") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => configured.to_owned(),
        Err(error) => return Err(error.into()),
    };
    Ok(EnvFilter::builder().with_regex(false).parse(directive)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fmt,
        sync::{atomic::AtomicUsize, mpsc},
        time::Duration,
    };

    struct Counted(Arc<AtomicUsize>);

    impl fmt::Display for Counted {
        fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.fetch_add(1, Ordering::Relaxed);
            output.write_str("counted")
        }
    }

    #[test]
    fn module_filter_skips_disabled_formatting_and_accepts_warnings() {
        let count = Arc::new(AtomicUsize::new(0));
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter("off,probe=warn")
            .with_writer(io::sink)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(target: "probe", value = %Counted(Arc::clone(&count)));
            assert_eq!(count.load(Ordering::Relaxed), 0);
            tracing::warn!(target: "probe", value = %Counted(Arc::clone(&count)));
            assert_eq!(count.load(Ordering::Relaxed), 1);
        });
    }

    #[test]
    fn oversized_writes_are_dropped_and_counted() {
        let (output, guard) = NonBlockingBuilder::default().finish(io::sink());
        let oversized = Arc::new(AtomicU64::new(0));
        let mut writer = Writer {
            output,
            oversized: Arc::clone(&oversized),
        };
        writer
            .write_all(&vec![0; MAXIMUM_RECORD_BYTES + 1])
            .unwrap();
        assert_eq!(oversized.load(Ordering::Relaxed), 1);
        writer.write_all(b"accepted\n").unwrap();
        assert_eq!(oversized.load(Ordering::Relaxed), 1);
        drop(guard);
    }

    struct BlockedSink {
        entered: Option<mpsc::Sender<()>>,
        release: mpsc::Receiver<()>,
    }

    impl Write for BlockedSink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if let Some(entered) = self.entered.take() {
                entered.send(()).unwrap();
                self.release.recv().unwrap();
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn stalled_sink_drops_excess_records_without_blocking_producer() {
        let (entered, writing) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let (mut output, guard) = NonBlockingBuilder::default()
            .buffered_lines_limit(2)
            .lossy(true)
            .finish(BlockedSink {
                entered: Some(entered),
                release: blocked,
            });
        let dropped = output.error_counter();
        output.write_all(b"first\n").unwrap();
        writing.recv_timeout(Duration::from_secs(1)).unwrap();
        let (finished, done) = mpsc::channel();
        let producer = std::thread::spawn(move || {
            for _ in 0..16 {
                output.write_all(b"next\n").unwrap();
            }
            finished.send(()).unwrap();
        });
        let completed = done.recv_timeout(Duration::from_secs(1));
        release.send(()).unwrap();
        producer.join().unwrap();
        completed.unwrap();
        assert!(dropped.dropped_lines() > 0);
        drop(guard);
    }
}
