// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::logs::repository::LogsRepository;
use diagnostics_data::{Data, Logs};
use fidl_fuchsia_diagnostics::{Selector, StreamMode};
use fuchsia_async::OnSignals;
use fuchsia_trace as ftrace;
use futures::channel::{mpsc, oneshot};
use futures::executor::block_on;
use futures::{select, FutureExt, StreamExt};
use log::warn;
use selectors::FastError;
use std::collections::HashSet;
use std::fmt::Display;
use std::io::{self, Write};
use std::pin::pin;
use std::sync::Arc;
use std::{mem, thread};
use zx::Signals;

const MAX_SERIAL_WRITE_SIZE: usize = 256;

/// Function that forwards logs from Archivist to the serial port. Logs will be filtered by
/// `allow_serial_log_tags` to include logs in the serial output, and `deny_serial_log_tags` to
/// exclude specific tags.
pub async fn launch_serial(
    allow_serial_log_tags: Vec<String>,
    deny_serial_log_tags: Vec<String>,
    logs_repo: Arc<LogsRepository>,
    sink: impl Write + Send + 'static,
    mut freeze_receiver: mpsc::UnboundedReceiver<oneshot::Sender<zx::EventPair>>,
) {
    let writer = SerialWriter::new(sink, deny_serial_log_tags.into_iter().collect());
    let mut barrier = writer.get_barrier();
    let mut write_logs_to_serial =
        pin!(SerialConfig::new(allow_serial_log_tags).write_logs(logs_repo, writer).fuse());
    loop {
        select! {
            _ = write_logs_to_serial => break,
            freeze_request = freeze_receiver.next() => {
                if let Some(request) = freeze_request {
                    // We must use the barrier before we send back the event.
                    barrier.wait().await;
                    let (client, server) = zx::EventPair::create();
                    let _ = request.send(client);
                    let _ = OnSignals::new(&server, Signals::EVENTPAIR_PEER_CLOSED).await;
                }
            }
        }
    }
}

#[derive(Default)]
pub struct SerialConfig {
    selectors: Vec<Selector>,
}

impl SerialConfig {
    /// Creates a new serial configuration from the given structured config values.
    pub fn new<C>(allowed_components: Vec<C>) -> Self
    where
        C: AsRef<str> + Display,
    {
        let selectors = allowed_components
            .into_iter()
            .filter_map(|selector| {
                match selectors::parse_component_selector::<FastError>(selector.as_ref()) {
                    Ok(s) => Some(Selector {
                        component_selector: Some(s),
                        tree_selector: None,
                        ..Selector::default()
                    }),
                    Err(err) => {
                        warn!(selector:%, err:?; "Failed to parse component selector");
                        None
                    }
                }
            })
            .collect();
        Self { selectors }
    }

    /// Returns a future that resolves when there's no more logs to write to serial. This can only
    /// happen when all log sink connections have been closed for the components that were
    /// configured to emit logs.
    async fn write_logs(self, repo: Arc<LogsRepository>, mut writer: SerialWriter) {
        let mut log_stream = repo.logs_cursor(
            StreamMode::SnapshotThenSubscribe,
            Some(self.selectors),
            ftrace::Id::random(),
        );
        while let Some(log) = log_stream.next().await {
            writer.log(&log).await;
        }
        // Ensure logs are flushed before we finish.
        writer.get_barrier().wait().await;
    }
}

/// A sink to write to serial. This Write implementation must be used together with SerialWriter.
#[derive(Default)]
pub struct SerialSink;

