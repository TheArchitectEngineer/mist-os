// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use anyhow::{anyhow, Result};
use attribution_processing::AttributionDataProvider;
use fuchsia_inspect::{ArrayProperty, Inspector};
use futures::FutureExt;
use inspect_runtime::PublishedInspectController;
use log::debug;
use stalls::StallProvider;
use std::sync::Arc;
use {fidl_fuchsia_kernel as fkernel, fuchsia_inspect as _, inspect_runtime as _};

/// Hold the resource required to serve the inspect tree.
/// The FIDL service stops when this object is dropped.
pub struct ServiceTask {
    _inspect_controller: PublishedInspectController,
}

/// Begins to serve the inspect tree, and returns an object holding the server's resources.
/// Dropping the `ServiceTask` stops the service.
pub fn start_service(
    attribution_data_service: Arc<impl AttributionDataProvider>,
    kernel_stats_proxy: fkernel::StatsProxy,
    stall_provider: Arc<impl StallProvider>,
) -> Result<ServiceTask> {
    debug!("Start serving inspect tree.");

    // This creates the root of an Inspect tree
    // The Inspector is a singleton that you can access from any scope
    let inspector = fuchsia_inspect::component::inspector();

    // This serves the Inspect tree, converting failures into fatal errors
    let inspect_controller =
        inspect_runtime::publish(inspector, inspect_runtime::PublishOptions::default())
            .ok_or_else(|| anyhow!("Failed to serve server handling `fuchsia.inspect.Tree`"))?;

    build_inspect_tree(attribution_data_service, kernel_stats_proxy, stall_provider, inspector);

    Ok(ServiceTask { _inspect_controller: inspect_controller })
}

