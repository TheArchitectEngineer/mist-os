// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context as _, Result};
use async_fs::File;
use async_lock::{Mutex, MutexGuard};
use async_trait::async_trait;
use fuchsia_async::Task;
use futures::prelude::*;
use futures::task::{Context as FutContext, Poll};
use protocols::prelude::*;
use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap};
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};
use tasks::TaskManager;
use thiserror::Error;
use {fidl_fuchsia_developer_ffx as ffx, fidl_fuchsia_tracing_controller as trace};

#[derive(Debug)]
struct TraceTask {
    target_info: ffx::TargetInfo,
    output_file: String,
    config: trace::TraceConfig,
    proxy: Option<trace::SessionProxy>,
    options: ffx::TraceOptions,
    terminate_result: Rc<Mutex<trace::StopResult>>,
    start_time: Instant,
    shutdown_sender: async_channel::Sender<()>,
    task: Task<()>,
    trace_shutdown_complete: Rc<Mutex<bool>>,
}

// This is just implemented for convenience so the wrapper is await-able.
impl Future for TraceTask {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut FutContext<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.task).poll(cx)
    }
}

// Just a wrapper type for ffx::Trigger that does some unwrapping on allocation.
#[derive(Debug, PartialEq, Eq)]
struct TriggerSetItem {
    alert: String,
    action: ffx::Action,
}

impl TriggerSetItem {
    fn new(t: ffx::Trigger) -> Option<Self> {
        let alert = t.alert?;
        let action = t.action?;
        Some(Self { alert, action })
    }

    /// Convenience constructor for doing a lookup.
    fn lookup(alert: String) -> Self {
        Self { alert: alert, action: ffx::Action::Terminate }
    }
}

impl std::cmp::PartialOrd for TriggerSetItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::cmp::Ord for TriggerSetItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.alert.cmp(&other.alert)
    }
}

type TriggersFut<'a> = Pin<Box<dyn Future<Output = Option<ffx::Action>> + 'a>>;

struct TriggersWatcher<'a> {
    inner: TriggersFut<'a>,
}

impl<'a> TriggersWatcher<'a> {
    fn new(
        controller: &'a trace::SessionProxy,
        triggers: Option<Vec<ffx::Trigger>>,
        shutdown: async_channel::Receiver<()>,
    ) -> Self {
        Self {
            inner: Box::pin(async move {
                let set: BTreeSet<TriggerSetItem> = triggers
                    .map(|t| t.into_iter().filter_map(|i| TriggerSetItem::new(i)).collect())
                    .unwrap_or(BTreeSet::new());
                let mut shutdown_fut = shutdown.recv().fuse();
                loop {
                    let mut watch_alert = controller.watch_alert().fuse();
                    futures::select! {
                        _ = shutdown_fut => {
                            tracing::debug!("received shutdown alert");
                            break;
                        }
                        alert = watch_alert => {
                            let Ok(alert) = alert else { break };
                            tracing::trace!("alert received: {}", alert);
                            let lookup_item = TriggerSetItem::lookup(alert);
                            if set.contains(&lookup_item) {
                                return set.get(&lookup_item).map(|s| s.action.clone());
                            }
                        }
                    }
                }
                None
            }),
        }
    }
}

impl Future for TriggersWatcher<'_> {
    type Output = Option<ffx::Action>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut FutContext<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.inner).poll(cx)
    }
}