impl Write for SerialSink {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if cfg!(debug_assertions) {
            debug_assert!(buffer.len() <= MAX_SERIAL_WRITE_SIZE);
        } else {
            use std::sync::atomic::{AtomicBool, Ordering};
            static ALREADY_LOGGED: AtomicBool = AtomicBool::new(false);
            if buffer.len() > MAX_SERIAL_WRITE_SIZE && !ALREADY_LOGGED.swap(true, Ordering::Relaxed)
            {
                let size = buffer.len();
                log::error!(
                    size;
                    "Skipping write to serial due to internal error. Exceeded max buffer size."
                );
                return Ok(buffer.len());
            }
        }
        // SAFETY: calling a syscall. We pass a pointer to the buffer and its exact size.
        unsafe {
            zx::sys::zx_debug_write(buffer.as_ptr(), buffer.len());
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// An enum to represent commands sent to the worker thread.
enum WorkerCommand {
    /// Write the given bytes.
    Write(Vec<u8>),
    /// Flush all pending writes and notify when done.
    Flush(oneshot::Sender<()>),
}

/// A writer that provides an async interface to a synchronous, blocking write operation.
///
/// It spawns a dedicated thread to handle the synchronous writes, allowing the async
/// context to remain non-blocking.
struct SerialWriter {
    denied_tags: HashSet<String>,
    sender: mpsc::UnboundedSender<WorkerCommand>,
    buffers: mpsc::UnboundedReceiver<Vec<u8>>,
    current_buffer: Vec<u8>,
}

impl SerialWriter {
    /// Creates a new Writer and spawns a worker thread.
    fn new<S: Write + Send + 'static>(mut sink: S, denied_tags: HashSet<String>) -> Self {
        let (sender, mut receiver) = mpsc::unbounded();
        let (buffer_sender, buffers) = mpsc::unbounded();

        // Limit to 32 buffers (one comes from `current_buffer`).
        for _ in 0..31 {
            buffer_sender.unbounded_send(Vec::with_capacity(MAX_SERIAL_WRITE_SIZE)).unwrap();
        }

        thread::spawn(move || {
            block_on(async {
                while let Some(command) = receiver.next().await {
                    match command {
                        WorkerCommand::Write(bytes) => {
                            // Ignore errors.
                            let _ = sink.write(&bytes);
                            // Send the buffer back.
                            let _ = buffer_sender.unbounded_send(bytes);
                        }
                        WorkerCommand::Flush(notifier) => {
                            let _ = notifier.send(());
                        }
                    }
                }
            });
        });

        Self {
            denied_tags,
            sender,
            buffers,
            current_buffer: Vec::with_capacity(MAX_SERIAL_WRITE_SIZE),
        }
    }

    /// Asynchronously writes bytes.
    async fn write(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            if self.current_buffer.capacity() == 0 {
                self.current_buffer = self.buffers.next().await.unwrap();
                self.current_buffer.clear();
            }
            let (part, rem) = bytes.split_at(std::cmp::min(self.space(), bytes.len()));
            self.current_buffer.extend(part);
            if !rem.is_empty() {
                self.flush();
            }
            bytes = rem;
        }
    }

