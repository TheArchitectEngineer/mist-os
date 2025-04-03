// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, Result};
use fidl_fuchsia_inspect::InspectSinkMarker;
use fidl_fuchsia_settings::PrivacyMarker;
use fidl_fuchsia_settings_test::*;
use fidl_fuchsia_stash::StoreMarker;
use fuchsia_component::client;
use fuchsia_component::server::ServiceFs;
use fuchsia_component_test::{
    Capability, ChildOptions, LocalComponentHandles, RealmBuilder, RealmInstance, Ref, Route,
};
use futures::{StreamExt, TryStreamExt};
use log::*;
use vfs::file::vmo::read_only;
use vfs::pseudo_directory;
use {fidl_fuchsia_component_sandbox as fsandbox, fidl_fuchsia_io as fio, fuchsia_async as fasync};

#[fuchsia::main]
async fn main() -> Result<(), Error> {
    let mut fs = ServiceFs::new();
    fs.dir("svc").add_fidl_service(|stream: RealmFactoryRequestStream| stream);
    fs.take_and_serve_directory_handle()?;
    fs.for_each_concurrent(0, serve_realm_factory).await;
    Ok(())
}

async fn serve_realm_factory(stream: RealmFactoryRequestStream) {
    if let Err(err) = handle_request_stream(stream).await {
        error!("{:?}", err);
    }
}

async fn handle_request_stream(mut stream: RealmFactoryRequestStream) -> Result<()> {
    let mut task_group = fasync::TaskGroup::new();
    let mut realms = vec![];
    let id_gen = sandbox::CapabilityIdGenerator::new();
    let store = client::connect_to_protocol::<fsandbox::CapabilityStoreMarker>().unwrap();
    while let Ok(Some(request)) = stream.try_next().await {
        match request {
            RealmFactoryRequest::CreateRealm { options, realm_server, responder } => {
                let realm = create_realm(options).await?;
                let request_stream = realm_server.into_stream();
                task_group.spawn(async move {
                    realm_proxy::service::serve(realm, request_stream).await.unwrap();
                });
                responder.send(Ok(()))?;
            }
            RealmFactoryRequest::CreateRealm2 { options, dictionary, responder } => {
                let realm = create_realm(options).await?;
                let dict_ref = realm.root.controller().get_exposed_dictionary().await?.unwrap();
                let dict_id = id_gen.next();
                store
                    .import(dict_id, fsandbox::Capability::Dictionary(dict_ref))
                    .await
                    .unwrap()
                    .unwrap();
                store.dictionary_legacy_export(dict_id, dictionary.into()).await.unwrap().unwrap();
                realms.push(realm);
                responder.send(Ok(()))?;
            }
            RealmFactoryRequest::_UnknownMethod { .. } => unreachable!(),
        }
    }

    task_group.join().await;
    Ok(())
}

async fn create_realm(_: RealmOptions) -> Result<RealmInstance, Error> {
    let builder = RealmBuilder::new().await?;
    let setui =
        builder.add_child("setui_service", "#meta/setui_service.cm", ChildOptions::new()).await?;
    let stash = builder.add_child("stash", "#meta/stash.cm", ChildOptions::new().eager()).await?;
    let config_data = builder
        .add_local_child(
            "config_data",
            move |handles| Box::pin(serve_config_data(handles)),
            ChildOptions::new(),
        )
        .await?;

    builder
        .add_route(
            Route::new()
                .capability(
                    Capability::directory("config-data")
                        .path("/config/data")
                        .rights(fio::R_STAR_DIR),
                )
                .from(&config_data)
                .to(&setui)
                .to(&stash),
        )
        .await?;

    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol::<InspectSinkMarker>())
                .capability(Capability::storage("data"))
                .from(Ref::parent())
                .to(&setui)
                .to(&stash),
        )
        .await?;

    builder
        .add_route(
            Route::new().capability(Capability::protocol::<StoreMarker>()).from(&stash).to(&setui),
        )
        .await?;

    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol::<PrivacyMarker>())
                .from(&setui)
                .to(Ref::parent()),
        )
        .await?;

    let realm = builder.build().await?;
    Ok(realm)
}

async fn serve_config_data(handles: LocalComponentHandles) -> Result<(), Error> {
    // TODO(b/307569251): Inject this data from the test.
    let config_data_dir = pseudo_directory! {
        "data" => pseudo_directory! {
            // TODO(311204355): Docs should exist to explain how to use these JSON files.
            "interface_configuration.json" => read_only(r#"{"interfaces": ["Privacy"]}"#),
            "service_flags.json" => read_only(r#"{"controller_flags": []}"#),
            "board_agent_configuration.json" => read_only(r#"{"agent_types": []}"#),
            "agent_configuration.json" => read_only(r#"{"agent_types": []}"#),
        },
    };
    let mut fs = ServiceFs::new();
    fs.add_remote("config", vfs::directory::serve(config_data_dir, fio::PERM_READABLE));
    fs.serve_connection(handles.outgoing_dir).expect("failed to serve config-data");
    fs.collect::<()>().await;
    Ok(())
}