fn build_inspect_tree(
    attribution_data_service: Arc<impl AttributionDataProvider>,
    kernel_stats_proxy: fkernel::StatsProxy,
    stall_provider: Arc<impl StallProvider>,
    inspector: &Inspector,
) {
    // Lazy evaluation is unregistered when the `LazyNode` is dropped.
    {
        let kernel_stats_proxy = kernel_stats_proxy.clone();
        inspector.root().record_lazy_child("kmem_stats", move || {
            let kernel_stats_proxy = kernel_stats_proxy.clone();
            async move {
                let inspector = Inspector::default();
                let root = inspector.root();
                let mem_stats = kernel_stats_proxy.get_memory_stats().await?;
                mem_stats.total_bytes.map(|v| root.record_uint("total_bytes", v));
                mem_stats.free_bytes.map(|v| root.record_uint("free_bytes", v));
                mem_stats.free_loaned_bytes.map(|v| root.record_uint("free_loaned_bytes", v));
                mem_stats.wired_bytes.map(|v| root.record_uint("wired_bytes", v));
                mem_stats.total_heap_bytes.map(|v| root.record_uint("total_heap_bytes", v));
                mem_stats.free_heap_bytes.map(|v| root.record_uint("free_heap_bytes", v));
                mem_stats.vmo_bytes.map(|v| root.record_uint("vmo_bytes", v));
                mem_stats.mmu_overhead_bytes.map(|v| root.record_uint("mmu_overhead_bytes", v));
                mem_stats.ipc_bytes.map(|v| root.record_uint("ipc_bytes", v));
                mem_stats.cache_bytes.map(|v| root.record_uint("cache_bytes", v));
                mem_stats.slab_bytes.map(|v| root.record_uint("slab_bytes", v));
                mem_stats.zram_bytes.map(|v| root.record_uint("zram_bytes", v));
                mem_stats.other_bytes.map(|v| root.record_uint("other_bytes", v));
                mem_stats
                    .vmo_reclaim_total_bytes
                    .map(|v| root.record_uint("vmo_reclaim_total_bytes", v));
                mem_stats
                    .vmo_reclaim_newest_bytes
                    .map(|v| root.record_uint("vmo_reclaim_newest_bytes", v));
                mem_stats
                    .vmo_reclaim_oldest_bytes
                    .map(|v| root.record_uint("vmo_reclaim_oldest_bytes", v));
                mem_stats
                    .vmo_reclaim_disabled_bytes
                    .map(|v| root.record_uint("vmo_reclaim_disabled_bytes", v));
                mem_stats
                    .vmo_discardable_locked_bytes
                    .map(|v| root.record_uint("vmo_discardable_locked_bytes", v));
                mem_stats
                    .vmo_discardable_unlocked_bytes
                    .map(|v| root.record_uint("vmo_discardable_unlocked_bytes", v));
                Ok(inspector)
            }
            .boxed()
        })
    };

    {
        inspector.root().record_lazy_child("kmem_stats_compression", move || {
            let kernel_stats_proxy = kernel_stats_proxy.clone();
            async move {
                let inspector = Inspector::default();
                let cmp_stats = kernel_stats_proxy.get_memory_stats_compression().await?;
                cmp_stats
                    .uncompressed_storage_bytes
                    .map(|v| inspector.root().record_uint("uncompressed_storage_bytes", v));
                cmp_stats
                    .compressed_storage_bytes
                    .map(|v| inspector.root().record_uint("compressed_storage_bytes", v));
                cmp_stats
                    .compressed_fragmentation_bytes
                    .map(|v| inspector.root().record_uint("compressed_fragmentation_bytes", v));
                cmp_stats
                    .compression_time
                    .map(|v| inspector.root().record_int("compression_time", v));
                cmp_stats
                    .decompression_time
                    .map(|v| inspector.root().record_int("decompression_time", v));
                cmp_stats
                    .total_page_compression_attempts
                    .map(|v| inspector.root().record_uint("total_page_compression_attempts", v));
                cmp_stats
                    .failed_page_compression_attempts
                    .map(|v| inspector.root().record_uint("failed_page_compression_attempts", v));
                cmp_stats
                    .total_page_decompressions
                    .map(|v| inspector.root().record_uint("total_page_decompressions", v));
                cmp_stats
                    .compressed_page_evictions
                    .map(|v| inspector.root().record_uint("compressed_page_evictions", v));
                cmp_stats
                    .eager_page_compressions
                    .map(|v| inspector.root().record_uint("eager_page_compressions", v));
                cmp_stats
                    .memory_pressure_page_compressions
                    .map(|v| inspector.root().record_uint("memory_pressure_page_compressions", v));
                cmp_stats
                    .critical_memory_page_compressions
                    .map(|v| inspector.root().record_uint("critical_memory_page_compressions", v));
                cmp_stats
                    .pages_decompressed_unit_ns
                    .map(|v| inspector.root().record_uint("pages_decompressed_unit_ns", v));
                cmp_stats.pages_decompressed_within_log_time.map(|v| {
                    let array =
                        inspector.root().create_uint_array("pages_decompressed_within_log_time", 8);
                    // Using constant strings saves allocations.
                    array.set(0, v[0]);
                    array.set(1, v[1]);
                    array.set(2, v[2]);
                    array.set(3, v[3]);
                    array.set(4, v[4]);
                    array.set(5, v[5]);
                    array.set(6, v[6]);
                    array.set(7, v[7]);
                    inspector.root().record(array);
                });
                Ok(inspector)
            }
            .boxed()
        });
    }

    {
        inspector.root().record_lazy_child("current", move || {
            let attribution_data_service = attribution_data_service.clone();
            async move {
                let inspector = Inspector::default();
                let current_attribution_data =
                    attribution_data_service.get_attribution_data().await?;
                let summary =
                    attribution_processing::attribute_vmos(current_attribution_data).summary();

                summary.principals.into_iter().for_each(|p| {
                    let node = inspector.root().create_child(p.name);
                    node.record_uint("committed_private", p.committed_private);
                    node.record_double("committed_scaled", p.committed_scaled);
                    node.record_uint("committed_total", p.committed_total);
                    node.record_uint("populated_private", p.populated_private);
                    node.record_double("populated_scaled", p.populated_scaled);
                    node.record_uint("populated_total", p.populated_total);
                    inspector.root().record(node);
                });
                Ok(inspector)
            }
            .boxed()
        });
    }

    {
        inspector.root().record_lazy_child("stalls", move || {
            let stall_info = stall_provider.get_stall_info().unwrap();
            let stall_rate_opt = stall_provider.get_stall_rate();
            async move {
                let inspector = Inspector::default();
                inspector.root().record_int("current_some", stall_info.stall_time_some);
                inspector.root().record_int("current_full", stall_info.stall_time_full);
                if let Some(stall_rate) = stall_rate_opt {
                    inspector
                        .root()
                        .record_int("rate_interval_s", stall_rate.interval.into_seconds());
                    inspector.root().record_int("rate_some", stall_rate.rate_some);
                    inspector.root().record_int("rate_full", stall_rate.rate_full);
                }
                Ok(inspector)
            }
            .boxed()
        });
    }
}

