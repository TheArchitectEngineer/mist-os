// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::ContainerStartInfo;
use anyhow::{anyhow, Context, Error};
use bstr::BString;
use starnix_container_structured_config::Config as ContainerStructuredConfig;
use starnix_core::device::android::bootloader_message_store::android_bootloader_message_store_init;
use starnix_core::device::remote_block_device::remote_block_device_init;
use starnix_core::mm::MlockPinFlavor;
use starnix_core::task::{CurrentTask, Kernel, KernelFeatures};
use starnix_core::vfs::FsString;
use starnix_features::Feature;
use starnix_logging::log_error;
use starnix_modules_ashmem::ashmem_device_init;
use starnix_modules_framebuffer::{AspectRatio, Framebuffer};
use starnix_modules_gpu::gpu_device_init;
use starnix_modules_gralloc::gralloc_device_init;
use starnix_modules_hvdcp_opti::hvdcp_opti_init;
use starnix_modules_input::uinput::register_uinput_device;
use starnix_modules_input::{
    EventProxyMode, InputDevice, InputEventsRelay, DEFAULT_KEYBOARD_DEVICE_ID,
    DEFAULT_MOUSE_DEVICE_ID, DEFAULT_TOUCH_DEVICE_ID,
};
use starnix_modules_kgsl::kgsl_device_init;
use starnix_modules_magma::magma_device_init;
use starnix_modules_nanohub::nanohub_device_init;
use starnix_modules_perfetto_consumer::start_perfetto_consumer_thread;
use starnix_modules_perfetto_producer::start_perfetto_producer_thread;
use starnix_modules_thermal::thermal_device_init;
use starnix_modules_touch_power_policy::TouchPowerPolicyDevice;
use starnix_sync::{Locked, Unlocked};
use starnix_uapi::error;
use starnix_uapi::errors::Errno;
use std::sync::mpsc::channel;
use std::sync::Arc;
use {
    fidl_fuchsia_sysinfo as fsysinfo, fidl_fuchsia_ui_composition as fuicomposition,
    fidl_fuchsia_ui_input3 as fuiinput, fidl_fuchsia_ui_policy as fuipolicy,
    fidl_fuchsia_ui_views as fuiviews,
};

/// A collection of parsed features, and their arguments.
#[derive(Default, Debug)]
pub struct Features {
    /// Features that must be available to the kernel after initialization.
    pub kernel: KernelFeatures,

    /// SELinux configuration.
    pub selinux: SELinuxFeature,

    /// Whether to enable ashmem.
    pub ashmem: bool,

    /// Whether to enable a framebuffer device.
    pub framebuffer: bool,

    /// Display aspect ratio.
    pub aspect_ratio: Option<AspectRatio>,

    /// This controls whether or not the default framebuffer background is black or colorful, to
    /// aid debugging.
    pub enable_visual_debugging: bool,

    /// Whether to enable gralloc.
    pub gralloc: bool,

    /// Whether to enable the Kernel Graphics Support Layer (kgsl) device used by Adreno GPUs.
    pub kgsl: bool,

    /// Supported magma vendor IDs.
    pub magma_supported_vendors: Option<Vec<u16>>,

    /// Whether to enable gfxstream.
    pub gfxstream: bool,

    /// Include the /container directory in the root file system.
    pub container: bool,

    /// Include the /test_data directory in the root file system.
    pub test_data: bool,

    /// Include the /custom_artifacts directory in the root file system.
    pub custom_artifacts: bool,

    /// Whether to provide android with a serial number.
    pub android_serialno: bool,

    /// Whether to enable Starnix's self-profiling. Results are visible in inspect.
    pub self_profile: bool,

    /// Optional perfetto configuration.
    pub perfetto: Option<FsString>,
    pub perfetto_producer: Option<FsString>,

    /// Whether to enable support for Android's Factory Data Reset.
    pub android_fdr: bool,

    /// Whether to allow the root filesystem to be read/write.
    pub rootfs_rw: bool,

    /// Whether to enable network manager and its filesystem.
    pub network_manager: bool,

    /// Whether to enable the nanohub module.
    pub nanohub: bool,

    pub enable_utc_time_adjustment: bool,

    pub thermal: Option<Vec<String>>,

    /// Whether to add android bootreason to kernel cmdline.
    pub android_bootreason: bool,

    pub hvdcp_opti: bool,
}

#[derive(Default, Debug, PartialEq)]
pub struct SELinuxFeature {
    /// True if SELinux should be enabled in the container.
    pub enabled: bool,

    /// Optional set of options to pass to the SELinux module.
    pub options: String,

