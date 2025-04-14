// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::atomic::{AtomicU16, Ordering};

use fidl::endpoints::DiscoverableProtocolMarker;
use fidl_fuchsia_update::ListenerMarker;
use fidl_test_persistence_factory::ControllerMarker;
use fuchsia_component_test::{
    Capability, ChildOptions, RealmBuilder, RealmBuilderParams, RealmInstance, Ref, Route,
};
use regex::Regex;
use std::sync::LazyLock;

const SINGLE_COUNTER_URL: &str = "#meta/single_counter_test_component.cm";
const PERSISTENCE_URL: &str = "#meta/persistence.cm";
pub static REALM_NAME_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"persistence-test-\d{5}").unwrap());

pub async fn create() -> RealmInstance {
    static COUNTER: AtomicU16 = AtomicU16::new(0);

    // We want deterministic realm names of a fixed length. Add a fixed-size
    // variable component so that realm names are unique across test cases.
    let realm_name = format!("persistence-test-{:05}", COUNTER.fetch_add(1, Ordering::Relaxed));
    assert!(
        REALM_NAME_PATTERN.is_match(&realm_name),
        "{} does not match {:?}",
        realm_name,
        *REALM_NAME_PATTERN
    );

    let builder = RealmBuilder::with_params(RealmBuilderParams::new().realm_name(realm_name))
        .await
        .expect("Failed to create realm builder");
    let single_counter = builder
        .add_child("single_counter", SINGLE_COUNTER_URL, ChildOptions::new())
        .await
        .expect("Failed to create single_counter");
    let persistence = builder
        .add_child("persistence", PERSISTENCE_URL, ChildOptions::new())
        .await
        .expect("Failed to create persistence");
    let config_server = crate::mock_filesystems::create_config_data(&builder)
        .await
        .expect("Failed to create filesystem from config");
    builder
        .add_route(
            Route::new()
                .capability(
                    Capability::directory("config-data")
                        .path("/config/data")
                        .rights(fidl_fuchsia_io::R_STAR_DIR),
                )
                .from(&config_server)
                .to(&persistence),
        )
        .await
        .expect("Failed to add route for /config/data directory");

    let cache_server = crate::mock_filesystems::create_cache_server(&builder)
        .await
        .expect("Failed to create cache server");
    builder
        .add_route(
            Route::new()
                .capability(
                    Capability::directory("cache")
                        .path("/cache")
                        .rights(fidl_fuchsia_io::RW_STAR_DIR),
                )
                .from(&cache_server)
                .to(&persistence),
        )
        .await
        .expect("Failed to add route for /cache directory");

    let update_server = crate::mock_fidl::handle_update_check_services(&builder)
        .await
        .expect("Failed to create update server");
    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol_by_name(ListenerMarker::PROTOCOL_NAME))
                .from(&update_server)
                .to(&persistence),
        )
        .await
        .expect("Failed to add route for fuchsia.update.Listener");
    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol_by_name(ControllerMarker::PROTOCOL_NAME))
                .from(&update_server)
                .to(Ref::parent()),
        )
        .await
        .expect("Failed to add route for fuchsia.update.Controller");

    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol_by_name(
                    "fuchsia.samplertestcontroller.SamplerTestController",
                ))
                .from(&single_counter)
                .to(Ref::parent()),
        )
        .await
        .expect("Failed to add route for fuchsia.samplertestcontroller.SamplerTestController");

    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol_by_name(crate::TEST_PERSISTENCE_SERVICE_NAME))
                .from(&persistence)
                .to(Ref::parent()),
        )
        .await
        .expect("Failed to add route for persistence");

    builder
        .add_route(
            Route::new()
                .capability(
                    Capability::protocol_by_name("fuchsia.diagnostics.ArchiveAccessor")
                        .as_("fuchsia.diagnostics.ArchiveAccessor.feedback"),
                )
                .from(Ref::parent())
                .to(&persistence),
        )
        .await
        .expect("Failed to add route for fuchsia.diagnostics.ArchiveAccessor");
    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol_by_name("fuchsia.logger.LogSink"))
                .from(Ref::parent())
                .to(&persistence)
                .to(&single_counter),
        )
        .await
        .expect("Failed to add route for fuchsia.logger.LogSink");

    builder
        .add_route(
            Route::new()
                .capability(
                    Capability::protocol_by_name("fuchsia.component.Binder")
                        .as_("fuchsia.component.PersistenceBinder"),
                )
                .from(&persistence)
                .to(Ref::parent()),
        )
        .await
        .expect("Failed to add route for fuchsia.component.Binder");

    builder.build().await.expect("Failed to build test realm")
}
