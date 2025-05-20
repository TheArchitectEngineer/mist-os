// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_memory_attribution_plugin as fplugin;
use fuchsia_trace::duration;
use futures::AsyncWriteExt;
use traces::CATEGORY_MEMORY_CAPTURE;

use attribution_processing::AttributionData;

/// AttributionSnapshot holds and serves a snapshot of the memory of a Fuchsia system, to be sent
/// to a ffx command on a host.
pub struct AttributionSnapshot(fplugin::Snapshot);

impl AttributionSnapshot {
    pub fn new(
        attribution_data: AttributionData,
        kernel_statistics: fplugin::KernelStatistics,
        memory_stalls: zx::MemoryStall,
    ) -> AttributionSnapshot {
        AttributionSnapshot(fplugin::Snapshot {
            attributions: Some(
                attribution_data.attributions.into_iter().map(|a| a.into()).collect(),
            ),
            principals: Some(
                attribution_data.principals_vec.into_iter().map(|p| p.into()).collect(),
            ),
            resources: Some(attribution_data.resources_vec.into_iter().map(|r| r.into()).collect()),
            // TODO(b/411002259) change the protocol to avoid conversion.
            resource_names: Some(
                attribution_data.resource_names.iter().map(|r| r.to_string()).collect(),
            ),
            kernel_statistics: Some(kernel_statistics.into()),
            performance_metrics: Some(fplugin::PerformanceImpactMetrics {
                some_memory_stalls_ns: Some(memory_stalls.stall_time_some),
                full_memory_stalls_ns: Some(memory_stalls.stall_time_full),
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    pub async fn serve(self, socket: zx::Socket) {
        duration!(CATEGORY_MEMORY_CAPTURE, c"AttributionSnapshot::serve");
        let mut asocket = fidl::AsyncSocket::from_socket(socket);

        let data = {
            duration!(CATEGORY_MEMORY_CAPTURE, c"AttributionSnapshot::serve persist");
            fidl::persist(&self.0).unwrap()
        };
        asocket.write_all(&data).await.unwrap();
    }
}
