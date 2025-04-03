// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::builtin_environment::{BuiltinEnvironment, BuiltinEnvironmentBuilder};
use crate::capability;
use crate::framework::realm::Realm;
use crate::model::component::instance::InstanceState;
use crate::model::component::{ComponentInstance, StartReason};
use crate::model::events::registry::EventSubscription;
use crate::model::model::Model;
use crate::model::testing::mocks::{ControlMessage, MockResolver, MockRunner};
use crate::model::testing::test_hook::TestHook;
use camino::Utf8PathBuf;
use cm_config::RuntimeConfig;
use cm_rust::{
    Availability, CapabilityDecl, ComponentDecl, ConfigChecksum, ConfigDecl, ConfigField,
    ConfigSingleValue, ConfigValue, ConfigValueSource, ConfigValueSpec, ConfigValueType,
    ConfigValuesData, EventStreamDecl, NativeIntoFidl, RunnerDecl, UseEventStreamDecl, UseSource,
};
use cm_rust_testing::*;
use cm_types::Url;
use fidl::endpoints;
use futures::channel::mpsc::Receiver;
use futures::lock::Mutex;
use futures::{StreamExt, TryStreamExt};
use hooks::{EventType, HooksRegistration};
use moniker::{ChildName, Moniker};
use std::collections::HashSet;
use std::sync::Arc;
use vfs::directory::entry::DirectoryEntry;
use vfs::service;
use zx::{self as zx, Koid};
use {
    fidl_fuchsia_component as fcomponent, fidl_fuchsia_component_decl as fdecl,
    fidl_fuchsia_component_runner as fcrunner, fidl_fuchsia_io as fio, fidl_fuchsia_sys2 as fsys,
};

pub const TEST_RUNNER_NAME: &str = cm_rust_testing::TEST_RUNNER_NAME;

// TODO(https://fxbug.dev/42140194): remove function wrappers once the routing_test_helpers
// lib has a stable API.
pub fn default_component_decl() -> ComponentDecl {
    ::routing_test_helpers::default_component_decl()
}

pub fn component_decl_with_test_runner() -> ComponentDecl {
    ::routing_test_helpers::component_decl_with_test_runner()
}

pub struct ComponentInfo {
    pub component: Arc<ComponentInstance>,
    pub channel_id: Koid,
}

impl ComponentInfo {
    /// Given a `ComponentInstance` which has been bound, look up the resolved URL
    /// and package into a `ComponentInfo` struct.
    pub async fn new(component: Arc<ComponentInstance>) -> ComponentInfo {
        // The koid is the only unique piece of information we have about
        // a component start request. Two start requests for the same
        // component URL look identical to the Runner, the only difference
        // being the Channel passed to the Runner to use for the
        // ComponentController protocol.
        let koid = {
            component
                .lock_state()
                .await
                .get_started_state()
                .expect("expected component to be running")
                .program_koid()
                .expect("program is unexpectedly missing")
        };

        ComponentInfo { component, channel_id: koid }
    }

    /// Checks that the component is shut down, panics if this is not true.
    pub async fn check_is_shut_down(&self, runner: &MockRunner) {
        // Check the list of requests for this component
        let request_map = runner.get_request_map();
        let unlocked_map = request_map.lock().await;
        let request_vec = unlocked_map
            .get(&self.channel_id)
            .expect("request map didn't have channel id, perhaps the controller wasn't started?");
        assert_eq!(*request_vec, vec![ControlMessage::Stop]);

        assert!(self.component.lock_state().await.is_shut_down());
    }

    /// Checks that the component has not been shut down, panics if it has.
    pub async fn check_not_shut_down(&self, runner: &MockRunner) {
        // If the MockController has started, check that no stop requests have
        // been received.
        let request_map = runner.get_request_map();
        let unlocked_map = request_map.lock().await;
        if let Some(request_vec) = unlocked_map.get(&self.channel_id) {
            assert_eq!(*request_vec, vec![]);
        }

        assert!(!self.component.lock_state().await.is_shut_down());
    }
}

pub async fn execution_is_shut_down(component: &ComponentInstance) -> bool {
    component.lock_state().await.is_shut_down()
}

