// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::run_command;
use crate::tests::utils::{
    inspect_accessor_data, make_inspect_with_length, make_inspects_for_lifecycle,
    setup_fake_archive_accessor, setup_fake_rcs, FakeAccessorData,
};
use errors::ResultExt as _;
use ffx_writer::{Format, MachineWriter, TestBuffers};
use fidl_fuchsia_diagnostics::{
    ClientSelectorConfiguration, DataType, SelectorArgument, StreamMode, StreamParameters,
};
use iquery::commands::SelectorsCommand;
use std::rc::Rc;

#[fuchsia::test]
async fn test_selectors_no_parameters() {
    let params = StreamParameters {
        stream_mode: Some(StreamMode::Snapshot),
        data_type: Some(DataType::Inspect),
        client_selector_configuration: Some(ClientSelectorConfiguration::SelectAll(true)),
        ..Default::default()
    };
    let expected_responses = Rc::new(vec![]);
    let test_buffers = TestBuffers::default();
    let mut writer = MachineWriter::new_test(Some(Format::Json), &test_buffers);
    let cmd = SelectorsCommand { data: vec![], selectors: vec![], accessor: None };
    assert!(run_command(
        setup_fake_rcs(vec![]),
        setup_fake_archive_accessor(vec![FakeAccessorData::new(
            params,
            expected_responses.clone(),
        )]),
        SelectorsCommand::from(cmd),
        &mut writer
    )
    .await
    .unwrap_err()
    .ffx_error()
    .is_some());
}

#[fuchsia::test]
async fn test_selectors_with_unknown_component_search() {
    let params = StreamParameters {
        stream_mode: Some(StreamMode::Snapshot),
        data_type: Some(DataType::Inspect),
        format: Some(fidl_fuchsia_diagnostics::Format::Json),
        client_selector_configuration: Some(ClientSelectorConfiguration::SelectAll(true)),
        ..Default::default()
    };
    let expected_responses = Rc::new(vec![]);
    let test_buffers = TestBuffers::default();
    let mut writer = MachineWriter::new_test(Some(Format::Json), &test_buffers);
    let cmd = SelectorsCommand {
        selectors: vec!["some-bad-moniker".to_string()],
        accessor: None,
        data: vec![],
    };
    assert!(run_command(
        setup_fake_rcs(vec![]),
        setup_fake_archive_accessor(vec![FakeAccessorData::new(
            params,
            expected_responses.clone(),
        )]),
        SelectorsCommand::from(cmd),
        &mut writer
    )
    .await
    .unwrap_err()
    .ffx_error()
    .is_some());
}

#[fuchsia::test]
async fn test_selectors_with_unknown_manifest() {
    let params = StreamParameters {
        stream_mode: Some(StreamMode::Snapshot),
        data_type: Some(DataType::Inspect),
        format: Some(fidl_fuchsia_diagnostics::Format::Json),
        client_selector_configuration: Some(ClientSelectorConfiguration::SelectAll(true)),
        ..Default::default()
    };
    let expected_responses = Rc::new(vec![]);
    let test_buffers = TestBuffers::default();
    let mut writer = MachineWriter::new_test(Some(Format::Json), &test_buffers);
    let cmd = SelectorsCommand {
        selectors: vec!["some-bad-moniker".to_string()],
        accessor: None,
        data: vec![],
    };
    assert!(run_command(
        setup_fake_rcs(vec![]),
        setup_fake_archive_accessor(vec![FakeAccessorData::new(
            params,
            expected_responses.clone(),
        )]),
        SelectorsCommand::from(cmd),
        &mut writer
    )
    .await
    .unwrap_err()
    .ffx_error()
    .is_some());
}

