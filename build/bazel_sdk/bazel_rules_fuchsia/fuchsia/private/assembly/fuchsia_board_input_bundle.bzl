# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rules for defining assembly board input bundle."""

load("//fuchsia/constraints:target_compatibility.bzl", "COMPATIBILITY")
load("//fuchsia/private:fuchsia_package.bzl", "get_driver_component_manifests")
load("//fuchsia/private:providers.bzl", "FuchsiaPackageInfo")
load(":providers.bzl", "FuchsiaBoardInputBundleInfo")
load(":utils.bzl", "LOCAL_ONLY_ACTION_KWARGS", "select_root_dir_with_file")
load("//fuchsia/private:fuchsia_toolchains.bzl", "FUCHSIA_TOOLCHAIN_DEFINITION", "get_fuchsia_sdk_toolchain")

def _fuchsia_board_input_bundle_impl(ctx):
    sdk = get_fuchsia_sdk_toolchain(ctx)

    driver_entries = []
    creation_inputs = []
    build_id_dirs = []

    for dep in ctx.attr.base_driver_packages:
        driver_entries.append(
            {
                "package": dep[FuchsiaPackageInfo].package_manifest.path,
                "components": get_driver_component_manifests(dep),
                "set": "base",
            },
        )
        creation_inputs += dep[FuchsiaPackageInfo].files
        build_id_dirs += dep[FuchsiaPackageInfo].build_id_dirs
    for dep in ctx.attr.bootfs_driver_packages:
        driver_entries.append(
            {
                "package": dep[FuchsiaPackageInfo].package_manifest.path,
                "components": get_driver_component_manifests(dep),
                "set": "bootfs",
            },
        )
        creation_inputs += dep[FuchsiaPackageInfo].files
        build_id_dirs += dep[FuchsiaPackageInfo].build_id_dirs

    # Create driver list file
    driver_list = {"drivers": driver_entries}
    driver_list_file = ctx.actions.declare_file(ctx.label.name + "_driver.list")
    creation_inputs.append(driver_list_file)
    content = json.encode_indent(driver_list, indent = "  ")
    ctx.actions.write(driver_list_file, content)

    creation_args = ["--drivers", driver_list_file.path]

    if ctx.attr.version and ctx.file.version_file:
        fail("Only one of \"version\" or \"version_file\" can be set.")
        # TODO(https://fxbug.dev/397489730):
        # Make it required to have exactly one of these set
        # once these changes have rolled into all downstream repositories.

    if ctx.attr.version:
        creation_args.extend(["--version", ctx.attr.version])
    elif ctx.file.version_file:
        creation_args.extend(["--version-file", ctx.file.version_file.path])
        creation_inputs.append(ctx.file.version_file)

    # Add single-file configs
    for (arg, file) in [
        ("--energy-model-config", "energy_model_config"),
        ("--cpu-manager-config", "cpu_manager_config"),
        ("--power-manager-config", "power_manager_config"),
        ("--power-metrics-recorder-config", "power_metrics_recorder_config"),
        ("--system-power-mode-config", "system_power_mode_config"),
        ("--thermal-config", "thermal_config"),
    ]:
        config_file = getattr(ctx.file, file)
        if config_file:
            creation_inputs.append(config_file)
            creation_args.extend(
                [
                    arg,
                    config_file.path,
                ],
            )

    # Add multi-file configs
    for (arg, files) in [
        ("--thread-roles", "thread_roles"),
        ("--sysmem-format-costs-config", "sysmem_format_costs_config"),
    ]:
        config_files = getattr(ctx.files, files)
        if config_files:
            for config_file in config_files:
                creation_inputs.append(config_file)
                creation_args.extend([
                    arg,
                    config_file.path,
                ])

    # Add package entries
    for dep in ctx.attr.base_packages:
        creation_args.extend(
            [
                "--base-packages",
                dep[FuchsiaPackageInfo].package_manifest.path,
            ],
        )
        creation_inputs += dep[FuchsiaPackageInfo].files
        build_id_dirs += dep[FuchsiaPackageInfo].build_id_dirs

    for dep in ctx.attr.bootfs_packages:
        creation_args.extend(
            [
                "--bootfs-packages",
                dep[FuchsiaPackageInfo].package_manifest.path,
            ],
        )
        creation_inputs += dep[FuchsiaPackageInfo].files
        build_id_dirs += dep[FuchsiaPackageInfo].build_id_dirs

    # Create Board Input Bundle
    board_input_bundle_dir = ctx.actions.declare_directory(ctx.label.name)
    args = [
        "generate",
        "board-input-bundle",
        "--name",
        ctx.label.name,
        "--output",
        board_input_bundle_dir.path,
    ] + creation_args
    ctx.actions.run(
        executable = sdk.assembly_config,
        arguments = args,
        inputs = creation_inputs,
        outputs = [board_input_bundle_dir],
        progress_message = "Creating board input bundle for %s" % ctx.label.name,
        mnemonic = "CreateBIB",
        **LOCAL_ONLY_ACTION_KWARGS
    )

    return [
        DefaultInfo(
            files = depset([board_input_bundle_dir]),
        ),
        FuchsiaBoardInputBundleInfo(
            directory = board_input_bundle_dir.path,
            build_id_dirs = build_id_dirs,
        ),
        OutputGroupInfo(
            build_id_dirs = depset(transitive = build_id_dirs),
        ),
    ]

