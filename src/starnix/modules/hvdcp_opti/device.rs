// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::iio_file::{IioDirectory0, IioDirectory1};
use super::power_supply_file::{BatteryPowerSupply, BmsPowerSupply, UsbPowerSupply};
use super::qbg_battery_file::create_battery_profile_device;
use super::qbg_file::{create_qbg_device, QbgClassDirectory};
use super::utils::connect_to_device;
use starnix_core::device::kobject::DeviceMetadata;
use starnix_core::device::DeviceMode;
use starnix_core::fs::sysfs::DeviceDirectory;
use starnix_core::task::CurrentTask;
use starnix_logging::log_warn;
use starnix_sync::{FileOpsCore, LockBefore, Locked};
use starnix_uapi::device_type::DeviceType;

pub fn hvdcp_opti_init<L>(locked: &mut Locked<'_, L>, current_task: &CurrentTask)
where
    L: LockBefore<FileOpsCore>,
{
    if let Err(e) = connect_to_device() {
        // hvdcp_opti only supported on Sorrel. Let it fail.
        log_warn!(
            "Could not connect to hvdcp_opti server {}. This is expected on everything but Sorrel.",
            e
        );
        return;
    }

    let kernel = current_task.kernel();
    let registry = &kernel.device_registry;

    // /dev/qbg
    registry.register_device(
        locked,
        current_task,
        "qbg".into(),
        DeviceMetadata::new("qbg".into(), DeviceType::new(484, 0), DeviceMode::Char),
        registry.objects.get_or_create_class_with_ops(
            "qbg".into(),
            registry.objects.virtual_bus(),
            QbgClassDirectory::new,
        ),
        DeviceDirectory::new,
        create_qbg_device,
    );

    // /dev/qbg_battery
    registry.register_device(
        locked,
        current_task,
        "qbg_battery".into(),
        DeviceMetadata::new("qbg_battery".into(), DeviceType::new(485, 0), DeviceMode::Char),
        registry.objects.get_or_create_class("qbg_battery".into(), registry.objects.virtual_bus()),
        DeviceDirectory::new,
        create_battery_profile_device,
    );

    // /sys/bus/iio/devices/iio:device
    // IIO devices should not show up under /sys/class. This makes it show up under /sys/class,
    // but it's OK.
    let iio = registry
        .objects
        .get_or_create_class("iio".into(), registry.objects.get_or_create_bus("iio".into()));
    registry.add_numberless_device(
        locked,
        current_task,
        "iio:device0".into(),
        iio.clone(),
        IioDirectory0::new,
    );

    registry.add_numberless_device(
        locked,
        current_task,
        "iio:device1".into(),
        iio,
        IioDirectory1::new,
    );

    // power_supply devices don't show up under any bus. This makes it show up under virtual_bus,
    // but it's OK.
    let power_supply =
        registry.objects.get_or_create_class("power_supply".into(), registry.objects.virtual_bus());
    // /sys/class/power_supply/usb
    registry.add_numberless_device(
        locked,
        current_task,
        "usb".into(),
        power_supply.clone(),
        UsbPowerSupply::new,
    );

    // /sys/class/power_supply/battery
    registry.add_numberless_device(
        locked,
        current_task,
        "battery".into(),
        power_supply.clone(),
        BatteryPowerSupply::new,
    );

    // /sys/class/power_supply/bms
    registry.add_numberless_device(
        locked,
        current_task,
        "bms".into(),
        power_supply,
        BmsPowerSupply::new,
    );
}