#[fuchsia::test]
async fn test_selectors_with_succesful_component_search() {
    let test_buffers = TestBuffers::default();
    let mut writer = MachineWriter::new_test(Some(Format::Json), &test_buffers);
    let cmd =
        SelectorsCommand { selectors: vec!["moniker1".to_string()], accessor: None, data: vec![] };
    let lifecycle_data = inspect_accessor_data(
        ClientSelectorConfiguration::SelectAll(true),
        make_inspects_for_lifecycle(),
    );
    let inspects = vec![
        make_inspect_with_length("test/moniker1", 1, 20),
        make_inspect_with_length("test/moniker1", 3, 10),
        make_inspect_with_length("test/moniker1", 6, 30),
    ];
    let inspect_data = inspect_accessor_data(
        ClientSelectorConfiguration::Selectors(vec![SelectorArgument::StructuredSelector(
            selectors::parse_verbose("test/moniker1:[...]root").unwrap(),
        )]),
        inspects,
    );
    run_command(
        setup_fake_rcs(vec!["test/moniker1"]),
        setup_fake_archive_accessor(vec![lifecycle_data, inspect_data]),
        SelectorsCommand::from(cmd),
        &mut writer,
    )
    .await
    .unwrap();

    let expected = serde_json::to_string(&vec![
        String::from(r#"test/moniker1:[name=fake-name]name:hello_1"#),
        String::from(r#"test/moniker1:[name=fake-name]name:hello_3"#),
        String::from(r#"test/moniker1:[name=fake-name]name:hello_6"#),
    ])
    .unwrap();
    let output = test_buffers.into_stdout_str();
    assert_eq!(output.trim_end(), expected);
}

#[fuchsia::test]
async fn test_selectors_with_manifest_that_exists() {
    let test_buffers = TestBuffers::default();
    let mut writer = MachineWriter::new_test(Some(Format::Json), &test_buffers);
    let cmd =
        SelectorsCommand { selectors: vec!["moniker1".to_string()], accessor: None, data: vec![] };
    let lifecycle_data = inspect_accessor_data(
        ClientSelectorConfiguration::SelectAll(true),
        make_inspects_for_lifecycle(),
    );
    let inspects = vec![
        make_inspect_with_length("test/moniker1", 1, 20),
        make_inspect_with_length("test/moniker1", 3, 10),
        make_inspect_with_length("test/moniker1", 6, 30),
    ];
    let inspect_data = inspect_accessor_data(
        ClientSelectorConfiguration::Selectors(vec![SelectorArgument::StructuredSelector(
            selectors::parse_verbose("test/moniker1:[...]root").unwrap(),
        )]),
        inspects,
    );
    run_command(
        setup_fake_rcs(vec!["test/moniker1"]),
        setup_fake_archive_accessor(vec![lifecycle_data, inspect_data]),
        SelectorsCommand::from(cmd),
        &mut writer,
    )
    .await
    .unwrap();

    let expected = serde_json::to_string(&vec![
        String::from(r#"test/moniker1:[name=fake-name]name:hello_1"#),
        String::from(r#"test/moniker1:[name=fake-name]name:hello_3"#),
        String::from(r#"test/moniker1:[name=fake-name]name:hello_6"#),
    ])
    .unwrap();
    let output = test_buffers.into_stdout_str();
    assert_eq!(output.trim_end(), expected);
}

#[fuchsia::test]
async fn test_selectors_with_selectors() {
    let test_buffers = TestBuffers::default();
    let mut writer = MachineWriter::new_test(Some(Format::Json), &test_buffers);
    let cmd = SelectorsCommand {
        data: vec![],
        selectors: vec![String::from("test/moniker1:name:hello_3")],
        accessor: None,
    };
    let lifecycle_data = inspect_accessor_data(
        ClientSelectorConfiguration::SelectAll(true),
        make_inspects_for_lifecycle(),
    );
    let inspects = vec![make_inspect_with_length("test/moniker1", 3, 10)];
    let inspect_data = inspect_accessor_data(
        ClientSelectorConfiguration::Selectors(vec![SelectorArgument::StructuredSelector(
            selectors::parse_verbose("test/moniker1:[...]name:hello_3").unwrap(),
        )]),
        inspects,
    );
    run_command(
        setup_fake_rcs(vec![]),
        setup_fake_archive_accessor(vec![lifecycle_data, inspect_data]),
        SelectorsCommand::from(cmd),
        &mut writer,
    )
    .await
    .unwrap();

    let expected =
        serde_json::to_string(&vec![String::from(r#"test/moniker1:[name=fake-name]name:hello_3"#)])
            .unwrap();
    let output = test_buffers.into_stdout_str();
    assert_eq!(output.trim_end(), expected);
}