    /// Optional set of access-check exceptions to pass to the SELinux module.
    pub exceptions: Vec<String>,
}

impl Features {
    pub fn record_inspect(&self, parent_node: &fuchsia_inspect::Node) {
        parent_node.record_child("features", |inspect_node| match self {
            Features {
                kernel:
                    KernelFeatures {
                        bpf_v2,
                        enable_suid,
                        io_uring,
                        error_on_failed_reboot,
                        default_uid,
                        default_seclabel,
                        selinux_test_suite,
                        default_ns_mount_options,
                        mlock_always_onfault,
                        mlock_pin_flavor,
                        netstack_mark,
                        rtnetlink_assume_ifb0_existence,
                    },
                selinux,
                ashmem,
                framebuffer,
                aspect_ratio,
                enable_visual_debugging,
                gralloc,
                kgsl,
                magma_supported_vendors,
                gfxstream,
                container,
                test_data,
                custom_artifacts,
                android_serialno,
                self_profile,
                perfetto,
                perfetto_producer,
                android_fdr,
                rootfs_rw,
                network_manager,
                nanohub,
                enable_utc_time_adjustment,
                thermal,
                android_bootreason,
                hvdcp_opti,
            } => {
                inspect_node.record_bool("selinux", selinux.enabled);
                inspect_node.record_bool("ashmem", *ashmem);
                inspect_node.record_bool("framebuffer", *framebuffer);
                inspect_node.record_bool("gralloc", *gralloc);
                inspect_node.record_bool("kgsl", *kgsl);
                inspect_node.record_string(
                    "magma_supported_vendors",
                    match magma_supported_vendors {
                        Some(vendors) => vendors
                            .iter()
                            .map(|vendor| format!("0x{:x}", vendor))
                            .collect::<Vec<String>>()
                            .join(","),
                        None => "".to_string(),
                    },
                );
                inspect_node.record_bool("gfxstream", *gfxstream);
                inspect_node.record_bool("container", *container);
                inspect_node.record_bool("test_data", *test_data);
                inspect_node.record_bool("custom_artifacts", *custom_artifacts);
                inspect_node.record_bool("android_serialno", *android_serialno);
                inspect_node.record_bool("self_profile", *self_profile);
                inspect_node.record_string(
                    "aspect_ratio",
                    aspect_ratio
                        .as_ref()
                        .map(|aspect_ratio| {
                            format!("width: {} height: {}", aspect_ratio.width, aspect_ratio.height)
                        })
                        .unwrap_or_default(),
                );
                inspect_node.record_string(
                    "perfetto",
                    perfetto.as_ref().map(|p| p.to_string()).unwrap_or_default(),
                );
                inspect_node.record_string(
                    "perfetto_producer",
                    perfetto_producer.as_ref().map(|p| p.to_string()).unwrap_or_default(),
                );
                inspect_node.record_bool("android_fdr", *android_fdr);
                inspect_node.record_bool("rootfs_rw", *rootfs_rw);
                inspect_node.record_bool("network_manager", *network_manager);
                inspect_node.record_bool("nanohub", *nanohub);
                inspect_node.record_string(
                    "thermal",
                    match thermal {
                        Some(devices) => devices.join(","),
                        None => "".to_string(),
                    },
                );
                inspect_node.record_bool("android_bootreason", *android_bootreason);
                inspect_node.record_bool("hvdcp_opti", *hvdcp_opti);

                inspect_node.record_child("kernel", |kernel_node| {
                    kernel_node.record_bool("bpf_v2", *bpf_v2);
                    kernel_node.record_bool("enable_suid", *enable_suid);
                    kernel_node.record_bool("io_uring", *io_uring);
                    kernel_node.record_bool("error_on_failed_reboot", *error_on_failed_reboot);
                    kernel_node.record_bool("enable_visual_debugging", *enable_visual_debugging);
                    kernel_node.record_int("default_uid", (*default_uid).into());
                    kernel_node.record_string(
                        "default_seclabel",
                        default_seclabel.as_deref().unwrap_or_default(),
                    );
                    kernel_node.record_bool("selinux_test_suite", *selinux_test_suite);
                    inspect_node.record_string(
                        "default_ns_mount_options",
                        format!("{:?}", default_ns_mount_options),
                    );
                    inspect_node
                        .record_bool("enable_utc_time_adjustment", *enable_utc_time_adjustment);
                    inspect_node.record_bool("mlock_always_onfault", *mlock_always_onfault);
                    inspect_node
                        .record_string("mlock_pin_flavor", format!("{:?}", mlock_pin_flavor));
                    inspect_node.record_bool("netstack_mark", *netstack_mark);
                    inspect_node.record_bool(
                        "rtnetlink_assume_ifb0_existence",
                        *rtnetlink_assume_ifb0_existence,
                    );
                });
            }
        });
    }
}