#[derive(Debug, Error)]
enum TraceTaskStartError {
    #[error("fidl error: {0:?}")]
    FidlError(#[from] fidl::Error),
    #[error("tracing start error: {0:?}")]
    TracingStartError(trace::StartError),
    #[error("general start error: {0:?}")]
    GeneralError(#[from] anyhow::Error),
}

async fn trace_shutdown(
    proxy: &trace::SessionProxy,
) -> Result<trace::StopResult, ffx::RecordingError> {
    proxy
        .stop_tracing(&trace::StopOptions { write_results: Some(true), ..Default::default() })
        .await
        .map_err(|e| {
            tracing::warn!("stopping tracing: {:?}", e);
            ffx::RecordingError::RecordingStop
        })?
        .map_err(|e| {
            tracing::warn!("Received stop error: {:?}", e);
            ffx::RecordingError::RecordingStop
        })
}

impl TraceTask {
    async fn new(
        map: Weak<Mutex<TraceMap>>,
        target_info: ffx::TargetInfo,
        output_file: String,
        options: ffx::TraceOptions,
        config: trace::TraceConfig,
        provisioner: trace::ProvisionerProxy,
    ) -> Result<Self, TraceTaskStartError> {
        let duration = options.duration;
        let (client, server) = fidl::Socket::create_stream();
        let client = fidl::AsyncSocket::from_socket(client);
        let f = File::create(&output_file).await.context("opening file")?;
        let (client_end, server_end) = fidl::endpoints::create_proxy::<trace::SessionMarker>();
        let expanded_categories = match config.categories.clone() {
            Some(categories) => {
                let context = ffx_config::global_env_context()
                    .context("Discovering ffx environment context")?;
                Some(ffx_trace::expand_categories(&context, categories).await?)
            }
            None => None,
        };
        let config_with_expanded_categories =
            trace::TraceConfig { categories: expanded_categories, ..config.clone() };
        provisioner.initialize_tracing(server_end, &config_with_expanded_categories, server)?;
        client_end
            .start_tracing(&trace::StartOptions::default())
            .await?
            .map_err(TraceTaskStartError::TracingStartError)?;
        let output_file_clone = output_file.clone();
        let target_info_clone = target_info.clone();
        let pipe_fut = async move {
            tracing::debug!("{:?} -> {} starting trace.", target_info_clone, output_file_clone);
            let mut out_file = f;
            let res = futures::io::copy(client, &mut out_file)
                .await
                .map_err(|e| tracing::warn!("file error: {:#?}", e));
            tracing::debug!(
                "{:?} -> {} trace complete, result: {:#?}",
                target_info_clone,
                output_file_clone,
                res
            );
            // async_fs files don't guarantee that the file is flushed on drop, so we need to
            // explicitly flush the file after writing.
            if let Err(err) = out_file.flush().await {
                tracing::warn!("file error: {:#?}", err);
            }
        };
        let controller = client_end.clone();
        let triggers = options.triggers.clone();
        let trace_shutdown_complete = Rc::new(Mutex::new(false));
        let terminate_result = Rc::new(Mutex::new(trace::StopResult::default()));
        let (shutdown_sender, shutdown_receiver) = async_channel::bounded::<()>(1);
        Ok(Self {
            target_info: target_info.clone(),
            config,
            proxy: Some(client_end),
            options,
            terminate_result: terminate_result.clone(),
            start_time: Instant::now(),
            shutdown_sender,
            output_file: output_file.clone(),
            trace_shutdown_complete: trace_shutdown_complete.clone(),
            task: Task::local(async move {
                let mut timeout_fut = Box::pin(async move {
                    if let Some(duration) = duration {
                        fuchsia_async::Timer::new(Duration::from_secs_f64(duration)).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                })
                .fuse();
                let mut pipe_fut = Box::pin(pipe_fut).fuse();
                let trigger_proxy = controller.clone();
                let mut trigger_fut =
                    TriggersWatcher::new(&trigger_proxy, triggers, shutdown_receiver).fuse();

                // pipe_fut waits for the trace task to end before completing. We explicitly
                // remove the trace task after shutdown so that pip_fut can finish
                // writing to the output file.
                let drop_task_fut = async move {
                    if let Some(map) = map.upgrade() {
                        let mut map = map.lock().await;
                        let _ = map.output_file_to_nodename.remove(&output_file);
                        let _ = map.nodename_to_task.remove(&target_info.nodename.unwrap_or_else(
                            || {
                                tracing::info!(
                                    "trace writing to '{}' has no target nodename",
                                    output_file
                                );
                                String::new()
                            },
                        ));
                    }
                };
                let shutdown_fut = async move {
                    let mut done = trace_shutdown_complete.lock().await;
                    if !*done {
                        match trace_shutdown(&controller).await {
                            Ok(result) => {
                                let mut terminate_result_guard = terminate_result.lock().await;
                                *terminate_result_guard = result.into();
                            }
                            Err(e) => {
                                tracing::warn!("error shutting down trace: {:?}", e);
                            }
                        }
                        *done = true
                    }
                    // Remove the controller.
                    drop(controller);
                };
                futures::select! {
                    _ = pipe_fut => {},
                    _ = timeout_fut => {
                        tracing::debug!("timeout reached, doing cleanup");
                        shutdown_fut.await;
                        drop(trigger_fut);
                        drop(trigger_proxy);
                        let _ = futures::join!(
                            drop_task_fut,
                            pipe_fut
                        );
                    }
                    action = trigger_fut => {
                        if let Some(action) = action {
                            match action {
                                ffx::Action::Terminate => {
                                    tracing::debug!("received terminate trigger");
                                }
                            }
                        }
                        shutdown_fut.await;
                        drop(trigger_fut);
                        drop(trigger_proxy);
                        let _ = futures::join!(
                            drop_task_fut,
                            pipe_fut
                        );
                    }
                };
            }),
        })
    }

    async fn shutdown(mut self) -> Result<trace::StopResult, ffx::RecordingError> {
        {
            let proxy = self.proxy.take().expect("missing trace session proxy");
            let mut trace_shutdown_done = self.trace_shutdown_complete.lock().await;
            if !*trace_shutdown_done {
                match trace_shutdown(&proxy).await {
                    Ok(trace_result) => {
                        let mut terminate_result_guard = self.terminate_result.lock().await;
                        *terminate_result_guard = trace_result.into();
                    }
                    Err(e) => {
                        tracing::warn!("error shutting down trace: {:?}", e);
                    }
                };
                *trace_shutdown_done = true;
            }
        }
        let target_info_clone = self.target_info.clone();
        let output_file = self.output_file.clone();
        let terminate_result: Rc<Mutex<trace::StopResult>> = self.terminate_result.clone();
        let _ = self.shutdown_sender.send(()).await;
        self.await;
        tracing::trace!("trace task {:?} -> {} shutdown completed", target_info_clone, output_file);
        let terminate_result_guard = terminate_result.lock().await;
        Ok(terminate_result_guard.clone())
    }
}

#[derive(Default)]
struct TraceMap {
    nodename_to_task: HashMap<String, TraceTask>,
    output_file_to_nodename: HashMap<String, String>,
}

#[ffx_protocol]
#[derive(Default)]
pub struct TracingProtocol {
    tasks: Rc<Mutex<TraceMap>>,
    iter_tasks: TaskManager,
}

async fn get_controller_proxy(
    target_query: Option<&String>,
    cx: &Context,
) -> Result<(ffx::TargetInfo, trace::ProvisionerProxy)> {
    let (target, proxy) = cx
        .open_target_proxy_with_info::<trace::ProvisionerMarker>(
            target_query.cloned(),
            "/core/trace_manager",
        )
        .await?;
    Ok((target, proxy))
}

impl TracingProtocol {
    async fn remove_output_file_or_find_target_nodename(
        &self,
        cx: &Context,
        tasks: &mut MutexGuard<'_, TraceMap>,
        output_file: &String,
    ) -> Result<String, ffx::RecordingError> {
        match tasks.output_file_to_nodename.remove(output_file) {
            Some(n) => Ok(n),
            None => {
                let target = cx
                    .get_target_collection()
                    .await
                    .map_err(|e| {
                        tracing::warn!("unable to get target collection: {:?}", e);
                        ffx::RecordingError::RecordingStop
                    })?
                    .query_single_enabled_target(&output_file.to_string().into())
                    .map_err(|_| {
                        tracing::warn!("target query '{output_file}' matches multiple targets");
                        ffx::RecordingError::NoSuchTarget
                    })?
                    .ok_or_else(|| {
                        tracing::warn!("target query '{}' matches no targets", output_file);
                        ffx::RecordingError::NoSuchTarget
                    })?;

                target.nodename().ok_or_else(|| {
                    tracing::warn!(
                        "target query '{}' matches target with no nodename",
                        output_file
                    );
                    ffx::RecordingError::DisconnectedTarget
                })
            }
        }
    }
}

#[async_trait(?Send)]
impl FidlProtocol for TracingProtocol {
    type Protocol = ffx::TracingMarker;
    type StreamHandler = FidlStreamHandler<Self>;

    async fn handle(&self, cx: &Context, req: ffx::TracingRequest) -> Result<()> {
        match req {
            ffx::TracingRequest::StartRecording {
                target_query,
                output_file,
                options,
                target_config,
                responder,
            } => {
                let mut tasks = self.tasks.lock().await;
                let target_query = target_query.string_matcher;
                let (target_info, provisioner) =
                    match get_controller_proxy(target_query.as_ref(), cx).await {
                        Ok(p) => p,
                        Err(e) => {
                            tracing::warn!("getting target controller proxy: {:?}", e);
                            return responder
                                .send(Err(ffx::RecordingError::TargetProxyOpen))
                                .map_err(Into::into);
                        }
                    };
                // This should functionally never happen (a target whose nodename isn't
                // known after having been identified for service discovery would be a
                // critical error).
                let nodename = match target_info.nodename {
                    Some(ref n) => n.clone(),
                    None => {
                        tracing::warn!(
                            "query does not match a valid target with nodename: {:?}",
                            target_query
                        );
                        return responder
                            .send(Err(ffx::RecordingError::TargetProxyOpen))
                            .map_err(Into::into);
                    }
                };
                match tasks.output_file_to_nodename.entry(output_file.clone()) {
                    Entry::Occupied(_) => {
                        return responder
                            .send(Err(ffx::RecordingError::DuplicateTraceFile))
                            .map_err(Into::into);
                    }
                    Entry::Vacant(e) => {
                        let task = match TraceTask::new(
                            Rc::downgrade(&self.tasks),
                            target_info.clone(),
                            output_file.clone(),
                            options,
                            target_config,
                            provisioner,
                        )
                        .await
                        {
                            Ok(t) => t,
                            Err(e) => {
                                tracing::warn!("unable to start trace: {:?}", e);
                                let res = match e {
                                    TraceTaskStartError::TracingStartError(t) => match t {
                                        trace::StartError::AlreadyStarted => {
                                            Err(ffx::RecordingError::RecordingAlreadyStarted)
                                        }
                                        e => {
                                            tracing::warn!("Start error: {:?}", e);
                                            Err(ffx::RecordingError::RecordingStart)
                                        }
                                    },
                                    e => {
                                        tracing::warn!("Start error: {:?}", e);
                                        Err(ffx::RecordingError::RecordingStart)
                                    }
                                };
                                return responder.send(res).map_err(Into::into);
                            }
                        };
                        e.insert(nodename.clone());
                        tasks.nodename_to_task.insert(nodename, task);
                    }
                }
                responder.send(Ok(&target_info)).map_err(Into::into)
            }
            ffx::TracingRequest::StopRecording { name, responder } => {
                let task = {
                    let mut tasks = self.tasks.lock().await;
                    let nodename = match self
                        .remove_output_file_or_find_target_nodename(cx, &mut tasks, &name)
                        .await
                    {
                        Ok(n) => n,
                        Err(e) => return responder.send(Err(e)).map_err(Into::into),
                    };
                    if let Some(task) = tasks.nodename_to_task.remove(&nodename) {
                        // If we have found the task using nodename and not output file, the
                        // output_file_to_nodename mapping might still be around. Explicitly
                        // remove it to be sure.
                        let _ = tasks.output_file_to_nodename.remove(&task.output_file);
                        task
                    } else {
                        // TODO(https://fxbug.dev/42167418)
                        tracing::warn!("no task associated with trace file '{}'", name);
                        return responder
                            .send(Err(ffx::RecordingError::NoSuchTraceFile))
                            .map_err(Into::into);
                    }
                };
                let output_file = task.output_file.clone();
                let target_info = task.target_info.clone();
                let categories = task.config.categories.clone().unwrap_or_default();
                responder
                    .send(match task.shutdown().await {
                        Ok(ref result) => Ok((&target_info, &output_file, &categories, result)),
                        Err(e) => Err(e),
                    })
                    .map_err(Into::into)
            }
            ffx::TracingRequest::Status { iterator, responder } => {
                let mut stream = iterator.into_stream();
                let res = self
                    .tasks
                    .lock()
                    .await
                    .nodename_to_task
                    .values()
                    .map(|t| ffx::TraceInfo {
                        target: Some(t.target_info.clone()),
                        output_file: Some(t.output_file.clone()),
                        duration: t.options.duration.clone(),
                        remaining_runtime: t.options.duration.clone().map(|d| {
                            Duration::from_secs_f64(d)
                                .checked_sub(t.start_time.elapsed())
                                .unwrap_or(Duration::from_secs(0))
                                .as_secs_f64()
                        }),
                        config: Some(t.config.clone()),
                        triggers: t.options.triggers.clone(),
                        ..Default::default()
                    })
                    .collect::<Vec<_>>();
                self.iter_tasks.spawn(async move {
                    const CHUNK_SIZE: usize = 20;
                    let mut iter = res.chunks(CHUNK_SIZE).fuse();
                    while let Ok(Some(ffx::TracingStatusIteratorRequest::GetNext { responder })) =
                        stream.try_next().await
                    {
                        let _ = responder.send(iter.next().unwrap_or(&[])).map_err(|e| {
                            tracing::warn!("responding to tracing status iterator: {:?}", e);
                        });
                    }
                });
                responder.send().map_err(Into::into)
            }
        }
    }

    async fn stop(&mut self, _cx: &Context) -> Result<()> {
        let tasks = {
            let mut tasks = self.tasks.lock().await;
            tasks.output_file_to_nodename.clear();
            tasks.nodename_to_task.drain().map(|(_, v)| v.shutdown()).collect::<Vec<_>>()
        };
        futures::future::join_all(tasks).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocols::testing::FakeDaemonBuilder;
    use std::cell::RefCell;

    const FAKE_CONTROLLER_TRACE_OUTPUT: &'static str = "HOWDY HOWDY HOWDY";

    #[derive(Default)]
    struct FakeProvisioner {
        start_error: Option<trace::StartError>,
    }

    #[async_trait(?Send)]
    impl FidlProtocol for FakeProvisioner {
        type Protocol = trace::ProvisionerMarker;
        type StreamHandler = FidlStreamHandler<Self>;

        async fn handle(&self, _cx: &Context, req: trace::ProvisionerRequest) -> Result<()> {
            match req {
                trace::ProvisionerRequest::InitializeTracing { controller, output, .. } => {
                    let start_error = self.start_error;
                    let mut stream = controller.into_stream();
                    while let Ok(Some(req)) = stream.try_next().await {
                        match req {
                            trace::SessionRequest::StartTracing { responder, .. } => {
                                let response = match start_error {
                                    Some(e) => Err(e),
                                    None => Ok(()),
                                };
                                responder.send(response).expect("Failed to start")
                            }
                            trace::SessionRequest::StopTracing { responder, payload } => {
                                if start_error.is_some() {
                                    responder
                                        .send(Err(trace::StopError::NotStarted))
                                        .expect("Failed to stop")
                                } else {
                                    assert_eq!(payload.write_results.unwrap(), true);
                                    assert_eq!(
                                        FAKE_CONTROLLER_TRACE_OUTPUT.len(),
                                        output
                                            .write(FAKE_CONTROLLER_TRACE_OUTPUT.as_bytes())
                                            .unwrap()
                                    );
                                    responder
                                        .send(Ok(&trace::StopResult::default()))
                                        .expect("Failed to stop")
                                }
                                break;
                            }
                            trace::SessionRequest::WatchAlert { responder } => {
                                responder.send("").expect("Unable to send alert");
                            }
                            r => panic!("unexpected request: {:#?}", r),
                        }
                    }
                    Ok(())
                }
                r => panic!("unexpected request: {:#?}", r),
            }
        }
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_trace_start_stop_write_check() {
        let daemon = FakeDaemonBuilder::new()
            .register_fidl_protocol::<FakeProvisioner>()
            .register_fidl_protocol::<TracingProtocol>()
            .target(ffx::TargetInfo { nodename: Some("foobar".to_string()), ..Default::default() })
            .build();
        let proxy = daemon.open_proxy::<ffx::TracingMarker>().await;
        let temp_dir = tempfile::TempDir::new().unwrap();
        let output = temp_dir.path().join("trace-test.fxt").into_os_string().into_string().unwrap();
        proxy
            .start_recording(
                &ffx::TargetQuery {
                    string_matcher: Some("foobar".to_owned()),
                    ..Default::default()
                },
                &output,
                &ffx::TraceOptions::default(),
                &trace::TraceConfig::default(),
            )
            .await
            .unwrap()
            .unwrap();
        proxy.stop_recording(&output).await.unwrap().unwrap();

        let mut f = File::open(std::path::PathBuf::from(output)).await.unwrap();
        let mut res = String::new();
        f.read_to_string(&mut res).await.unwrap();
        assert_eq!(res, FAKE_CONTROLLER_TRACE_OUTPUT.to_string());
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_trace_error_double_start() {
        let daemon = FakeDaemonBuilder::new()
            .register_fidl_protocol::<FakeProvisioner>()
            .register_fidl_protocol::<TracingProtocol>()
            .target(ffx::TargetInfo { nodename: Some("foobar".to_string()), ..Default::default() })
            .build();
        let proxy = daemon.open_proxy::<ffx::TracingMarker>().await;
        let temp_dir = tempfile::TempDir::new().unwrap();
        let output = temp_dir.path().join("trace-test.fxt").into_os_string().into_string().unwrap();
        proxy
            .start_recording(
                &ffx::TargetQuery {
                    string_matcher: Some("foobar".to_owned()),
                    ..Default::default()
                },
                &output,
                &ffx::TraceOptions::default(),
                &trace::TraceConfig::default(),
            )
            .await
            .unwrap()
            .unwrap();
        // The target query needs to be empty here in order to fall back to checking
        // the trace file.
        assert_eq!(
            Err(ffx::RecordingError::DuplicateTraceFile),
            proxy
                .start_recording(
                    &ffx::TargetQuery::default(),
                    &output,
                    &ffx::TraceOptions::default(),
                    &trace::TraceConfig::default()
                )
                .await
                .unwrap()
        );
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_trace_error_handling_already_started() {
        let fake_provisioner = Rc::new(RefCell::new(FakeProvisioner::default()));
        fake_provisioner.borrow_mut().start_error.replace(trace::StartError::AlreadyStarted);
        let daemon = FakeDaemonBuilder::new()
            .register_fidl_protocol::<TracingProtocol>()
            .inject_fidl_protocol(fake_provisioner)
            .target(ffx::TargetInfo { nodename: Some("foobar".to_string()), ..Default::default() })
            .build();
        let proxy = daemon.open_proxy::<ffx::TracingMarker>().await;
        let temp_dir = tempfile::TempDir::new().unwrap();
        let output = temp_dir.path().join("trace-test.fxt").into_os_string().into_string().unwrap();
        assert_eq!(
            Err(ffx::RecordingError::RecordingAlreadyStarted),
            proxy
                .start_recording(
                    &ffx::TargetQuery {
                        string_matcher: Some("foobar".to_owned()),
                        ..Default::default()
                    },
                    &output,
                    &ffx::TraceOptions::default(),
                    &trace::TraceConfig::default()
                )
                .await
                .unwrap()
        );
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_trace_error_handling_generic_start_error() {
        let fake_provisioner = Rc::new(RefCell::new(FakeProvisioner::default()));
        fake_provisioner.borrow_mut().start_error.replace(trace::StartError::NotInitialized);
        let daemon = FakeDaemonBuilder::new()
            .register_fidl_protocol::<TracingProtocol>()
            .inject_fidl_protocol(fake_provisioner)
            .target(ffx::TargetInfo { nodename: Some("foobar".to_string()), ..Default::default() })
            .build();
        let proxy = daemon.open_proxy::<ffx::TracingMarker>().await;
        let temp_dir = tempfile::TempDir::new().unwrap();
        let output = temp_dir.path().join("trace-test.fxt").into_os_string().into_string().unwrap();
        assert_eq!(
            Err(ffx::RecordingError::RecordingStart),
            proxy
                .start_recording(
                    &ffx::TargetQuery {
                        string_matcher: Some("foobar".to_owned()),
                        ..Default::default()
                    },
                    &output,
                    &ffx::TraceOptions::default(),
                    &trace::TraceConfig::default()
                )
                .await
                .unwrap()
        );
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_trace_shutdown_no_trace() {
        let daemon = FakeDaemonBuilder::new().register_fidl_protocol::<TracingProtocol>().build();
        let proxy = daemon.open_proxy::<ffx::TracingMarker>().await;
        let temp_dir = tempfile::TempDir::new().unwrap();
        let output = temp_dir.path().join("trace-test.fxt").into_os_string().into_string().unwrap();
        assert_eq!(
            ffx::RecordingError::NoSuchTarget,
            proxy.stop_recording(&output).await.unwrap().unwrap_err()
        );
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_trace_duration_shutdown_via_output_file() {
        let daemon = FakeDaemonBuilder::new()
            .register_fidl_protocol::<FakeProvisioner>()
            .target(ffx::TargetInfo { nodename: Some("foobar".to_owned()), ..Default::default() })
            .build();
        let protocol = Rc::new(RefCell::new(TracingProtocol::default()));
        let (proxy, _task) = protocols::testing::create_proxy(protocol.clone(), &daemon).await;
        let temp_dir = tempfile::TempDir::new().unwrap();
        let output = temp_dir.path().join("trace-test.fxt").into_os_string().into_string().unwrap();
        proxy
            .start_recording(
                &ffx::TargetQuery {
                    string_matcher: Some("foobar".to_owned()),
                    ..Default::default()
                },
                &output,
                &ffx::TraceOptions { duration: Some(500000.0), ..Default::default() },
                &trace::TraceConfig::default(),
            )
            .await
            .unwrap()
            .unwrap();
        proxy.stop_recording(&output).await.unwrap().unwrap();

        let mut f = File::open(std::path::PathBuf::from(output)).await.unwrap();
        let mut res = String::new();
        f.read_to_string(&mut res).await.unwrap();
        assert_eq!(res, FAKE_CONTROLLER_TRACE_OUTPUT.to_string());
        let tasks = protocol.borrow().tasks.clone();
        assert!(tasks.lock().await.nodename_to_task.is_empty());
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_trace_duration_shutdown_via_nodename() {
        let daemon = FakeDaemonBuilder::new()
            .register_fidl_protocol::<FakeProvisioner>()
            .target(ffx::TargetInfo { nodename: Some("foobar".to_string()), ..Default::default() })
            .build();
        let protocol = Rc::new(RefCell::new(TracingProtocol::default()));
        let (proxy, _task) = protocols::testing::create_proxy(protocol.clone(), &daemon).await;
        let temp_dir = tempfile::TempDir::new().unwrap();
        let output = temp_dir.path().join("trace-test.fxt").into_os_string().into_string().unwrap();
        proxy
            .start_recording(
                &ffx::TargetQuery {
                    string_matcher: Some("foobar".to_owned()),
                    ..Default::default()
                },
                &output,
                &ffx::TraceOptions { duration: Some(500000.0), ..Default::default() },
                &trace::TraceConfig::default(),
            )
            .await
            .unwrap()
            .unwrap();
        proxy.stop_recording("foobar").await.unwrap().unwrap();

        let mut f = File::open(std::path::PathBuf::from(output)).await.unwrap();
        let mut res = String::new();
        f.read_to_string(&mut res).await.unwrap();
        assert_eq!(res, FAKE_CONTROLLER_TRACE_OUTPUT.to_string());
        let tasks = protocol.borrow().tasks.clone();
        assert!(tasks.lock().await.nodename_to_task.is_empty());
    }

    fn spawn_fake_alert_watcher(alert: &'static str) -> trace::SessionProxy {
        let (proxy, server) = fidl::endpoints::create_proxy::<trace::SessionMarker>();
        let mut stream = server.into_stream();
        fuchsia_async::Task::local(async move {
            while let Ok(Some(req)) = stream.try_next().await {
                match req {
                    trace::SessionRequest::WatchAlert { responder } => {
                        responder.send(alert).unwrap();
                    }
                    r => panic!("unexpected request in this test: {:?}", r),
                }
            }
        })
        .detach();
        proxy
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_triggers_valid() {
        let proxy = spawn_fake_alert_watcher("foober");
        let (_sender, receiver) = async_channel::bounded::<()>(1);
        let triggers = Some(vec![
            ffx::Trigger { alert: Some("foo".to_owned()), action: None, ..Default::default() },
            ffx::Trigger {
                alert: Some("foober".to_owned()),
                action: Some(ffx::Action::Terminate),
                ..Default::default()
            },
        ]);
        let res = TriggersWatcher::new(&proxy, triggers, receiver).await;
        assert_eq!(res, Some(ffx::Action::Terminate));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_triggers_server_dropped() {
        let (proxy, server) = fidl::endpoints::create_proxy::<trace::SessionMarker>();
        let (_sender, receiver) = async_channel::bounded::<()>(1);
        drop(server);
        let triggers = Some(vec![
            ffx::Trigger { alert: Some("foo".to_owned()), action: None, ..Default::default() },
            ffx::Trigger {
                alert: Some("foober".to_owned()),
                action: Some(ffx::Action::Terminate),
                ..Default::default()
            },
        ]);
        let res = TriggersWatcher::new(&proxy, triggers, receiver).await;
        assert_eq!(res, None);
    }
}