/// Returns true if the given child (live or deleting) exists.
pub async fn has_child<'a>(component: &'a ComponentInstance, moniker: &'a str) -> bool {
    match *component.lock_state().await {
        InstanceState::Resolved(ref s) | InstanceState::Started(ref s, _) => {
            s.children().map(|(k, _)| k.clone()).any(|m| m == moniker.try_into().unwrap())
        }
        InstanceState::Shutdown(ref state, _) => {
            state.children.iter().map(|(k, _)| k.clone()).any(|m| m == moniker.try_into().unwrap())
        }
        InstanceState::Destroyed => false,
        _ => panic!("not resolved"),
    }
}

/// Return the incarnation id of the given child.
pub async fn get_incarnation_id<'a>(component: &'a ComponentInstance, moniker: &'a str) -> u64 {
    component
        .lock_state()
        .await
        .get_resolved_state()
        .expect("not resolved")
        .get_child(&moniker.try_into().unwrap())
        .unwrap()
        .incarnation_id()
}

/// Return all monikers of the live children of the given `component`.
pub async fn get_live_children(component: &ComponentInstance) -> HashSet<ChildName> {
    match *component.lock_state().await {
        InstanceState::Resolved(ref s) | InstanceState::Started(ref s, _) => {
            s.children().map(|(m, _)| m.clone()).collect()
        }
        InstanceState::Shutdown(ref s, _) => s.children.iter().map(|(m, _)| m.clone()).collect(),
        InstanceState::Destroyed => HashSet::new(),
        _ => panic!("not resolved"),
    }
}

/// Return the child of the given `component` with moniker `child`.
pub async fn get_live_child<'a>(
    component: &'a ComponentInstance,
    child: &'a str,
) -> Arc<ComponentInstance> {
    component
        .lock_state()
        .await
        .get_resolved_state()
        .expect("not resolved")
        .get_child(&child.try_into().unwrap())
        .unwrap()
        .clone()
}

pub async fn list_directory<'a>(root_proxy: &'a fio::DirectoryProxy) -> Vec<String> {
    let entries = fuchsia_fs::directory::readdir(&root_proxy).await.expect("readdir failed");
    let mut items = entries.iter().map(|entry| entry.name.clone()).collect::<Vec<String>>();
    items.sort();
    items
}

pub async fn list_directory_recursive<'a>(root_proxy: &'a fio::DirectoryProxy) -> Vec<String> {
    let entries = fuchsia_fs::directory::readdir_recursive(&root_proxy, /*timeout=*/ None);
    let mut items = entries
        .map(|result| result.map(|entry| entry.name.clone()))
        .try_collect::<Vec<_>>()
        .await
        .expect("readdir failed");
    items.sort();
    items
}

pub async fn write_file<'a>(root_proxy: &'a fio::DirectoryProxy, path: &'a str, contents: &'a str) {
    let file_proxy = fuchsia_fs::directory::open_file_async(
        &root_proxy,
        path,
        fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_WRITABLE,
    )
    .expect("Failed to open file.");
    let _: u64 = file_proxy
        .write(contents.as_bytes())
        .await
        .expect("Unable to write file.")
        .map_err(zx::Status::from_raw)
        .expect("Write failed");
}

/// Create a `DirectoryEntry` and `Channel` pair. The created `DirectoryEntry`
/// provides the service `P`, sending all requests to the returned channel.
pub fn create_service_directory_entry<P>(
) -> (Arc<dyn DirectoryEntry>, futures::channel::mpsc::Receiver<fidl::endpoints::Request<P>>)
where
    P: fidl::endpoints::ProtocolMarker,
    fidl::endpoints::Request<P>: Send,
{
    use futures::sink::SinkExt;
    let (sender, receiver) = futures::channel::mpsc::channel(0);
    let entry = service::host(move |mut stream: P::RequestStream| {
        let mut sender = sender.clone();
        async move {
            while let Ok(Some(request)) = stream.try_next().await {
                sender.send(request).await.unwrap();
            }
        }
    });
    (entry, receiver)
}

/// Wait for a ComponentRunnerStart request, acknowledge it, and return
/// the start info.
///
/// Panics if the channel closes before we receive a request.
pub async fn wait_for_runner_request(
    recv: &mut Receiver<fcrunner::ComponentRunnerRequest>,
) -> fcrunner::ComponentStartInfo {
    let fcrunner::ComponentRunnerRequest::Start { start_info, .. } =
        recv.next().await.expect("Channel closed before request was received.")
    else {
        panic!("unknown runner request");
    };
    start_info
}