    /// Return a synchronous writer with the required capacity.
    ///
    /// NOTE: Using `io_writer` will mean that line breaks don't occur in the middle: they will
    /// always be at the beginning of whatever is being output, which is different from what happens
    /// with `write`.
    async fn io_writer(&mut self, required: usize) -> IoWriter<'_> {
        assert!(required < MAX_SERIAL_WRITE_SIZE);
        if self.current_buffer.capacity() == 0 || self.space() < required {
            self.flush();
            self.current_buffer = self.buffers.next().await.unwrap();
            self.current_buffer.clear();
        }
        IoWriter(self)
    }

    /// Flush the buffer.
    fn flush(&mut self) {
        if !self.current_buffer.is_empty() {
            self.current_buffer.push(b'\n');
            self.sender
                .unbounded_send(WorkerCommand::Write(mem::take(&mut self.current_buffer)))
                .unwrap();
        }
    }

    /// Returns a barrier.
    fn get_barrier(&self) -> Barrier {
        Barrier(self.sender.clone())
    }

    /// Returns the amount of space in the current buffer.
    fn space(&self) -> usize {
        // Always leave room for the \n.
        MAX_SERIAL_WRITE_SIZE - 1 - self.current_buffer.len()
    }

    /// Writes a log record.
    async fn log(&mut self, log: &Data<Logs>) {
        if let Some(tags) = log.tags() {
            if tags.iter().any(|tag| self.denied_tags.contains(tag)) {
                return;
            }
        }

        write!(
            self.io_writer(64).await, // 64 is ample
            "[{:05}.{:03}] {:05}:{:05}> [",
            zx::MonotonicDuration::from_nanos(log.metadata.timestamp.into_nanos()).into_seconds(),
            zx::MonotonicDuration::from_nanos(log.metadata.timestamp.into_nanos()).into_millis()
                % 1000,
            log.pid().unwrap_or(0),
            log.tid().unwrap_or(0)
        )
        .unwrap();

        if let Some(tags) = log.tags().filter(|tags| !tags.is_empty()) {
            for (i, tag) in tags.iter().enumerate() {
                self.write(tag.as_bytes()).await;
                if i < tags.len() - 1 {
                    self.write(b", ").await;
                }
            }
        } else {
            self.write(log.component_name().as_bytes()).await;
        }

        // Write this separately from the next so that the line-break, if necessary (unlikely
        // because the tags shouldn't take up much space), comes after this, but before the
        // severity.
        self.write(b"]").await;

        write!(self.io_writer(16).await, " {}: ", log.severity()).unwrap();
        if let Some(m) = log.msg() {
            self.write(m.as_bytes()).await;
        }

        for key_str in log.payload_keys_strings() {
            self.write(b" ").await;
            self.write(key_str.as_bytes()).await;
        }

        // NOTE: Whilst it might be tempting (for performance reasons) to try and buffer up more
        // messages before flushing, there are downstream consumers (in tests) that get confused if
        // part lines are written to the serial log, so we must make sure we write whole lines, and
        // it's easiest if we just send one line at a time.
        self.flush();
    }
}

struct IoWriter<'a>(&'a mut SerialWriter);