/// Parses all the featurse in `entries`.
///
/// Returns an error if parsing fails, or if an unsupported feature is present in `features`.
pub fn parse_features(
    start_info: &ContainerStartInfo,
    kernel_extra_features: &[String],
) -> Result<Features, Error> {
    let ContainerStructuredConfig {
        enable_utc_time_adjustment,
        extra_features,
        mlock_always_onfault,
        mlock_pin_flavor,
        selinux_exceptions,
        ui_visual_debugging_level,
    } = &start_info.config;

    let mut features = Features::default();
    for entry in start_info
        .program
        .features
        .iter()
        .chain(kernel_extra_features.iter())
        .chain(extra_features.iter())
    {
        let (feature, raw_args) = Feature::try_parse_feature_and_args(entry)?;
        match (feature, raw_args) {
            (Feature::AndroidFdr, _) => features.android_fdr = true,
            (Feature::AndroidSerialno, _) => features.android_serialno = true,
            (Feature::AndroidBootreason, _) => features.android_bootreason = true,
            (Feature::AspectRatio, Some(args)) => {
                let e = anyhow!("Invalid aspect_ratio: {:?}", args);
                let components: Vec<_> = args.split(':').collect();
                if components.len() != 2 {
                    return Err(e);
                }
                let width: u32 = components[0].parse().map_err(|_| anyhow!("Invalid aspect ratio width"))?;
                let height: u32 = components[1].parse().map_err(|_| anyhow!("Invalid aspect ratio height"))?;
                features.aspect_ratio = Some(AspectRatio { width, height });
            }
            (Feature::AspectRatio, None) => {
                return Err(anyhow!(
                    "Aspect ratio feature must contain the aspect ratio in the format: aspect_ratio:w:h"
                ))
            }
            (Feature::Container, _) => features.container = true,
            (Feature::CustomArtifacts, _) => features.custom_artifacts = true,
            (Feature::Ashmem, _) => features.ashmem = true,
            (Feature::Framebuffer, _) => features.framebuffer = true,
            (Feature::Gralloc, _) => features.gralloc = true,
            (Feature::Kgsl, _) => features.kgsl = true,
            (Feature::Magma, _) => if features.magma_supported_vendors.is_none() {
                const VENDOR_ARM: u16 = 0x13B5;
                const VENDOR_INTEL: u16 = 0x8086;
                features.magma_supported_vendors = Some(vec![VENDOR_ARM, VENDOR_INTEL])
            },
            (Feature::MagmaSupportedVendors, Some(arg)) => {
                features.magma_supported_vendors = Some(
                    arg.split(',')
                        .map(|s| {
                            let err = anyhow!("Feature format must be: magma_supported_vendors:0x1234[,0xabcd]");
                            let trimmed = s.trim_start_matches("0x");
                            u16::from_str_radix(trimmed, 16).map_err(|_| err)
                        }).collect::<Result<Vec<u16>, Error>>()?);
            },
            (Feature::MagmaSupportedVendors, None) => {
                return Err(anyhow!("Feature format must be: magma_supported_vendors:0x1234[,0xabcd]"))
            }
            (Feature::Nanohub, _) => features.nanohub = true,
            (Feature::NetstackMark, _) => features.kernel.netstack_mark = true,
            (Feature::NetworkManager, _) => features.network_manager = true,
            (Feature::Gfxstream, _) => features.gfxstream = true,
            (Feature::Bpf, Some(version)) => features.kernel.bpf_v2 = version == "v2",
            (Feature::Bpf, None) => return Err(anyhow!("bpf feature must have an argument (e.g. \"bpf:v2\")")),
            (Feature::EnableSuid, _) => features.kernel.enable_suid = true,
            (Feature::IoUring, _) => features.kernel.io_uring = true,
            (Feature::ErrorOnFailedReboot, _) => features.kernel.error_on_failed_reboot = true,
            (Feature::Perfetto, Some(socket_path)) => {
                features.perfetto = Some(socket_path.into());
            }
            (Feature::Perfetto, None) => {
                return Err(anyhow!("Perfetto feature must contain a socket path"));
            }
            (Feature::PerfettoProducer, Some(socket_path)) => {
                features.perfetto_producer = Some(socket_path.into());
            }
            (Feature::PerfettoProducer, None) => {
                return Err(anyhow!("Perfetto Producer feature must contain a socket path"));
            }
            (Feature::RootfsRw, _) => features.rootfs_rw = true,
            (Feature::RtnetlinkAssumeIfb0Existence, _) => {
                features.kernel.rtnetlink_assume_ifb0_existence = true;
            }
            (Feature::SelfProfile, _) => features.self_profile = true,
            (Feature::Selinux, arg) => {
                features.selinux = SELinuxFeature {
                    enabled: true,
                    options: arg.unwrap_or_default(),
                    exceptions: selinux_exceptions.clone(),
                };
            }
            (Feature::SelinuxTestSuite, _) => features.kernel.selinux_test_suite = true,
            (Feature::TestData, _) => features.test_data = true,
            (Feature::Thermal, Some(arg)) =>
                features.thermal = Some(
                    arg.split(',').map(String::from).collect::<Vec<String>>()),
            (Feature::Thermal, None) => {
                return Err(anyhow!("thermal feature must have an argument"))
            }
            (Feature::HvdcpOpti, _) => features.hvdcp_opti = true,
        };
    }

    if *ui_visual_debugging_level > 0 {
        features.enable_visual_debugging = true;
    }
    features.enable_utc_time_adjustment = *enable_utc_time_adjustment;

    features.kernel.default_uid = start_info.program.default_uid.0;
    features.kernel.default_seclabel = start_info.program.default_seclabel.clone();
    features.kernel.default_ns_mount_options =
        if let Some(mount_options) = &start_info.program.default_ns_mount_options {
            let options = mount_options
                .iter()
                .map(|item| {
                    let mut splitter = item.splitn(2, ":");
                    let key = splitter.next().expect("Failed to parse mount options");
                    let value = splitter.next().expect("Failed to parse mount options");
                    (key.to_string(), value.to_string())
                })
                .collect();
            Some(options)
        } else {
            None
        };

    features.kernel.mlock_always_onfault = *mlock_always_onfault;
    features.kernel.mlock_pin_flavor = MlockPinFlavor::parse(mlock_pin_flavor.as_str())?;

    Ok(features)
}