/// Contains test model and ancillary objects.
pub struct TestModelResult {
    pub model: Arc<Model>,
    pub builtin_environment: Arc<Mutex<BuiltinEnvironment>>,
    pub realm_proxy: Option<fcomponent::RealmProxy>,
    pub mock_runner: Arc<MockRunner>,
    pub mock_resolver: Arc<MockResolver>,
}

pub struct TestEnvironmentBuilder {
    root_component: String,
    components: Vec<(&'static str, ComponentDecl)>,
    config_values: Vec<(&'static str, ConfigValuesData)>,
    runtime_config: RuntimeConfig,
    component_id_index_path: Option<Utf8PathBuf>,
    realm_moniker: Option<Moniker>,
    hooks: Vec<HooksRegistration>,
    front_hooks: Vec<HooksRegistration>,
}

impl TestEnvironmentBuilder {
    pub fn new() -> Self {
        Self {
            root_component: "root".to_owned(),
            components: vec![],
            config_values: vec![],
            runtime_config: Default::default(),
            component_id_index_path: None,
            realm_moniker: None,
            hooks: vec![],
            front_hooks: vec![],
        }
    }

    pub fn set_root_component(mut self, root_component: &str) -> Self {
        self.root_component = root_component.to_owned();
        self
    }

    pub fn set_components(mut self, components: Vec<(&'static str, ComponentDecl)>) -> Self {
        self.components = components;
        self
    }

    pub fn set_config_values(
        mut self,
        config_values: Vec<(&'static str, ConfigValuesData)>,
    ) -> Self {
        self.config_values = config_values;
        self
    }

    pub fn set_component_id_index_path(mut self, path: Utf8PathBuf) -> Self {
        self.component_id_index_path = Some(path);
        self
    }

    pub fn set_runtime_config(mut self, runtime_config: RuntimeConfig) -> Self {
        self.runtime_config = runtime_config;
        self
    }

    pub fn set_realm_moniker(mut self, moniker: Moniker) -> Self {
        self.realm_moniker = Some(moniker);
        self
    }

    pub fn set_hooks(mut self, hooks: Vec<HooksRegistration>) -> Self {
        self.hooks = hooks;
        self
    }

    pub fn set_front_hooks(mut self, hooks: Vec<HooksRegistration>) -> Self {
        self.front_hooks = hooks;
        self
    }

    /// Returns a `Model` and `BuiltinEnvironment` suitable for most tests.
    pub async fn build(mut self) -> TestModelResult {
        let mock_runner = Arc::new(MockRunner::new());

        let mock_resolver = MockResolver::new();
        for (name, decl) in &self.components {
            mock_resolver.add_component(name, decl.clone());
        }

        for (path, config) in &self.config_values {
            mock_resolver.add_config_values(path, config.clone());
        }

        self.runtime_config.root_component_url =
            Some(Url::new(format!("test:///{}", self.root_component)).unwrap());
        self.runtime_config.builtin_capabilities.push(CapabilityDecl::Runner(RunnerDecl {
            name: TEST_RUNNER_NAME.parse().unwrap(),
            source_path: None,
        }));
        self.runtime_config.builtin_capabilities.push(CapabilityDecl::EventStream(
            EventStreamDecl { name: "started".parse().unwrap() },
        ));
        self.runtime_config.component_id_index_path = self.component_id_index_path;
        self.runtime_config.enable_introspection = true;

        let mock_resolver = Arc::new(mock_resolver);
        let builtin_environment = Arc::new(Mutex::new(
            BuiltinEnvironmentBuilder::new()
                .add_resolver("test".to_string(), mock_resolver.clone())
                .add_runner(TEST_RUNNER_NAME.parse().unwrap(), mock_runner.clone(), true)
                .set_runtime_config(self.runtime_config)
                .build()
                .await
                .expect("builtin environment setup failed"),
        ));
        let model = builtin_environment.lock().await.model.clone();

        model.root().hooks.install(self.hooks).await;
        model.root().hooks.install_front_for_test(self.front_hooks).await;

        // Host framework service for `moniker`, if requested.
        let realm_proxy = if let Some(moniker) = self.realm_moniker {
            let component = model
                .root()
                .find_and_maybe_resolve(&moniker)
                .await
                .unwrap_or_else(|e| panic!("could not look up {}: {:?}", moniker, e));
            let host = Realm::new(Arc::downgrade(&model), model.context().runtime_config().clone());
            let (realm_proxy, server) = endpoints::create_proxy::<fcomponent::RealmMarker>();
            capability::open_framework(&host, &component, server.into()).await.unwrap();
            Some(realm_proxy)
        } else {
            None
        };

        TestModelResult { model, builtin_environment, realm_proxy, mock_runner, mock_resolver }
    }
}

/// A test harness for tests that wish to register or verify actions.
pub struct ActionsTest {
    pub model: Arc<Model>,
    pub builtin_environment: Arc<Mutex<BuiltinEnvironment>>,
    pub test_hook: Arc<TestHook>,
    pub realm_proxy: Option<fcomponent::RealmProxy>,
    pub runner: Arc<MockRunner>,
    pub resolver: Arc<MockResolver>,
}

impl ActionsTest {
    pub async fn new(
        root_component: &'static str,
        components: Vec<(&'static str, ComponentDecl)>,
        moniker: Option<Moniker>,
    ) -> Self {
        Self::new_with_hooks(root_component, components, moniker, vec![]).await
    }

    pub async fn new_with_hooks(
        root_component: &'static str,
        components: Vec<(&'static str, ComponentDecl)>,
        moniker: Option<Moniker>,
        extra_hooks: Vec<HooksRegistration>,
    ) -> Self {
        let test_hook = Arc::new(TestHook::new());
        let mut hooks = test_hook.hooks();
        hooks.extend(extra_hooks);
        let builder = TestEnvironmentBuilder::new()
            .set_root_component(root_component)
            .set_components(components)
            .set_hooks(hooks);
        let builder =
            if let Some(moniker) = moniker { builder.set_realm_moniker(moniker) } else { builder };
        let TestModelResult { model, builtin_environment, realm_proxy, mock_runner, mock_resolver } =
            builder.build().await;

        Self {
            model,
            builtin_environment,
            test_hook,
            realm_proxy,
            runner: mock_runner,
            resolver: mock_resolver,
        }
    }

    pub async fn look_up(&self, moniker: Moniker) -> Arc<ComponentInstance> {
        self.model
            .root()
            .find_and_maybe_resolve(&moniker)
            .await
            .unwrap_or_else(|e| panic!("could not look up {}: {:?}", moniker, e))
    }

    pub async fn start(&self, moniker: Moniker) -> Arc<ComponentInstance> {
        self.model
            .root()
            .start_instance(&moniker, &StartReason::Eager)
            .await
            .unwrap_or_else(|e| panic!("could not start {}: {:?}", moniker, e))
    }

    /// Add a dynamic child to the given collection, with the given name to the
    /// component that our proxy member variable corresponds to. Passes no
    /// `CreateChildArgs`.
    pub async fn create_dynamic_child(&self, coll: &str, name: &str) {
        self.create_dynamic_child_with_args(coll, name, fcomponent::CreateChildArgs::default())
            .await
            .expect("failed to create child")
    }

    /// Add a dynamic child to the given collection, with the given name to the
    /// component that our proxy member variable corresponds to.
    pub async fn create_dynamic_child_with_args(
        &self,
        coll: &str,
        name: &str,
        args: fcomponent::CreateChildArgs,
    ) -> Result<(), fcomponent::Error> {
        let collection_ref = fdecl::CollectionRef { name: coll.to_string() };
        let child_decl = ChildBuilder::new().name(name).build().native_into_fidl();
        let res = self
            .realm_proxy
            .as_ref()
            .expect("realm service not started")
            .create_child(&collection_ref, &child_decl, args)
            .await;
        res.expect("failed to create child")
    }

    pub async fn new_event_stream(&self, events: Vec<EventType>) -> fcomponent::EventStreamProxy {
        let builtin_environment_guard = self.builtin_environment.lock().await;
        new_event_stream(&*builtin_environment_guard, events).await
    }
}

/// Pulls events from the stream until the number of returned events is at least n.
pub async fn get_n_events(
    proxy: &fcomponent::EventStreamProxy,
    n: usize,
) -> Vec<fcomponent::Event> {
    let mut events = vec![];
    while events.len() < n {
        let mut next_events = proxy.get_next().await.unwrap();
        events.append(&mut next_events);
    }
    events
}

pub fn assert_event_type_and_moniker(
    event: &fcomponent::Event,
    event_type: fcomponent::EventType,
    moniker: impl std::string::ToString,
) {
    let fcomponent::Event {
        header:
            Some(fcomponent::EventHeader {
                event_type: Some(discovered_event_type),
                moniker: Some(discovered_moniker),
                ..
            }),
        ..
    } = event
    else {
        panic!("malformed event discovered: {:?}", event);
    };
    assert_eq!(&event_type, discovered_event_type);
    let moniker = moniker.to_string();
    assert_eq!(&moniker, discovered_moniker);
}

/// Create a new event stream for the provided environment.
pub async fn new_event_stream(
    builtin_environment: &BuiltinEnvironment,
    events: Vec<EventType>,
) -> fcomponent::EventStreamProxy {
    let mut event_source = builtin_environment.event_source_factory.create_for_above_root();
    let event_stream = event_source
        .subscribe(
            events
                .into_iter()
                .map(|event| EventSubscription {
                    event_name: UseEventStreamDecl {
                        source_name: event.into(),
                        source: UseSource::Parent,
                        scope: None,
                        target_path: "/svc/fuchsia.component.EventStream".parse().unwrap(),
                        filter: None,
                        availability: Availability::Required,
                    },
                })
                .collect(),
        )
        .await
        .expect("subscribe to event stream");
    let (proxy, fidl_stream) =
        fidl::endpoints::create_proxy_and_stream::<fcomponent::EventStreamMarker>();
    // It's obviously not ideal to detach this, but the need to create an extra task here goes away
    // in the next commit.
    fuchsia_async::Task::spawn(async move {
        let _event_source = event_source;
        crate::model::events::serve::serve_event_stream(event_stream, fidl_stream).await;
    })
    .detach();
    proxy
}

/// Create a test ConfigDecl and an associated ConfigValuesData and ConfigChecksum.
pub fn new_config_decl() -> (ConfigDecl, ConfigValuesData, ConfigChecksum) {
    let checksum = ConfigChecksum::Sha256([
        0x07, 0xA8, 0xE6, 0x85, 0xC8, 0x79, 0xA9, 0x79, 0xC3, 0x26, 0x17, 0xDC, 0x4E, 0x74, 0x65,
        0x7F, 0xF1, 0xF7, 0x73, 0xE7, 0x12, 0xEE, 0x51, 0xFD, 0xF6, 0x57, 0x43, 0x07, 0xA7, 0xAF,
        0x2E, 0x64,
    ]);
    let config = ConfigDecl {
        fields: vec![ConfigField {
            key: "my_field".to_string(),
            type_: ConfigValueType::Bool,
            mutability: Default::default(),
        }],
        checksum: checksum.clone(),
        value_source: ConfigValueSource::PackagePath("meta/root.cvf".into()),
    };
    let config_values = ConfigValuesData {
        values: vec![ConfigValueSpec { value: ConfigValue::Single(ConfigSingleValue::Bool(true)) }],
        checksum: checksum.clone(),
    };
    (config, config_values, checksum)
}

pub async fn lifecycle_controller(test: &TestModelResult) -> fsys::LifecycleControllerProxy {
    let host = {
        let env = test.builtin_environment.lock().await;
        env.lifecycle_controller.clone().unwrap()
    };
    let (proxy, server) = endpoints::create_proxy::<fsys::LifecycleControllerMarker>();
    capability::open_framework(&host, test.model.root(), server.into()).await.unwrap();
    proxy
}

pub async fn config_override(test: &TestModelResult) -> fsys::ConfigOverrideProxy {
    let host = {
        let env = test.builtin_environment.lock().await;
        env.config_override.clone().unwrap()
    };
    let (proxy, server) = fidl::endpoints::create_proxy::<fsys::ConfigOverrideMarker>();
    capability::open_framework(&host, test.model.root(), server.into()).await.unwrap();
    proxy
}