impl Write for IoWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.current_buffer.extend(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Barrier(mpsc::UnboundedSender<WorkerCommand>);

impl Barrier {
    /// Asynchronously waits for all pending writes to complete.
    async fn wait(&mut self) {
        let (tx, rx) = oneshot::channel();

        self.0.unbounded_send(WorkerCommand::Flush(tx)).unwrap();

        // Wait for the worker thread to signal that the flush is complete.
        let _ = rx.await;
    }
}

#[cfg(test)]
mod tests {
    use fuchsia_async::{self as fasync};
    use futures::channel::mpsc::{self, unbounded};
    use futures::SinkExt;

    use super::*;
    use crate::identity::ComponentIdentity;
    use crate::logs::testing::make_message;
    use diagnostics_data::{BuilderArgs, LogsDataBuilder, LogsField, LogsProperty, Severity};
    use fuchsia_async::TimeoutExt;
    use futures::FutureExt;
    use moniker::ExtendedMoniker;
    use std::sync::Mutex;
    use std::time::Duration;
    use zx::BootInstant;

    /// TestSink will send log lines received (delimited by \n) over a channel.
    struct TestSink {
        buffer: Vec<u8>,
        snd: mpsc::UnboundedSender<String>,
    }

    impl TestSink {
        fn new() -> (Self, mpsc::UnboundedReceiver<String>) {
            let (snd, rcv) = mpsc::unbounded();
            (Self { buffer: Vec::new(), snd }, rcv)
        }
    }

    impl Write for TestSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            for chunk in buf.split_inclusive(|c| *c == b'\n') {
                if !self.buffer.is_empty() {
                    self.buffer.extend(chunk);
                    if *self.buffer.last().unwrap() == b'\n' {
                        self.snd
                            .unbounded_send(
                                String::from_utf8(std::mem::take(&mut self.buffer))
                                    .expect("wrote valid utf8"),
                            )
                            .expect("sent item");
                    }
                } else if *chunk.last().unwrap() == b'\n' {
                    self.snd
                        .unbounded_send(str::from_utf8(chunk).expect("wrote valid utf8").into())
                        .unwrap();
                } else {
                    self.buffer.extend(chunk);
                }
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// FakeSink collects logs into a buffer.
    #[derive(Clone, Default)]
    struct FakeSink(Arc<Mutex<Vec<u8>>>);

    impl FakeSink {
        fn with_buffer<R>(&self, f: impl FnOnce(&Vec<u8>) -> R) -> R {
            f(&self.0.lock().unwrap())
        }
    }

    impl Write for FakeSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            unreachable!();
        }
    }

    #[fuchsia::test]
    async fn write_to_serial_handles_denied_tags() {
        let log = LogsDataBuilder::new(BuilderArgs {
            timestamp: BootInstant::from_nanos(1),
            component_url: Some("url".into()),
            moniker: "core/foo".try_into().unwrap(),
            severity: Severity::Info,
        })
        .add_tag("denied-tag")
        .build();
        let sink = FakeSink::default();
        let mut writer =
            SerialWriter::new(sink.clone(), HashSet::from_iter(["denied-tag".to_string()]));
        writer.log(&log).await;
        writer.get_barrier().wait().await;
        assert!(sink.with_buffer(|b| b.is_empty()));
    }

    #[fuchsia::test]
    async fn write_to_serial_splits_lines() {
        let message = concat!(
            "Lorem ipsum dolor sit amet, consectetur adipiscing elit. Aliquam accumsan eu neque ",
            "quis molestie. Nam rhoncus sapien non eleifend tristique. Duis quis turpis volutpat ",
            "neque bibendum molestie. Etiam ac sapien justo. Nullam aliquet ipsum nec tincidunt."
        );
        let log = LogsDataBuilder::new(BuilderArgs {
            timestamp: BootInstant::from_nanos(123456789),
            component_url: Some("url".into()),
            moniker: "core/foo".try_into().unwrap(),
            severity: Severity::Info,
        })
        .add_tag("bar")
        .set_message(message)
        .add_key(LogsProperty::String(LogsField::Other("key".to_string()), "value".to_string()))
        .add_key(LogsProperty::Int(LogsField::Other("other_key".to_string()), 3))
        .set_pid(1234)
        .set_tid(5678)
        .build();
        let sink = FakeSink::default();
        let mut writer = SerialWriter::new(sink.clone(), HashSet::new());
        writer.log(&log).await;
        writer.get_barrier().wait().await;
        sink.with_buffer(|b| {
            assert_eq!(
                str::from_utf8(b).unwrap(),
                format!(
                    "[00000.123] 01234:05678> [bar] INFO: {}\n{} key=value other_key=3\n",
                    &message[..218],
                    &message[218..]
                )
            )
        });
    }

    #[fuchsia::test]
    async fn when_no_tags_are_present_the_component_name_is_used() {
        let log = LogsDataBuilder::new(BuilderArgs {
            timestamp: BootInstant::from_nanos(123456789),
            component_url: Some("url".into()),
            moniker: "core/foo".try_into().unwrap(),
            severity: Severity::Info,
        })
        .set_message("my msg")
        .set_pid(1234)
        .set_tid(5678)
        .build();
        let sink = FakeSink::default();
        let mut writer = SerialWriter::new(sink.clone(), HashSet::new());
        writer.log(&log).await;
        writer.flush();
        writer.get_barrier().wait().await;
        sink.with_buffer(|b| {
            assert_eq!(str::from_utf8(b).unwrap(), "[00000.123] 01234:05678> [foo] INFO: my msg\n");
        });
    }

    #[fuchsia::test]
    async fn pauses_logs_correctly() {
        let repo = LogsRepository::for_test(fasync::Scope::new());

        let bootstrap_foo_container = repo.get_log_container(Arc::new(ComponentIdentity::new(
            ExtendedMoniker::parse_str("./bootstrap/foo").unwrap(),
            "fuchsia-pkg://bootstrap-foo",
        )));
        let bootstrap_bar_container = repo.get_log_container(Arc::new(ComponentIdentity::new(
            ExtendedMoniker::parse_str("./bootstrap/bar").unwrap(),
            "fuchsia-pkg://bootstrap-bar",
        )));

        let core_foo_container = repo.get_log_container(Arc::new(ComponentIdentity::new(
            ExtendedMoniker::parse_str("./core/foo").unwrap(),
            "fuchsia-pkg://core-foo",
        )));
        let core_baz_container = repo.get_log_container(Arc::new(ComponentIdentity::new(
            ExtendedMoniker::parse_str("./core/baz").unwrap(),
            "fuchsia-pkg://core-baz",
        )));

        bootstrap_foo_container.ingest_message(make_message(
            "a",
            None,
            zx::BootInstant::from_nanos(1),
        ));

        core_baz_container.ingest_message(make_message("c", None, zx::BootInstant::from_nanos(2)));
        let (sink, mut rcv) = TestSink::new();
        let cloned_repo = Arc::clone(&repo);
        let (mut sender, receiver) = unbounded();
        let _serial_task = fasync::Task::spawn(async move {
            let allowed = vec!["bootstrap/**".into(), "/core/foo".into()];
            let denied = vec!["foo".into()];
            launch_serial(allowed, denied, cloned_repo, sink, receiver).await;
        });
        bootstrap_bar_container.ingest_message(make_message(
            "b",
            Some("foo"),
            zx::BootInstant::from_nanos(3),
        ));

        let received = rcv.next().await.unwrap();

        assert_eq!(received, "[00000.000] 00001:00002> [foo] DEBUG: a\n");

        let (tx, rx) = oneshot::channel();
        sender.send(tx).await.unwrap();

        let freeze_token = rx.await.unwrap();

        core_foo_container.ingest_message(make_message("c", None, zx::BootInstant::from_nanos(4)));

        // The pipeline is asynchronous, so all we can do is assert that no message is sent with a
        // timeout.
        assert!(rcv.next().map(|_| false).on_timeout(Duration::from_millis(500), || true).await);

        drop(freeze_token);

        assert_eq!(rcv.next().await.unwrap(), "[00000.000] 00001:00002> [foo] DEBUG: c\n");
    }

    #[fuchsia::test]
    async fn writes_ingested_logs() {
        let serial_config = SerialConfig::new(vec!["bootstrap/**", "/core/foo"]);
        let repo = LogsRepository::for_test(fasync::Scope::new());

        let bootstrap_foo_container = repo.get_log_container(Arc::new(ComponentIdentity::new(
            ExtendedMoniker::parse_str("./bootstrap/foo").unwrap(),
            "fuchsia-pkg://bootstrap-foo",
        )));
        let bootstrap_bar_container = repo.get_log_container(Arc::new(ComponentIdentity::new(
            ExtendedMoniker::parse_str("./bootstrap/bar").unwrap(),
            "fuchsia-pkg://bootstrap-bar",
        )));

        let core_foo_container = repo.get_log_container(Arc::new(ComponentIdentity::new(
            ExtendedMoniker::parse_str("./core/foo").unwrap(),
            "fuchsia-pkg://core-foo",
        )));
        let core_baz_container = repo.get_log_container(Arc::new(ComponentIdentity::new(
            ExtendedMoniker::parse_str("./core/baz").unwrap(),
            "fuchsia-pkg://core-baz",
        )));

        bootstrap_foo_container.ingest_message(make_message(
            "a",
            None,
            zx::BootInstant::from_nanos(1),
        ));
        core_baz_container.ingest_message(make_message("c", None, zx::BootInstant::from_nanos(2)));
        let (sink, rcv) = TestSink::new();

        let writer = SerialWriter::new(sink, HashSet::from_iter(["foo".to_string()]));
        let _serial_task = fasync::Task::spawn(serial_config.write_logs(Arc::clone(&repo), writer));
        bootstrap_bar_container.ingest_message(make_message(
            "b",
            Some("foo"),
            zx::BootInstant::from_nanos(3),
        ));
        core_foo_container.ingest_message(make_message("c", None, zx::BootInstant::from_nanos(4)));
        let received: Vec<_> = rcv.take(2).collect().await;

        // We must see the logs emitted before we installed the serial listener and after. We must
        // not see the log from /core/baz and we must not see the log from bootstrap/bar with tag
        // "foo".
        assert_eq!(
            received,
            vec![
                "[00000.000] 00001:00002> [foo] DEBUG: a\n",
                "[00000.000] 00001:00002> [foo] DEBUG: c\n"
            ]
        );
    }
}