/// Runs all the features that are enabled in `system_task.kernel()`.
pub fn run_container_features(
    locked: &mut Locked<Unlocked>,
    system_task: &CurrentTask,
    features: &Features,
) -> Result<(), Error> {
    let kernel = system_task.kernel();

    let mut enabled_profiling = false;
    if features.framebuffer {
        let framebuffer = Framebuffer::device_init(
            locked,
            system_task,
            features.aspect_ratio,
            features.enable_visual_debugging,
        )
        .context("initializing framebuffer")?;

        let (touch_source_client, touch_source_server) = fidl::endpoints::create_endpoints();
        let (mouse_source_client, mouse_source_server) = fidl::endpoints::create_endpoints();
        let view_bound_protocols = fuicomposition::ViewBoundProtocols {
            touch_source: Some(touch_source_server),
            mouse_source: Some(mouse_source_server),
            ..Default::default()
        };
        let view_identity = fuiviews::ViewIdentityOnCreation::from(
            fuchsia_scenic::ViewRefPair::new().expect("Failed to create ViewRefPair"),
        );
        let view_ref = fuchsia_scenic::duplicate_view_ref(&view_identity.view_ref)
            .expect("Failed to dup view ref.");
        let keyboard =
            fuchsia_component::client::connect_to_protocol_sync::<fuiinput::KeyboardMarker>()
                .expect("Failed to connect to keyboard");
        let registry_proxy = fuchsia_component::client::connect_to_protocol_sync::<
            fuipolicy::DeviceListenerRegistryMarker,
        >()
        .expect("Failed to connect to device listener registry");

        // These need to be set before `Framebuffer::start_server` is called.
        // `Framebuffer::start_server` is only called when the `framebuffer` component feature is
        // enabled. The container is the runner for said components, and `run_container_features`
        // is performed before the Container is fully initialized. Therefore, it's safe to set
        // these values at this point.
        //
        // In the future, we would like to avoid initializing a framebuffer unconditionally on the
        // Kernel, at which point this logic will need to change.
        *framebuffer.view_identity.lock() = Some(view_identity);
        *framebuffer.view_bound_protocols.lock() = Some(view_bound_protocols);

        let framebuffer_info = framebuffer.info.read();

        let display_width = framebuffer_info.xres as i32;
        let display_height = framebuffer_info.yres as i32;

        let touch_device =
            InputDevice::new_touch(display_width, display_height, &kernel.inspect_node);
        let keyboard_device = InputDevice::new_keyboard(&kernel.inspect_node);
        let mouse_device = InputDevice::new_mouse(&kernel.inspect_node);

        touch_device.clone().register(
            locked,
            &kernel.kthreads.system_task(),
            DEFAULT_TOUCH_DEVICE_ID,
        );
        keyboard_device.clone().register(
            locked,
            &kernel.kthreads.system_task(),
            DEFAULT_KEYBOARD_DEVICE_ID,
        );
        mouse_device.clone().register(
            locked,
            &kernel.kthreads.system_task(),
            DEFAULT_MOUSE_DEVICE_ID,
        );

        let input_events_relay = InputEventsRelay::new();
        input_events_relay.start_relays(
            &kernel,
            EventProxyMode::WakeContainer,
            touch_source_client,
            keyboard,
            mouse_source_client,
            view_ref,
            registry_proxy,
            touch_device.open_files.clone(),
            keyboard_device.open_files.clone(),
            mouse_device.open_files.clone(),
            Some(touch_device.inspect_status.clone()),
            Some(keyboard_device.inspect_status.clone()),
            Some(mouse_device.inspect_status.clone()),
        );

        register_uinput_device(locked, &kernel.kthreads.system_task(), input_events_relay);

        // Channel we use to inform the relay of changes to `touch_standby`
        let (touch_standby_sender, touch_standby_receiver) = channel::<bool>();
        let touch_policy_device = TouchPowerPolicyDevice::new(touch_standby_sender);
        touch_policy_device.clone().register(locked, &kernel.kthreads.system_task());
        touch_policy_device.start_relay(&kernel, touch_standby_receiver);

        framebuffer.start_server(kernel, None);
    }
    if features.gralloc {
        // The virtgralloc0 device allows vulkan_selector to indicate to gralloc
        // whether swiftshader or magma will be used. This is separate from the
        // magma feature because the policy choice whether to use magma or
        // swiftshader is in vulkan_selector, and it can potentially choose
        // switfshader for testing purposes even when magma0 is present. Also,
        // it's nice to indicate swiftshader the same way regardless of whether
        // the magma feature is enabled or disabled. If a call to gralloc AIDL
        // IAllocator allocate2 occurs with this feature disabled, the call will
        // fail.
        gralloc_device_init(locked, system_task);
    }
    if features.kgsl {
        kgsl_device_init(locked, system_task);
    }
    if let Some(supported_vendors) = &features.magma_supported_vendors {
        magma_device_init(locked, system_task, supported_vendors.clone());
    }
    if features.gfxstream {
        gpu_device_init(locked, system_task);
    }
    if let Some(socket_path) = features.perfetto.clone() {
        start_perfetto_consumer_thread(kernel, socket_path)
            .context("Failed to start perfetto consumer thread")?;
    }
    if let Some(socket_path) = features.perfetto_producer.clone() {
        start_perfetto_producer_thread(kernel, socket_path)
    }
    if features.self_profile {
        enabled_profiling = true;
        fuchsia_inspect::component::inspector().root().record_lazy_child(
            "self_profile",
            fuchsia_inspect_contrib::ProfileDuration::lazy_node_callback,
        );
        fuchsia_inspect_contrib::start_self_profiling();
    }
    if features.ashmem {
        ashmem_device_init(locked, system_task);
    }
    if !enabled_profiling {
        fuchsia_inspect_contrib::stop_self_profiling();
    }
    if features.android_fdr {
        android_bootloader_message_store_init(locked, system_task);
        remote_block_device_init(locked, system_task);
    }
    if features.network_manager {
        if let Err(e) = kernel.network_manager.init(kernel) {
            log_error!("Network manager initialization failed: ({e:?})");
        }
    }
    if features.nanohub {
        nanohub_device_init(locked, system_task);
    }
    if let Some(devices) = &features.thermal {
        thermal_device_init(locked, system_task, devices.clone());
    }
    if features.hvdcp_opti {
        hvdcp_opti_init(locked, system_task);
    }

    Ok(())
}

/// Runs features requested by individual components inside the container.
pub fn run_component_features(
    kernel: &Arc<Kernel>,
    entries: &Vec<String>,
    mut incoming_dir: Option<fidl_fuchsia_io::DirectoryProxy>,
) -> Result<(), Errno> {
    for entry in entries {
        match entry.as_str() {
            "framebuffer" => {
                Framebuffer::get(kernel)?.start_server(kernel, incoming_dir.take());
            }
            feature => {
                return error!(ENOSYS, format!("Unsupported feature: {}", feature));
            }
        }
    }
    Ok(())
}

pub async fn get_serial_number() -> anyhow::Result<BString> {
    let sysinfo = fuchsia_component::client::connect_to_protocol::<fsysinfo::SysInfoMarker>()?;
    let serial = sysinfo.get_serial_number().await?.map_err(zx::Status::from_raw)?;
    Ok(BString::from(serial))
}