#[cfg(test)]
mod tests {
    use attribution_processing::{
        Attribution, AttributionData, Principal, PrincipalDescription, PrincipalIdentifier,
        PrincipalType, Resource, ResourceReference,
    };
    use diagnostics_assertions::assert_data_tree;
    use futures::future::BoxFuture;
    use futures::TryStreamExt;
    use {fidl_fuchsia_memory_attribution_plugin as fplugin, fuchsia_async as fasync};

    use super::*;

    struct FakeAttributionDataProvider {}

    impl AttributionDataProvider for FakeAttributionDataProvider {
        fn get_attribution_data(&self) -> BoxFuture<'_, Result<AttributionData, anyhow::Error>> {
            async {
                Ok(AttributionData {
                    principals_vec: vec![Principal {
                        identifier: PrincipalIdentifier(1),
                        description: PrincipalDescription::Component("principal".to_owned()),
                        principal_type: PrincipalType::Runnable,
                        parent: None,
                    }],
                    resources_vec: vec![Resource {
                        koid: 10,
                        name_index: 0,
                        resource_type: fplugin::ResourceType::Vmo(fplugin::Vmo {
                            parent: None,
                            private_committed_bytes: Some(1024),
                            private_populated_bytes: Some(2048),
                            scaled_committed_bytes: Some(1024),
                            scaled_populated_bytes: Some(2048),
                            total_committed_bytes: Some(1024),
                            total_populated_bytes: Some(2048),
                            ..Default::default()
                        }),
                    }],
                    resource_names: vec!["resource".to_owned()],
                    attributions: vec![Attribution {
                        source: PrincipalIdentifier(1),
                        subject: PrincipalIdentifier(1),
                        resources: vec![ResourceReference::KernelObject(10)],
                    }],
                })
            }
            .boxed()
        }
    }

    async fn serve_kernel_stats(
        mut request_stream: fkernel::StatsRequestStream,
    ) -> Result<(), fidl::Error> {
        while let Some(request) = request_stream.try_next().await? {
            match request {
                fkernel::StatsRequest::GetMemoryStats { responder } => {
                    responder
                        .send(&fkernel::MemoryStats {
                            total_bytes: Some(1),
                            free_bytes: Some(2),
                            wired_bytes: Some(3),
                            total_heap_bytes: Some(4),
                            free_heap_bytes: Some(5),
                            vmo_bytes: Some(6),
                            mmu_overhead_bytes: Some(7),
                            ipc_bytes: Some(8),
                            other_bytes: Some(9),
                            free_loaned_bytes: Some(10),
                            cache_bytes: Some(11),
                            slab_bytes: Some(12),
                            zram_bytes: Some(13),
                            vmo_reclaim_total_bytes: Some(14),
                            vmo_reclaim_newest_bytes: Some(15),
                            vmo_reclaim_oldest_bytes: Some(16),
                            vmo_reclaim_disabled_bytes: Some(17),
                            vmo_discardable_locked_bytes: Some(18),
                            vmo_discardable_unlocked_bytes: Some(19),
                            ..Default::default()
                        })
                        .unwrap();
                }
                fkernel::StatsRequest::GetMemoryStatsExtended { responder: _ } => {
                    unimplemented!("Deprecated call, should not be used")
                }
                fkernel::StatsRequest::GetMemoryStatsCompression { responder } => {
                    responder
                        .send(&fkernel::MemoryStatsCompression {
                            uncompressed_storage_bytes: Some(20),
                            compressed_storage_bytes: Some(21),
                            compressed_fragmentation_bytes: Some(22),
                            compression_time: Some(23),
                            decompression_time: Some(24),
                            total_page_compression_attempts: Some(25),
                            failed_page_compression_attempts: Some(26),
                            total_page_decompressions: Some(27),
                            compressed_page_evictions: Some(28),
                            eager_page_compressions: Some(29),
                            memory_pressure_page_compressions: Some(30),
                            critical_memory_page_compressions: Some(31),
                            pages_decompressed_unit_ns: Some(32),
                            pages_decompressed_within_log_time: Some([
                                40, 41, 42, 43, 44, 45, 46, 47,
                            ]),
                            ..Default::default()
                        })
                        .unwrap();
                }
                fkernel::StatsRequest::GetCpuStats { responder: _ } => unimplemented!(),
                fkernel::StatsRequest::GetCpuLoad { duration: _, responder: _ } => unimplemented!(),
            }
        }
        Ok(())
    }

    #[test]
    fn test_build_inspect_tree() {
        let mut exec = fasync::TestExecutor::new();

        let data_provider = Arc::new(FakeAttributionDataProvider {});

        let (stats_provider, stats_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<fkernel::StatsMarker>();

        fasync::Task::spawn(async move {
            serve_kernel_stats(stats_request_stream).await.unwrap();
        })
        .detach();

        let inspector = fuchsia_inspect::Inspector::default();

        struct FakeStallProvider {}
        impl StallProvider for FakeStallProvider {
            fn get_stall_info(&self) -> Result<zx::MemoryStall, anyhow::Error> {
                Ok(zx::MemoryStall { stall_time_some: 10, stall_time_full: 20 })
            }

            fn get_stall_rate(&self) -> Option<stalls::MemoryStallRate> {
                Some(stalls::MemoryStallRate {
                    interval: fasync::MonotonicDuration::from_seconds(60),
                    rate_some: 1,
                    rate_full: 2,
                })
            }
        }

        build_inspect_tree(
            data_provider,
            stats_provider,
            Arc::new(FakeStallProvider {}),
            &inspector,
        );

        let output = exec
            .run_singlethreaded(fuchsia_inspect::reader::read(&inspector))
            .expect("got hierarchy");

        assert_data_tree!(output, root: {
                    current: {
                        principal: {
        committed_private: 1024u64,
        committed_scaled: 1024.0,
        committed_total: 1024u64,
        populated_private: 2048u64,
        populated_scaled: 2048.0,
        populated_total: 2048u64
                        }
                    },
                    kmem_stats: {
                        total_bytes: 1u64,
                        free_bytes: 2u64,
                        wired_bytes: 3u64,
                        total_heap_bytes: 4u64,
                        free_heap_bytes: 5u64,
                        vmo_bytes: 6u64,
                        mmu_overhead_bytes: 7u64,
                        ipc_bytes: 8u64,
                        other_bytes: 9u64,
                        free_loaned_bytes: 10u64,
                        cache_bytes: 11u64,
                        slab_bytes: 12u64,
                        zram_bytes: 13u64,
                        vmo_reclaim_total_bytes: 14u64,
                        vmo_reclaim_newest_bytes: 15u64,
                        vmo_reclaim_oldest_bytes: 16u64,
                        vmo_reclaim_disabled_bytes: 17u64,
                        vmo_discardable_locked_bytes: 18u64,
                        vmo_discardable_unlocked_bytes: 19u64
                    },
                    kmem_stats_compression: {
                        uncompressed_storage_bytes: 20u64,
                        compressed_storage_bytes: 21u64,
                        compressed_fragmentation_bytes: 22u64,
                        compression_time: 23i64,
                        decompression_time: 24i64,
                        total_page_compression_attempts: 25u64,
                        failed_page_compression_attempts: 26u64,
                        total_page_decompressions: 27u64,
                        compressed_page_evictions: 28u64,
                        eager_page_compressions: 29u64,
                        memory_pressure_page_compressions: 30u64,
                        critical_memory_page_compressions: 31u64,
                        pages_decompressed_unit_ns: 32u64,
                        pages_decompressed_within_log_time: vec![
                            40u64, 41u64, 42u64, 43u64, 44u64, 45u64, 46u64, 47u64,
                        ]
                    },
                    stalls: {
                        current_some: 10i64,
                        current_full: 20i64,
                        rate_some: 1i64,
                        rate_full: 2i64,
                        rate_interval_s: 60i64
                    }
                });
    }
}