fuchsia_board_input_bundle = rule(
    doc = """Generates a board input bundle.""",
    implementation = _fuchsia_board_input_bundle_impl,
    toolchains = [FUCHSIA_TOOLCHAIN_DEFINITION],
    attrs = {
        "base_driver_packages": attr.label_list(
            doc = "Base-driver packages to include in board.",
            providers = [FuchsiaPackageInfo],
            default = [],
        ),
        "bootfs_driver_packages": attr.label_list(
            doc = "Bootfs-driver packages to include in board.",
            providers = [FuchsiaPackageInfo],
            default = [],
        ),
        "base_packages": attr.label_list(
            doc = "Base packages to include in board.",
            providers = [FuchsiaPackageInfo],
            default = [],
        ),
        "bootfs_packages": attr.label_list(
            doc = "Bootfs packages to include in board.",
            providers = [FuchsiaPackageInfo],
            default = [],
        ),
        "energy_model_config": attr.label(
            doc = "Path to energy model configuration",
            allow_single_file = True,
        ),
        "cpu_manager_config": attr.label(
            doc = "Path to cpu_manager configuration",
            allow_single_file = True,
        ),
        "power_manager_config": attr.label(
            doc = "Path to power_manager configuration",
            allow_single_file = True,
        ),
        "power_metrics_recorder_config": attr.label(
            doc = "Path to power_metrics_recorder configuration",
            allow_single_file = True,
        ),
        "system_power_mode_config": attr.label(
            doc = "Path to system power mode configuration",
            allow_single_file = True,
        ),
        "thermal_config": attr.label(
            doc = "Path to thermal configuration",
            allow_single_file = True,
        ),
        "thread_roles": attr.label_list(
            doc = "Path to thread role configuration files",
            default = [],
            allow_files = True,
        ),
        "sysmem_format_costs_config": attr.label_list(
            doc = "Path to sysmem format costs files",
            default = [],
            allow_files = True,
        ),
        "version": attr.string(
            doc = "Release version string",
        ),
        "version_file": attr.label(
            doc = "Path to a file containing the current release version.",
            allow_single_file = True,
        ),
    } | COMPATIBILITY.HOST_ATTRS,
)

def _fuchsia_prebuilt_board_input_bundle_impl(ctx):
    directory = select_root_dir_with_file(ctx.files.files, "board_input_bundle.json")
    return [
        DefaultInfo(files = depset(ctx.files.files)),
        FuchsiaBoardInputBundleInfo(
            directory = directory,
            build_id_dirs = [],
        ),
    ]

fuchsia_prebuilt_board_input_bundle = rule(
    doc = """Defines a Board Input Bundle based on preexisting BIB files.""",
    implementation = _fuchsia_prebuilt_board_input_bundle_impl,
    attrs = {
        "files": attr.label(
            doc = "All files belonging to the Board Input Bundles",
            mandatory = True,
        ),
    },
)
