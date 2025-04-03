// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use anyhow::{anyhow, Error};
use block_protocol::{BlockFifoRequest, BlockFifoResponse};
use fidl_fuchsia_hardware_block_driver::{BlockIoFlag, BlockOpcode};
use fuchsia_sync::Mutex;
use futures::{Future, FutureExt as _, TryStreamExt as _};
use std::borrow::Cow;
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::num::NonZero;
use std::ops::Range;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use zx::HandleBased;
use {
    fidl_fuchsia_hardware_block as fblock, fidl_fuchsia_hardware_block_partition as fpartition,
    fidl_fuchsia_hardware_block_volume as fvolume, fuchsia_async as fasync,
};

pub mod async_interface;
pub mod c_interface;

#[derive(Clone)]
pub enum DeviceInfo {
    Block(BlockInfo),
    Partition(PartitionInfo),
}

/// Information associated with non-partition block devices.
#[derive(Clone)]
pub struct BlockInfo {
    pub device_flags: fblock::Flag,
    pub block_count: u64,
}

/// Information associated with a block device that is also a partition.
#[derive(Clone)]
pub struct PartitionInfo {
    /// The device flags reported by the underlying device.
    pub device_flags: fblock::Flag,
    /// If `block_range` is None, the partition is a volume and may not be contiguous.
    /// In this case, the server will use the `get_volume_info` method to get the count of assigned
    /// slices and use that (along with the slice and block sizes) to determine the block count.
    pub block_range: Option<Range<u64>>,
    pub type_guid: [u8; 16],
    pub instance_guid: [u8; 16],
    pub name: String,
    pub flags: u64,
}

// Multiple Block I/O request may be sent as a group.
// Notes:
// - the group is identified by the group id in the request
// - if using groups, a response will not be sent unless `BlockIoFlag::GROUP_LAST`
//   flag is set.
// - when processing a request of a group fails, subsequent requests of that
//   group will not be processed.
//
// Refer to sdk/fidl/fuchsia.hardware.block.driver/block.fidl for details.
//
// FifoMessageGroup keeps track of the relevant BlockFifoResponse field for
// a group requests. Only `status` and `count` needs to be updated.
struct FifoMessageGroup {
    status: zx::Status,
    count: u32,
    req_id: Option<u32>,
}

impl FifoMessageGroup {
    fn new() -> Self {
        FifoMessageGroup { status: zx::Status::OK, count: 0, req_id: None }
    }
}

#[derive(Default)]
struct FifoMessageGroups(Mutex<BTreeMap<u16, FifoMessageGroup>>);

// Keeps track of all the group requests that are currently being processed
impl FifoMessageGroups {
    /// Completes a request and returns a response to be sent if it's the last outstanding request
    /// for this group.
    fn complete(&self, group_id: u16, status: zx::Status) -> Option<BlockFifoResponse> {
        let mut map = self.0.lock();
        let Entry::Occupied(mut o) = map.entry(group_id) else { unreachable!() };
        let group = o.get_mut();
        if group.count == 1 {
            if let Some(reqid) = group.req_id {
                let status =
                    if group.status != zx::Status::OK { group.status } else { status }.into_raw();

                o.remove();

                return Some(BlockFifoResponse {
                    status,
                    reqid,
                    group: group_id,
                    ..Default::default()
                });
            }
        }

        group.count = group.count.checked_sub(1).unwrap();
        if status != zx::Status::OK && group.status == zx::Status::OK {
            group.status = status
        }
        None
    }
}

/// BlockServer is an implementation of fuchsia.hardware.block.partition.Partition.
/// cbindgen:no-export
pub struct BlockServer<SM> {
    block_size: u32,
    session_manager: Arc<SM>,
}

/// A single entry in `[OffsetMap]`.
pub struct BlockOffsetMapping {
    source_block_offset: u64,
    target_block_offset: u64,
    length: u64,
}

impl std::convert::TryFrom<fblock::BlockOffsetMapping> for BlockOffsetMapping {
    type Error = zx::Status;

    fn try_from(wire: fblock::BlockOffsetMapping) -> Result<Self, Self::Error> {
        wire.source_block_offset.checked_add(wire.length).ok_or(zx::Status::INVALID_ARGS)?;
        wire.target_block_offset.checked_add(wire.length).ok_or(zx::Status::INVALID_ARGS)?;
        Ok(Self {
            source_block_offset: wire.source_block_offset,
            target_block_offset: wire.target_block_offset,
            length: wire.length,
        })
    }
}

/// Remaps the offset of block requests based on an internal map.
/// TODO(https://fxbug.dev/402515764): For now, this just supports a single entry in the map, which
/// is all that is required for GPT partitions.  If we want to support this for FVM, we will need
/// to support multiple entries, which requires changing the block server to support request
/// splitting.
pub struct OffsetMap {
    mapping: BlockOffsetMapping,
}

impl OffsetMap {
    pub fn new(mapping: BlockOffsetMapping) -> Self {
        Self { mapping }
    }

    /// Adjusts the requested range, returning the new dev_offset.
    /// Returns false if the request would exceed the range known to OffsetMap.
    pub fn adjust_request(&self, dev_offset: u64, length: u64) -> Option<u64> {
        if length == 0 {
            return None;
        }
        let end = dev_offset.checked_add(length)?;
        if self.mapping.source_block_offset > dev_offset
            || end > self.mapping.source_block_offset + self.mapping.length
        {
            return None;
        }
        let delta = dev_offset - self.mapping.source_block_offset;
        Some(self.mapping.target_block_offset + delta)
    }
}

// Methods take Arc<Self> rather than &self because of
// https://github.com/rust-lang/rust/issues/42940.
pub trait SessionManager: 'static {
    fn on_attach_vmo(
        self: Arc<Self>,
        vmo: &Arc<zx::Vmo>,
    ) -> impl Future<Output = Result<(), zx::Status>> + Send;

    /// Creates a new session to handle `stream`.
    /// The returned future should run until the session completes, for example when the client end
    /// closes.
    /// If `offset_map` is set, it will be used to remap the dev_offset of FIFO requests.
    fn open_session(
        self: Arc<Self>,
        stream: fblock::SessionRequestStream,
        offset_map: Option<OffsetMap>,
        block_size: u32,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Called to get block/partition information for Block::GetInfo, Partition::GetTypeGuid, etc.
    fn get_info(&self) -> impl Future<Output = Result<Cow<'_, DeviceInfo>, zx::Status>> + Send;

    /// Called to handle the GetVolumeInfo FIDL call.
    fn get_volume_info(
        &self,
    ) -> impl Future<Output = Result<(fvolume::VolumeManagerInfo, fvolume::VolumeInfo), zx::Status>> + Send
    {
        async { Err(zx::Status::NOT_SUPPORTED) }
    }

    /// Called to handle the QuerySlices FIDL call.
    fn query_slices(
        &self,
        _start_slices: &[u64],
    ) -> impl Future<Output = Result<Vec<fvolume::VsliceRange>, zx::Status>> + Send {
        async { Err(zx::Status::NOT_SUPPORTED) }
    }

    /// Called to handle the Shrink FIDL call.
    fn extend(
        &self,
        _start_slice: u64,
        _slice_count: u64,
    ) -> impl Future<Output = Result<(), zx::Status>> + Send {
        async { Err(zx::Status::NOT_SUPPORTED) }
    }

    /// Called to handle the Shrink FIDL call.
    fn shrink(
        &self,
        _start_slice: u64,
        _slice_count: u64,
    ) -> impl Future<Output = Result<(), zx::Status>> + Send {
        async { Err(zx::Status::NOT_SUPPORTED) }
    }
}

pub trait IntoSessionManager {
    type SM: SessionManager;

    fn into_session_manager(self) -> Arc<Self::SM>;
}

impl<SM: SessionManager> BlockServer<SM> {
    pub fn new(block_size: u32, session_manager: impl IntoSessionManager<SM = SM>) -> Self {
        Self { block_size, session_manager: session_manager.into_session_manager() }
    }

    /// Called to process requests for fuchsia.hardware.block.volume/Volume.
    pub async fn handle_requests(
        &self,
        mut requests: fvolume::VolumeRequestStream,
    ) -> Result<(), Error> {
        let scope = fasync::Scope::new();
        while let Some(request) = requests.try_next().await.unwrap() {
            if let Some(session) = self.handle_request(request).await? {
                scope.spawn(session.map(|_| ()));
            }
        }
        scope.await;
        Ok(())
    }

    /// Processes a partition request.  If a new session task is created in response to the request,
    /// it is returned.
    async fn handle_request(
        &self,
        request: fvolume::VolumeRequest,
    ) -> Result<Option<impl Future<Output = Result<(), Error>> + Send>, Error> {
        match request {
            fvolume::VolumeRequest::GetInfo { responder } => match self.device_info().await {
                Ok(info) => {
                    let (block_count, flags) = match info.as_ref() {
                        DeviceInfo::Block(BlockInfo { block_count, device_flags }) => {
                            (*block_count, *device_flags)
                        }
                        DeviceInfo::Partition(partition_info) => {
                            let block_count = if let Some(range) =
                                partition_info.block_range.as_ref()
                            {
                                range.end - range.start
                            } else {
                                let volume_info = self.session_manager.get_volume_info().await?;
                                volume_info.0.slice_size * volume_info.1.partition_slice_count
                                    / self.block_size as u64
                            };
                            (block_count, partition_info.device_flags)
                        }
                    };
                    responder.send(Ok(&fblock::BlockInfo {
                        block_count,
                        block_size: self.block_size,
                        max_transfer_size: fblock::MAX_TRANSFER_UNBOUNDED,
                        flags,
                    }))?;
                }
                Err(status) => responder.send(Err(status.into_raw()))?,
            },
            fvolume::VolumeRequest::GetStats { clear: _, responder } => {
                // TODO(https://fxbug.dev/348077960): Implement this
                responder.send(Err(zx::Status::NOT_SUPPORTED.into_raw()))?;
            }
            fvolume::VolumeRequest::OpenSession { session, control_handle: _ } => {
                return Ok(Some(self.session_manager.clone().open_session(
                    session.into_stream(),
                    None,
                    self.block_size,
                )));
            }
            fvolume::VolumeRequest::OpenSessionWithOffsetMap {
                session,
                offset_map,
                initial_mappings,
                control_handle: _,
            } => {
                if offset_map.is_some() || initial_mappings.as_ref().is_none_or(|m| m.len() != 1) {
                    // TODO(https://fxbug.dev/402515764): Support multiple mappings and
                    // dynamic querying for FVM as needed.  A single static map is
                    // sufficient for GPT.
                    session.close_with_epitaph(zx::Status::NOT_SUPPORTED)?;
                    return Ok(None);
                }
                let initial_mapping = match initial_mappings.unwrap().pop().unwrap().try_into() {
                    Ok(m) => m,
                    Err(status) => {
                        session.close_with_epitaph(status)?;
                        return Ok(None);
                    }
                };
                let offset_map = OffsetMap::new(initial_mapping);
                return Ok(Some(self.session_manager.clone().open_session(
                    session.into_stream(),
                    Some(offset_map),
                    self.block_size,
                )));
            }
            fvolume::VolumeRequest::GetTypeGuid { responder } => match self.device_info().await {
                Ok(info) => {
                    if let DeviceInfo::Partition(partition_info) = info.as_ref() {
                        let mut guid =
                            fpartition::Guid { value: [0u8; fpartition::GUID_LENGTH as usize] };
                        guid.value.copy_from_slice(&partition_info.type_guid);
                        responder.send(zx::sys::ZX_OK, Some(&guid))?;
                    } else {
                        responder.send(zx::sys::ZX_ERR_NOT_SUPPORTED, None)?;
                    }
                }
                Err(status) => {
                    responder.send(status.into_raw(), None)?;
                }
            },
            fvolume::VolumeRequest::GetInstanceGuid { responder } => {
                match self.device_info().await {
                    Ok(info) => {
                        if let DeviceInfo::Partition(partition_info) = info.as_ref() {
                            let mut guid =
                                fpartition::Guid { value: [0u8; fpartition::GUID_LENGTH as usize] };
                            guid.value.copy_from_slice(&partition_info.instance_guid);
                            responder.send(zx::sys::ZX_OK, Some(&guid))?;
                        } else {
                            responder.send(zx::sys::ZX_ERR_NOT_SUPPORTED, None)?;
                        }
                    }
                    Err(status) => {
                        responder.send(status.into_raw(), None)?;
                    }
                }
            }
            fvolume::VolumeRequest::GetName { responder } => match self.device_info().await {
                Ok(info) => {
                    if let DeviceInfo::Partition(partition_info) = info.as_ref() {
                        responder.send(zx::sys::ZX_OK, Some(&partition_info.name))?;
                    } else {
                        responder.send(zx::sys::ZX_ERR_NOT_SUPPORTED, None)?;
                    }
                }
                Err(status) => {
                    responder.send(status.into_raw(), None)?;
                }
            },
            fvolume::VolumeRequest::GetMetadata { responder } => match self.device_info().await {
                Ok(info) => {
                    if let DeviceInfo::Partition(info) = info.as_ref() {
                        let mut type_guid =
                            fpartition::Guid { value: [0u8; fpartition::GUID_LENGTH as usize] };
                        type_guid.value.copy_from_slice(&info.type_guid);
                        let mut instance_guid =
                            fpartition::Guid { value: [0u8; fpartition::GUID_LENGTH as usize] };
                        instance_guid.value.copy_from_slice(&info.instance_guid);
                        responder.send(Ok(&fpartition::PartitionGetMetadataResponse {
                            name: Some(info.name.clone()),
                            type_guid: Some(type_guid),
                            instance_guid: Some(instance_guid),
                            start_block_offset: info.block_range.as_ref().map(|range| range.start),
                            num_blocks: info
                                .block_range
                                .as_ref()
                                .map(|range| range.end - range.start),
                            flags: Some(info.flags),
                            ..Default::default()
                        }))?;
                    }
                }
                Err(status) => responder.send(Err(status.into_raw()))?,
            },
            fvolume::VolumeRequest::QuerySlices { responder, start_slices } => {
                match self.session_manager.query_slices(&start_slices).await {
                    Ok(mut results) => {
                        let results_len = results.len();
                        assert!(results_len <= 16);
                        results.resize(16, fvolume::VsliceRange { allocated: false, count: 0 });
                        responder.send(
                            zx::sys::ZX_OK,
                            &results.try_into().unwrap(),
                            results_len as u64,
                        )?;
                    }
                    Err(s) => {
                        responder.send(
                            s.into_raw(),
                            &[fvolume::VsliceRange { allocated: false, count: 0 }; 16],
                            0,
                        )?;
                    }
                }
            }
            fvolume::VolumeRequest::GetVolumeInfo { responder, .. } => {
                match self.session_manager.get_volume_info().await {
                    Ok((manager_info, volume_info)) => {
                        responder.send(zx::sys::ZX_OK, Some(&manager_info), Some(&volume_info))?
                    }
                    Err(s) => responder.send(s.into_raw(), None, None)?,
                }
            }
            fvolume::VolumeRequest::Extend { responder, start_slice, slice_count } => {
                responder.send(
                    zx::Status::from(self.session_manager.extend(start_slice, slice_count).await)
                        .into_raw(),
                )?;
            }
            fvolume::VolumeRequest::Shrink { responder, start_slice, slice_count } => {
                responder.send(
                    zx::Status::from(self.session_manager.shrink(start_slice, slice_count).await)
                        .into_raw(),
                )?;
            }
            fvolume::VolumeRequest::Destroy { responder, .. } => {
                responder.send(zx::sys::ZX_ERR_NOT_SUPPORTED)?;
            }
        }
        Ok(None)
    }

    async fn device_info(&self) -> Result<Cow<'_, DeviceInfo>, zx::Status> {
        self.session_manager.get_info().await
    }
}

struct SessionHelper<SM: SessionManager> {
    session_manager: Arc<SM>,
    offset_map: Option<OffsetMap>,
    block_size: u32,
    peer_fifo: zx::Fifo<BlockFifoResponse, BlockFifoRequest>,
    vmos: Mutex<BTreeMap<u16, Arc<zx::Vmo>>>,
    message_groups: FifoMessageGroups,
}

impl<SM: SessionManager> SessionHelper<SM> {
    fn new(
        session_manager: Arc<SM>,
        offset_map: Option<OffsetMap>,
        block_size: u32,
    ) -> Result<(Self, zx::Fifo<BlockFifoRequest, BlockFifoResponse>), zx::Status> {
        let (peer_fifo, fifo) = zx::Fifo::create(16)?;
        Ok((
            Self {
                session_manager,
                offset_map,
                block_size,
                peer_fifo,
                vmos: Mutex::default(),
                message_groups: FifoMessageGroups::default(),
            },
            fifo,
        ))
    }

    async fn handle_request(&self, request: fblock::SessionRequest) -> Result<(), Error> {
        match request {
            fblock::SessionRequest::GetFifo { responder } => {
                let rights = zx::Rights::TRANSFER
                    | zx::Rights::READ
                    | zx::Rights::WRITE
                    | zx::Rights::SIGNAL
                    | zx::Rights::WAIT;
                match self.peer_fifo.duplicate_handle(rights) {
                    Ok(fifo) => responder.send(Ok(fifo.downcast()))?,
                    Err(s) => responder.send(Err(s.into_raw()))?,
                }
                Ok(())
            }
            fblock::SessionRequest::AttachVmo { vmo, responder } => {
                let vmo = Arc::new(vmo);
                let vmo_id = {
                    let mut vmos = self.vmos.lock();
                    if vmos.len() == u16::MAX as usize {
                        responder.send(Err(zx::Status::NO_RESOURCES.into_raw()))?;
                        return Ok(());
                    } else {
                        let vmo_id = match vmos.last_entry() {
                            None => 1,
                            Some(o) => {
                                o.key().checked_add(1).unwrap_or_else(|| {
                                    let mut vmo_id = 1;
                                    // Find the first gap...
                                    for (&id, _) in &*vmos {
                                        if id > vmo_id {
                                            break;
                                        }
                                        vmo_id = id + 1;
                                    }
                                    vmo_id
                                })
                            }
                        };
                        vmos.insert(vmo_id, vmo.clone());
                        vmo_id
                    }
                };
                self.session_manager.clone().on_attach_vmo(&vmo).await?;
                responder.send(Ok(&fblock::VmoId { id: vmo_id }))?;
                Ok(())
            }
            fblock::SessionRequest::Close { responder } => {
                responder.send(Ok(()))?;
                Err(anyhow!("Closed"))
            }
        }
    }

    fn finish_fifo_request(
        &self,
        request: RequestTracking,
        status: zx::Status,
    ) -> Option<BlockFifoResponse> {
        match request.group_or_request {
            GroupOrRequest::Group(group_id) => {
                let response = self.message_groups.complete(group_id, status);
                fuchsia_trace::duration!(
                    c"storage",
                    c"block_server::finish_transaction_in_group",
                    "group" => u32::from(group_id),
                    "group_completed" => response.is_some(),
                    "status" => status.into_raw());
                if let Some(trace_flow_id) = request.trace_flow_id {
                    fuchsia_trace::flow_step!(
                        c"storage",
                        c"block_server::finish_transaction",
                        trace_flow_id.get().into()
                    );
                }
                response
            }
            GroupOrRequest::Request(reqid) => {
                fuchsia_trace::duration!(
                    c"storage", c"block_server::finish_transaction", "status" => status.into_raw());
                if let Some(trace_flow_id) = request.trace_flow_id {
                    fuchsia_trace::flow_step!(
                        c"storage",
                        c"block_server::finish_transaction",
                        trace_flow_id.get().into()
                    );
                }
                Some(BlockFifoResponse { status: status.into_raw(), reqid, ..Default::default() })
            }
        }
    }

    fn decode_fifo_request(&self, request: &BlockFifoRequest) -> Option<DecodedRequest> {
        let flags = BlockIoFlag::from_bits_truncate(request.command.flags);
        let is_group = flags.contains(BlockIoFlag::GROUP_ITEM);
        let last_in_group = flags.contains(BlockIoFlag::GROUP_LAST);
        let mut op_code =
            BlockOpcode::from_primitive(request.command.opcode).ok_or(zx::Status::INVALID_ARGS);

        let group_or_request = if is_group {
            let mut groups = self.message_groups.0.lock();
            let group = groups.entry(request.group).or_insert_with(|| FifoMessageGroup::new());
            if group.req_id.is_some() {
                // We have already received a request tagged as last.
                if group.status == zx::Status::OK {
                    group.status = zx::Status::INVALID_ARGS;
                }
                return None;
            }
            if last_in_group {
                group.req_id = Some(request.reqid);
                // If the group has had an error, there is no point trying to issue this request.
                if group.status != zx::Status::OK {
                    op_code = Err(group.status);
                }
            } else if group.status != zx::Status::OK {
                // The group has already encountered an error, so there is no point trying to issue
                // this request.
                return None;
            }
            group.count += 1;
            GroupOrRequest::Group(request.group)
        } else {
            GroupOrRequest::Request(request.reqid)
        };

        let mut vmo_offset = 0;
        let vmo = match op_code {
            Ok(BlockOpcode::Read) | Ok(BlockOpcode::Write) => (|| {
                if request.length == 0 {
                    return Err(zx::Status::INVALID_ARGS);
                }
                vmo_offset = request
                    .vmo_offset
                    .checked_mul(self.block_size as u64)
                    .ok_or(zx::Status::OUT_OF_RANGE)?;
                self.vmos
                    .lock()
                    .get(&request.vmoid)
                    .cloned()
                    .map_or(Err(zx::Status::IO), |vmo| Ok(Some(vmo)))
            })(),
            Ok(BlockOpcode::CloseVmo) => self
                .vmos
                .lock()
                .remove(&request.vmoid)
                .map_or(Err(zx::Status::IO), |vmo| Ok(Some(vmo))),
            _ => Ok(None),
        }
        .unwrap_or_else(|e| {
            op_code = Err(e);
            None
        });

        let operation = op_code.map(|code| match code {
            BlockOpcode::Read => Operation::Read {
                device_block_offset: request.dev_offset,
                block_count: request.length,
                _unused: 0,
                vmo_offset,
            },
            BlockOpcode::Write => Operation::Write {
                device_block_offset: request.dev_offset,
                block_count: request.length,
                options: if flags.contains(BlockIoFlag::FORCE_ACCESS) {
                    WriteOptions::FORCE_ACCESS
                } else {
                    WriteOptions::empty()
                },
                vmo_offset,
            },
            BlockOpcode::Flush => Operation::Flush,
            BlockOpcode::Trim => Operation::Trim {
                device_block_offset: request.dev_offset,
                block_count: request.length,
            },
            BlockOpcode::CloseVmo => Operation::CloseVmo,
        });
        if let Ok(operation) = operation.as_ref() {
            use fuchsia_trace::ArgValue;
            static CACHE: AtomicU64 = AtomicU64::new(0);
            if let Some(context) =
                fuchsia_trace::TraceCategoryContext::acquire_cached(c"storage", &CACHE)
            {
                let trace_args_with_group = [
                    ArgValue::of("group", u32::from(group_or_request.group_id())),
                    ArgValue::of("opcode", operation.trace_label()),
                ];
                let trace_args = [ArgValue::of("opcode", operation.trace_label())];
                let _scope = if group_or_request.is_group() {
                    fuchsia_trace::duration(
                        c"storage",
                        c"block_server::start_transaction",
                        &trace_args_with_group,
                    )
                } else {
                    fuchsia_trace::duration(
                        c"storage",
                        c"block_server::start_transaction",
                        &trace_args,
                    )
                };
                let trace_flow_id = NonZero::new(request.trace_flow_id);
                if let Some(trace_flow_id) = trace_flow_id.clone() {
                    fuchsia_trace::flow_step(
                        &context,
                        c"block_server::start_trnsaction",
                        trace_flow_id.get().into(),
                        &[],
                    );
                }
            }
        }
        Some(DecodedRequest::new(
            RequestTracking {
                group_or_request,
                trace_flow_id: NonZero::new(request.trace_flow_id),
            },
            operation,
            vmo,
            self.offset_map.as_ref(),
        ))
    }

    fn take_vmos(&self) -> BTreeMap<u16, Arc<zx::Vmo>> {
        std::mem::take(&mut *self.vmos.lock())
    }
}

#[derive(Debug)]
struct DecodedRequest {
    request_tracking: RequestTracking,
    operation: Result<Operation, zx::Status>,
    vmo: Option<Arc<zx::Vmo>>,
}

impl DecodedRequest {
    fn new(
        request_tracking: RequestTracking,
        operation: Result<Operation, zx::Status>,
        vmo: Option<Arc<zx::Vmo>>,
        offset_map: Option<&OffsetMap>,
    ) -> Self {
        let operation =
            operation.and_then(|operation| operation.validate_and_adjust_range(offset_map));
        Self { request_tracking, operation, vmo }
    }
}

/// cbindgen:no-export
pub type WriteOptions = block_protocol::WriteOptions;

#[repr(C)]
#[derive(Debug)]
pub enum Operation {
    // NOTE: On the C++ side, this ends up as a union and, for efficiency reasons, there is code
    // that assumes that some fields for reads and writes (and possibly trim) line-up (e.g. common
    // code can read `device_block_offset` from the read variant and then assume it's valid for the
    // write variant).
    Read {
        device_block_offset: u64,
        block_count: u32,
        _unused: u32,
        vmo_offset: u64,
    },
    Write {
        device_block_offset: u64,
        block_count: u32,
        options: WriteOptions,
        vmo_offset: u64,
    },
    Flush,
    Trim {
        device_block_offset: u64,
        block_count: u32,
    },
    /// This will never be seen by the C interface.
    CloseVmo,
}

impl Operation {
    // Adjusts the operation's block range via `OffsetMap`.  If the request would exceed the range
    // known to the map, or is otherwise invalid, returns an error.
    fn validate_and_adjust_range(
        mut self,
        offset_map: Option<&OffsetMap>,
    ) -> Result<Operation, zx::Status> {
        let adjust_offset = |dev_offset, length| {
            // For compatibility with the C++ server, always ensure the length is nonzero
            if length == 0 {
                return Err(zx::Status::INVALID_ARGS);
            }
            if let Some(offset_map) = offset_map {
                offset_map.adjust_request(dev_offset, length as u64).ok_or(zx::Status::OUT_OF_RANGE)
            } else {
                Ok(dev_offset)
            }
        };
        match &mut self {
            Operation::Read { device_block_offset, block_count, .. } => {
                *device_block_offset = adjust_offset(*device_block_offset, *block_count)?;
            }
            Operation::Write { device_block_offset, block_count, .. } => {
                *device_block_offset = adjust_offset(*device_block_offset, *block_count)?;
            }
            Operation::Trim { device_block_offset, block_count, .. } => {
                *device_block_offset = adjust_offset(*device_block_offset, *block_count)?;
            }
            _ => {}
        }
        Ok(self)
    }
    fn trace_label(&self) -> &'static str {
        match self {
            Operation::Read { .. } => "read",
            Operation::Write { .. } => "write",
            Operation::Flush { .. } => "flush",
            Operation::Trim { .. } => "trim",
            Operation::CloseVmo { .. } => "close_vmo",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RequestTracking {
    group_or_request: GroupOrRequest,
    trace_flow_id: Option<NonZero<u64>>,
}

#[derive(Clone, Copy, Debug)]
pub enum GroupOrRequest {
    Group(u16),
    Request(u32),
}

impl GroupOrRequest {
    fn is_group(&self) -> bool {
        if let Self::Group(_) = self {
            true
        } else {
            false
        }
    }

    fn group_id(&self) -> u16 {
        match self {
            Self::Group(id) => *id,
            Self::Request(_) => 0,
        }
    }
}

/// cbindgen:ignore
const IS_GROUP: u64 = 0x8000_0000_0000_0000;
/// cbindgen:ignore
const USED_VMO: u64 = 0x4000_0000_0000_0000;
#[repr(transparent)]
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct RequestId(u64);

impl RequestId {
    /// Marks the request as having used a VMO, so that we keep the VMO alive until
    /// the request has finished.
    fn with_vmo(self) -> Self {
        Self(self.0 | USED_VMO)
    }

    /// Returns whether the request ID indicates a VMO was used.
    fn did_have_vmo(&self) -> bool {
        self.0 & USED_VMO != 0
    }
}

impl From<GroupOrRequest> for RequestId {
    fn from(value: GroupOrRequest) -> Self {
        match value {
            GroupOrRequest::Group(group) => RequestId(group as u64 | IS_GROUP),
            GroupOrRequest::Request(request) => RequestId(request as u64),
        }
    }
}

impl From<RequestId> for GroupOrRequest {
    fn from(value: RequestId) -> Self {
        if value.0 & IS_GROUP == 0 {
            GroupOrRequest::Request(value.0 as u32)
        } else {
            GroupOrRequest::Group(value.0 as u16)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BlockServer, DeviceInfo, PartitionInfo};
    use crate::async_interface::FIFO_MAX_REQUESTS;
    use assert_matches::assert_matches;
    use block_protocol::{BlockFifoCommand, BlockFifoRequest, BlockFifoResponse, WriteOptions};
    use fidl_fuchsia_hardware_block_driver::{BlockIoFlag, BlockOpcode};
    use fuchsia_sync::Mutex;
    use futures::channel::oneshot;
    use futures::future::BoxFuture;
    use futures::FutureExt as _;
    use std::borrow::Cow;
    use std::future::poll_fn;
    use std::num::NonZero;
    use std::pin::pin;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use zx::{AsHandleRef as _, HandleBased as _};
    use {
        fidl_fuchsia_hardware_block as fblock, fidl_fuchsia_hardware_block_volume as fvolume,
        fuchsia_async as fasync,
    };

    #[derive(Default)]
    struct MockInterface {
        read_hook: Option<
            Box<
                dyn Fn(u64, u32, &Arc<zx::Vmo>, u64) -> BoxFuture<'static, Result<(), zx::Status>>
                    + Send
                    + Sync,
            >,
        >,
    }

    impl super::async_interface::Interface for MockInterface {
        async fn on_attach_vmo(&self, _vmo: &zx::Vmo) -> Result<(), zx::Status> {
            Ok(())
        }

        async fn get_info(&self) -> Result<Cow<'_, DeviceInfo>, zx::Status> {
            Ok(Cow::Owned(test_device_info()))
        }

        async fn read(
            &self,
            device_block_offset: u64,
            block_count: u32,
            vmo: &Arc<zx::Vmo>,
            vmo_offset: u64,
            _trace_flow_id: Option<NonZero<u64>>,
        ) -> Result<(), zx::Status> {
            if let Some(read_hook) = &self.read_hook {
                read_hook(device_block_offset, block_count, vmo, vmo_offset).await
            } else {
                unimplemented!();
            }
        }

        async fn write(
            &self,
            _device_block_offset: u64,
            _block_count: u32,
            _vmo: &Arc<zx::Vmo>,
            _vmo_offset: u64,
            _opts: WriteOptions,
            _trace_flow_id: Option<NonZero<u64>>,
        ) -> Result<(), zx::Status> {
            unreachable!();
        }

        async fn flush(&self, _trace_flow_id: Option<NonZero<u64>>) -> Result<(), zx::Status> {
            unreachable!();
        }

        async fn trim(
            &self,
            _device_block_offset: u64,
            _block_count: u32,
            _trace_flow_id: Option<NonZero<u64>>,
        ) -> Result<(), zx::Status> {
            unreachable!();
        }

        async fn get_volume_info(
            &self,
        ) -> Result<(fvolume::VolumeManagerInfo, fvolume::VolumeInfo), zx::Status> {
            // Hang forever for the test_requests_dont_block_sessions test.
            let () = std::future::pending().await;
            unreachable!();
        }
    }

    const BLOCK_SIZE: u32 = 512;

    fn test_device_info() -> DeviceInfo {
        DeviceInfo::Partition(PartitionInfo {
            device_flags: fblock::Flag::READONLY,
            block_range: Some(12..34),
            type_guid: [1; 16],
            instance_guid: [2; 16],
            name: "foo".to_string(),
            flags: 0xabcd,
        })
    }

    #[fuchsia::test]
    async fn test_info() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fvolume::VolumeMarker>();

        futures::join!(
            async {
                let block_server = BlockServer::new(BLOCK_SIZE, Arc::new(MockInterface::default()));
                block_server.handle_requests(stream).await.unwrap();
            },
            async {
                let expected_info = test_device_info();
                let partition_info = if let DeviceInfo::Partition(info) = &expected_info {
                    info
                } else {
                    unreachable!()
                };

                let block_info = proxy.get_info().await.unwrap().unwrap();
                assert_eq!(
                    block_info.block_count,
                    partition_info.block_range.as_ref().unwrap().end
                        - partition_info.block_range.as_ref().unwrap().start
                );
                assert_eq!(block_info.flags, fblock::Flag::READONLY);

                // TODO(https://fxbug.dev/348077960): Check max_transfer_size

                let (status, type_guid) = proxy.get_type_guid().await.unwrap();
                assert_eq!(status, zx::sys::ZX_OK);
                assert_eq!(&type_guid.as_ref().unwrap().value, &partition_info.type_guid);

                let (status, instance_guid) = proxy.get_instance_guid().await.unwrap();
                assert_eq!(status, zx::sys::ZX_OK);
                assert_eq!(&instance_guid.as_ref().unwrap().value, &partition_info.instance_guid);

                let (status, name) = proxy.get_name().await.unwrap();
                assert_eq!(status, zx::sys::ZX_OK);
                assert_eq!(name.as_ref(), Some(&partition_info.name));

                let metadata = proxy.get_metadata().await.unwrap().expect("get_flags failed");
                assert_eq!(metadata.name, name);
                assert_eq!(metadata.type_guid.as_ref(), type_guid.as_deref());
                assert_eq!(metadata.instance_guid.as_ref(), instance_guid.as_deref());
                assert_eq!(
                    metadata.start_block_offset,
                    Some(partition_info.block_range.as_ref().unwrap().start)
                );
                assert_eq!(metadata.num_blocks, Some(block_info.block_count));
                assert_eq!(metadata.flags, Some(partition_info.flags));

                std::mem::drop(proxy);
            }
        );
    }

    #[fuchsia::test]
    async fn test_attach_vmo() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fvolume::VolumeMarker>();

        let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
        let koid = vmo.get_koid().unwrap();

        futures::join!(
            async {
                let block_server = BlockServer::new(
                    BLOCK_SIZE,
                    Arc::new(MockInterface {
                        read_hook: Some(Box::new(move |_, _, vmo, _| {
                            assert_eq!(vmo.get_koid().unwrap(), koid);
                            Box::pin(async { Ok(()) })
                        })),
                        ..MockInterface::default()
                    }),
                );
                block_server.handle_requests(stream).await.unwrap();
            },
            async move {
                let (session_proxy, server) = fidl::endpoints::create_proxy();

                proxy.open_session(server).unwrap();

                let vmo_id = session_proxy
                    .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                    .await
                    .unwrap()
                    .unwrap();
                assert_ne!(vmo_id.id, 0);

                let mut fifo =
                    fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());
                let (mut reader, mut writer) = fifo.async_io();

                // Keep attaching VMOs until we eventually hit the maximum.
                let mut count = 1;
                loop {
                    match session_proxy
                        .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                        .await
                        .unwrap()
                    {
                        Ok(vmo_id) => assert_ne!(vmo_id.id, 0),
                        Err(e) => {
                            assert_eq!(e, zx::sys::ZX_ERR_NO_RESOURCES);
                            break;
                        }
                    }

                    // Only test every 10 to keep test time down.
                    if count % 10 == 0 {
                        writer
                            .write_entries(&BlockFifoRequest {
                                command: BlockFifoCommand {
                                    opcode: BlockOpcode::Read.into_primitive(),
                                    ..Default::default()
                                },
                                vmoid: vmo_id.id,
                                length: 1,
                                ..Default::default()
                            })
                            .await
                            .unwrap();

                        let mut response = BlockFifoResponse::default();
                        reader.read_entries(&mut response).await.unwrap();
                        assert_eq!(response.status, zx::sys::ZX_OK);
                    }

                    count += 1;
                }

                assert_eq!(count, u16::MAX as u64);

                // Detach the original VMO, and make sure we can then attach another one.
                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::CloseVmo.into_primitive(),
                            ..Default::default()
                        },
                        vmoid: vmo_id.id,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                let mut response = BlockFifoResponse::default();
                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_OK);

                let new_vmo_id = session_proxy
                    .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                    .await
                    .unwrap()
                    .unwrap();
                // It should reuse the same ID.
                assert_eq!(new_vmo_id.id, vmo_id.id);

                std::mem::drop(proxy);
            }
        );
    }

    #[fuchsia::test]
    async fn test_close() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fvolume::VolumeMarker>();

        let mut server = std::pin::pin!(async {
            let block_server = BlockServer::new(BLOCK_SIZE, Arc::new(MockInterface::default()));
            block_server.handle_requests(stream).await.unwrap();
        }
        .fuse());

        let mut client = std::pin::pin!(async {
            let (session_proxy, server) = fidl::endpoints::create_proxy();

            proxy.open_session(server).unwrap();

            // Dropping the proxy should not cause the session to terminate because the session is
            // still live.
            std::mem::drop(proxy);

            session_proxy.close().await.unwrap().unwrap();

            // Keep the session alive.  Calling `close` should cause the server to terminate.
            let _: () = std::future::pending().await;
        }
        .fuse());

        futures::select!(
            _ = server => {}
            _ = client => unreachable!(),
        );
    }

    #[derive(Default)]
    struct IoMockInterface {
        do_checks: bool,
        expected_op: Arc<Mutex<Option<ExpectedOp>>>,
        return_errors: bool,
    }

    #[derive(Debug)]
    enum ExpectedOp {
        Read(u64, u32, u64),
        Write(u64, u32, u64),
        Trim(u64, u32),
        Flush,
    }

    impl super::async_interface::Interface for IoMockInterface {
        async fn on_attach_vmo(&self, _vmo: &zx::Vmo) -> Result<(), zx::Status> {
            Ok(())
        }

        async fn get_info(&self) -> Result<Cow<'_, DeviceInfo>, zx::Status> {
            Ok(Cow::Owned(test_device_info()))
        }

        async fn read(
            &self,
            device_block_offset: u64,
            block_count: u32,
            _vmo: &Arc<zx::Vmo>,
            vmo_offset: u64,
            _trace_flow_id: Option<NonZero<u64>>,
        ) -> Result<(), zx::Status> {
            if self.return_errors {
                Err(zx::Status::INTERNAL)
            } else {
                if self.do_checks {
                    assert_matches!(
                        self.expected_op.lock().take(),
                        Some(ExpectedOp::Read(a, b, c)) if device_block_offset == a &&
                            block_count == b && vmo_offset / BLOCK_SIZE as u64 == c,
                        "Read {device_block_offset} {block_count} {vmo_offset}"
                    );
                }
                Ok(())
            }
        }

        async fn write(
            &self,
            device_block_offset: u64,
            block_count: u32,
            _vmo: &Arc<zx::Vmo>,
            vmo_offset: u64,
            _opts: WriteOptions,
            _trace_flow_id: Option<NonZero<u64>>,
        ) -> Result<(), zx::Status> {
            if self.return_errors {
                Err(zx::Status::NOT_SUPPORTED)
            } else {
                if self.do_checks {
                    assert_matches!(
                        self.expected_op.lock().take(),
                        Some(ExpectedOp::Write(a, b, c)) if device_block_offset == a &&
                            block_count == b && vmo_offset / BLOCK_SIZE as u64 == c,
                        "Write {device_block_offset} {block_count} {vmo_offset}"
                    );
                }
                Ok(())
            }
        }

        async fn flush(&self, _trace_flow_id: Option<NonZero<u64>>) -> Result<(), zx::Status> {
            if self.return_errors {
                Err(zx::Status::NO_RESOURCES)
            } else {
                if self.do_checks {
                    assert_matches!(self.expected_op.lock().take(), Some(ExpectedOp::Flush));
                }
                Ok(())
            }
        }

        async fn trim(
            &self,
            device_block_offset: u64,
            block_count: u32,
            _trace_flow_id: Option<NonZero<u64>>,
        ) -> Result<(), zx::Status> {
            if self.return_errors {
                Err(zx::Status::NO_MEMORY)
            } else {
                if self.do_checks {
                    assert_matches!(
                        self.expected_op.lock().take(),
                        Some(ExpectedOp::Trim(a, b)) if device_block_offset == a &&
                            block_count == b,
                        "Trim {device_block_offset} {block_count}"
                    );
                }
                Ok(())
            }
        }
    }

    #[fuchsia::test]
    async fn test_io() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fvolume::VolumeMarker>();

        let expected_op = Arc::new(Mutex::new(None));
        let expected_op_clone = expected_op.clone();

        let server = async {
            let block_server = BlockServer::new(
                BLOCK_SIZE,
                Arc::new(IoMockInterface {
                    return_errors: false,
                    do_checks: true,
                    expected_op: expected_op_clone,
                }),
            );
            block_server.handle_requests(stream).await.unwrap();
        };

        let client = async move {
            let (session_proxy, server) = fidl::endpoints::create_proxy();

            proxy.open_session(server).unwrap();

            let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
            let vmo_id = session_proxy
                .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                .await
                .unwrap()
                .unwrap();

            let mut fifo =
                fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());
            let (mut reader, mut writer) = fifo.async_io();

            // READ
            *expected_op.lock() = Some(ExpectedOp::Read(1, 2, 3));
            writer
                .write_entries(&BlockFifoRequest {
                    command: BlockFifoCommand {
                        opcode: BlockOpcode::Read.into_primitive(),
                        ..Default::default()
                    },
                    vmoid: vmo_id.id,
                    dev_offset: 1,
                    length: 2,
                    vmo_offset: 3,
                    ..Default::default()
                })
                .await
                .unwrap();

            let mut response = BlockFifoResponse::default();
            reader.read_entries(&mut response).await.unwrap();
            assert_eq!(response.status, zx::sys::ZX_OK);

            // WRITE
            *expected_op.lock() = Some(ExpectedOp::Write(4, 5, 6));
            writer
                .write_entries(&BlockFifoRequest {
                    command: BlockFifoCommand {
                        opcode: BlockOpcode::Write.into_primitive(),
                        ..Default::default()
                    },
                    vmoid: vmo_id.id,
                    dev_offset: 4,
                    length: 5,
                    vmo_offset: 6,
                    ..Default::default()
                })
                .await
                .unwrap();

            let mut response = BlockFifoResponse::default();
            reader.read_entries(&mut response).await.unwrap();
            assert_eq!(response.status, zx::sys::ZX_OK);

            // FLUSH
            *expected_op.lock() = Some(ExpectedOp::Flush);
            writer
                .write_entries(&BlockFifoRequest {
                    command: BlockFifoCommand {
                        opcode: BlockOpcode::Flush.into_primitive(),
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .await
                .unwrap();

            reader.read_entries(&mut response).await.unwrap();
            assert_eq!(response.status, zx::sys::ZX_OK);

            // TRIM
            *expected_op.lock() = Some(ExpectedOp::Trim(7, 8));
            writer
                .write_entries(&BlockFifoRequest {
                    command: BlockFifoCommand {
                        opcode: BlockOpcode::Trim.into_primitive(),
                        ..Default::default()
                    },
                    dev_offset: 7,
                    length: 8,
                    ..Default::default()
                })
                .await
                .unwrap();

            reader.read_entries(&mut response).await.unwrap();
            assert_eq!(response.status, zx::sys::ZX_OK);

            std::mem::drop(proxy);
        };

        futures::join!(server, client);
    }

    #[fuchsia::test]
    async fn test_io_errors() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fvolume::VolumeMarker>();

        futures::join!(
            async {
                let block_server = BlockServer::new(
                    BLOCK_SIZE,
                    Arc::new(IoMockInterface {
                        return_errors: true,
                        do_checks: false,
                        expected_op: Arc::new(Mutex::new(None)),
                    }),
                );
                block_server.handle_requests(stream).await.unwrap();
            },
            async move {
                let (session_proxy, server) = fidl::endpoints::create_proxy();

                proxy.open_session(server).unwrap();

                let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
                let vmo_id = session_proxy
                    .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                    .await
                    .unwrap()
                    .unwrap();

                let mut fifo =
                    fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());
                let (mut reader, mut writer) = fifo.async_io();

                // READ
                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            ..Default::default()
                        },
                        vmoid: vmo_id.id,
                        length: 1,
                        reqid: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                let mut response = BlockFifoResponse::default();
                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_ERR_INTERNAL);

                // WRITE
                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Write.into_primitive(),
                            ..Default::default()
                        },
                        vmoid: vmo_id.id,
                        length: 1,
                        reqid: 2,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_ERR_NOT_SUPPORTED);

                // FLUSH
                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Flush.into_primitive(),
                            ..Default::default()
                        },
                        reqid: 3,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_ERR_NO_RESOURCES);

                // TRIM
                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Trim.into_primitive(),
                            ..Default::default()
                        },
                        reqid: 4,
                        length: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_ERR_NO_MEMORY);

                std::mem::drop(proxy);
            }
        );
    }

    #[fuchsia::test]
    async fn test_invalid_args() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fvolume::VolumeMarker>();

        futures::join!(
            async {
                let block_server = BlockServer::new(
                    BLOCK_SIZE,
                    Arc::new(IoMockInterface {
                        return_errors: false,
                        do_checks: false,
                        expected_op: Arc::new(Mutex::new(None)),
                    }),
                );
                block_server.handle_requests(stream).await.unwrap();
            },
            async move {
                let (session_proxy, server) = fidl::endpoints::create_proxy();

                proxy.open_session(server).unwrap();

                let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
                let vmo_id = session_proxy
                    .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                    .await
                    .unwrap()
                    .unwrap();

                let mut fifo =
                    fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());

                async fn test(
                    fifo: &mut fasync::Fifo<BlockFifoResponse, BlockFifoRequest>,
                    request: BlockFifoRequest,
                ) -> Result<(), zx::Status> {
                    let (mut reader, mut writer) = fifo.async_io();
                    writer.write_entries(&request).await.unwrap();
                    let mut response = BlockFifoResponse::default();
                    reader.read_entries(&mut response).await.unwrap();
                    zx::Status::ok(response.status)
                }

                // READ

                let good_read_request = || BlockFifoRequest {
                    command: BlockFifoCommand {
                        opcode: BlockOpcode::Read.into_primitive(),
                        ..Default::default()
                    },
                    vmoid: vmo_id.id,
                    ..Default::default()
                };

                assert_eq!(
                    test(
                        &mut fifo,
                        BlockFifoRequest { vmoid: vmo_id.id + 1, ..good_read_request() }
                    )
                    .await,
                    Err(zx::Status::INVALID_ARGS)
                );

                assert_eq!(
                    test(
                        &mut fifo,
                        BlockFifoRequest {
                            vmo_offset: 0xffff_ffff_ffff_ffff,
                            ..good_read_request()
                        }
                    )
                    .await,
                    Err(zx::Status::INVALID_ARGS)
                );

                // WRITE

                let good_write_request = || BlockFifoRequest {
                    command: BlockFifoCommand {
                        opcode: BlockOpcode::Write.into_primitive(),
                        ..Default::default()
                    },
                    vmoid: vmo_id.id,
                    ..Default::default()
                };

                assert_eq!(
                    test(
                        &mut fifo,
                        BlockFifoRequest { vmoid: vmo_id.id + 1, ..good_write_request() }
                    )
                    .await,
                    Err(zx::Status::INVALID_ARGS)
                );

                assert_eq!(
                    test(
                        &mut fifo,
                        BlockFifoRequest {
                            vmo_offset: 0xffff_ffff_ffff_ffff,
                            ..good_write_request()
                        }
                    )
                    .await,
                    Err(zx::Status::INVALID_ARGS)
                );

                // CLOSE VMO

                assert_eq!(
                    test(
                        &mut fifo,
                        BlockFifoRequest {
                            command: BlockFifoCommand {
                                opcode: BlockOpcode::CloseVmo.into_primitive(),
                                ..Default::default()
                            },
                            vmoid: vmo_id.id + 1,
                            ..Default::default()
                        }
                    )
                    .await,
                    Err(zx::Status::IO)
                );

                std::mem::drop(proxy);
            }
        );
    }

    #[fuchsia::test]
    async fn test_concurrent_requests() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fvolume::VolumeMarker>();

        let waiting_readers = Arc::new(Mutex::new(Vec::new()));
        let waiting_readers_clone = waiting_readers.clone();

        futures::join!(
            async move {
                let block_server = BlockServer::new(
                    BLOCK_SIZE,
                    Arc::new(MockInterface {
                        read_hook: Some(Box::new(move |dev_block_offset, _, _, _| {
                            let (tx, rx) = oneshot::channel();
                            waiting_readers_clone.lock().push((dev_block_offset as u32, tx));
                            Box::pin(async move {
                                let _ = rx.await;
                                Ok(())
                            })
                        })),
                    }),
                );
                block_server.handle_requests(stream).await.unwrap();
            },
            async move {
                let (session_proxy, server) = fidl::endpoints::create_proxy();

                proxy.open_session(server).unwrap();

                let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
                let vmo_id = session_proxy
                    .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                    .await
                    .unwrap()
                    .unwrap();

                let mut fifo =
                    fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());
                let (mut reader, mut writer) = fifo.async_io();

                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            ..Default::default()
                        },
                        reqid: 1,
                        dev_offset: 1, // Intentionally use the same as `reqid`.
                        vmoid: vmo_id.id,
                        length: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            ..Default::default()
                        },
                        reqid: 2,
                        dev_offset: 2,
                        vmoid: vmo_id.id,
                        length: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                // Wait till both those entries are pending.
                poll_fn(|cx: &mut Context<'_>| {
                    if waiting_readers.lock().len() == 2 {
                        Poll::Ready(())
                    } else {
                        // Yield to the executor.
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;

                let mut response = BlockFifoResponse::default();
                assert!(futures::poll!(pin!(reader.read_entries(&mut response))).is_pending());

                let (id, tx) = waiting_readers.lock().pop().unwrap();
                tx.send(()).unwrap();

                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_OK);
                assert_eq!(response.reqid, id);

                assert!(futures::poll!(pin!(reader.read_entries(&mut response))).is_pending());

                let (id, tx) = waiting_readers.lock().pop().unwrap();
                tx.send(()).unwrap();

                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_OK);
                assert_eq!(response.reqid, id);
            }
        );
    }

    #[fuchsia::test]
    async fn test_groups() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fvolume::VolumeMarker>();

        futures::join!(
            async move {
                let block_server = BlockServer::new(
                    BLOCK_SIZE,
                    Arc::new(MockInterface {
                        read_hook: Some(Box::new(move |_, _, _, _| Box::pin(async { Ok(()) }))),
                    }),
                );
                block_server.handle_requests(stream).await.unwrap();
            },
            async move {
                let (session_proxy, server) = fidl::endpoints::create_proxy();

                proxy.open_session(server).unwrap();

                let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
                let vmo_id = session_proxy
                    .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                    .await
                    .unwrap()
                    .unwrap();

                let mut fifo =
                    fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());
                let (mut reader, mut writer) = fifo.async_io();

                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            flags: BlockIoFlag::GROUP_ITEM.bits(),
                            ..Default::default()
                        },
                        group: 1,
                        vmoid: vmo_id.id,
                        length: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            flags: (BlockIoFlag::GROUP_ITEM | BlockIoFlag::GROUP_LAST).bits(),
                            ..Default::default()
                        },
                        reqid: 2,
                        group: 1,
                        vmoid: vmo_id.id,
                        length: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                let mut response = BlockFifoResponse::default();
                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_OK);
                assert_eq!(response.reqid, 2);
                assert_eq!(response.group, 1);
            }
        );
    }

    #[fuchsia::test]
    async fn test_group_error() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fvolume::VolumeMarker>();

        let counter = Arc::new(AtomicU64::new(0));
        let counter_clone = counter.clone();

        futures::join!(
            async move {
                let block_server = BlockServer::new(
                    BLOCK_SIZE,
                    Arc::new(MockInterface {
                        read_hook: Some(Box::new(move |_, _, _, _| {
                            counter_clone.fetch_add(1, Ordering::Relaxed);
                            Box::pin(async { Err(zx::Status::BAD_STATE) })
                        })),
                    }),
                );
                block_server.handle_requests(stream).await.unwrap();
            },
            async move {
                let (session_proxy, server) = fidl::endpoints::create_proxy();

                proxy.open_session(server).unwrap();

                let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
                let vmo_id = session_proxy
                    .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                    .await
                    .unwrap()
                    .unwrap();

                let mut fifo =
                    fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());
                let (mut reader, mut writer) = fifo.async_io();

                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            flags: BlockIoFlag::GROUP_ITEM.bits(),
                            ..Default::default()
                        },
                        group: 1,
                        vmoid: vmo_id.id,
                        length: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                // Wait until processed.
                poll_fn(|cx: &mut Context<'_>| {
                    if counter.load(Ordering::Relaxed) == 1 {
                        Poll::Ready(())
                    } else {
                        // Yield to the executor.
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;

                let mut response = BlockFifoResponse::default();
                assert!(futures::poll!(pin!(reader.read_entries(&mut response))).is_pending());

                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            flags: BlockIoFlag::GROUP_ITEM.bits(),
                            ..Default::default()
                        },
                        group: 1,
                        vmoid: vmo_id.id,
                        length: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            flags: (BlockIoFlag::GROUP_ITEM | BlockIoFlag::GROUP_LAST).bits(),
                            ..Default::default()
                        },
                        reqid: 2,
                        group: 1,
                        vmoid: vmo_id.id,
                        length: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_ERR_BAD_STATE);
                assert_eq!(response.reqid, 2);
                assert_eq!(response.group, 1);

                assert!(futures::poll!(pin!(reader.read_entries(&mut response))).is_pending());

                // Only the first request should have been processed.
                assert_eq!(counter.load(Ordering::Relaxed), 1);
            }
        );
    }

    #[fuchsia::test]
    async fn test_group_with_two_lasts() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fvolume::VolumeMarker>();

        let (tx, rx) = oneshot::channel();

        futures::join!(
            async move {
                let rx = Mutex::new(Some(rx));
                let block_server = BlockServer::new(
                    BLOCK_SIZE,
                    Arc::new(MockInterface {
                        read_hook: Some(Box::new(move |_, _, _, _| {
                            let rx = rx.lock().take().unwrap();
                            Box::pin(async {
                                let _ = rx.await;
                                Ok(())
                            })
                        })),
                    }),
                );
                block_server.handle_requests(stream).await.unwrap();
            },
            async move {
                let (session_proxy, server) = fidl::endpoints::create_proxy();

                proxy.open_session(server).unwrap();

                let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
                let vmo_id = session_proxy
                    .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                    .await
                    .unwrap()
                    .unwrap();

                let mut fifo =
                    fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());
                let (mut reader, mut writer) = fifo.async_io();

                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            flags: (BlockIoFlag::GROUP_ITEM | BlockIoFlag::GROUP_LAST).bits(),
                            ..Default::default()
                        },
                        reqid: 1,
                        group: 1,
                        vmoid: vmo_id.id,
                        length: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            flags: (BlockIoFlag::GROUP_ITEM | BlockIoFlag::GROUP_LAST).bits(),
                            ..Default::default()
                        },
                        reqid: 2,
                        group: 1,
                        vmoid: vmo_id.id,
                        length: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                // Send an independent request to flush through the fifo.
                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::CloseVmo.into_primitive(),
                            ..Default::default()
                        },
                        reqid: 3,
                        vmoid: vmo_id.id,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                // It should succeed.
                let mut response = BlockFifoResponse::default();
                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_OK);
                assert_eq!(response.reqid, 3);

                // Now release the original request.
                tx.send(()).unwrap();

                // The response should be for the first message tagged as last, and it should be
                // an error because we sent two messages with the LAST marker.
                let mut response = BlockFifoResponse::default();
                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_ERR_INVALID_ARGS);
                assert_eq!(response.reqid, 1);
                assert_eq!(response.group, 1);
            }
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn test_requests_dont_block_sessions() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fvolume::VolumeMarker>();

        let (tx, rx) = oneshot::channel();

        fasync::Task::local(async move {
            let rx = Mutex::new(Some(rx));
            let block_server = BlockServer::new(
                BLOCK_SIZE,
                Arc::new(MockInterface {
                    read_hook: Some(Box::new(move |_, _, _, _| {
                        let rx = rx.lock().take().unwrap();
                        Box::pin(async {
                            let _ = rx.await;
                            Ok(())
                        })
                    })),
                }),
            );
            block_server.handle_requests(stream).await.unwrap();
        })
        .detach();

        let mut fut = pin!(async {
            let (session_proxy, server) = fidl::endpoints::create_proxy();

            proxy.open_session(server).unwrap();

            let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
            let vmo_id = session_proxy
                .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                .await
                .unwrap()
                .unwrap();

            let mut fifo =
                fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());
            let (mut reader, mut writer) = fifo.async_io();

            writer
                .write_entries(&BlockFifoRequest {
                    command: BlockFifoCommand {
                        opcode: BlockOpcode::Read.into_primitive(),
                        flags: (BlockIoFlag::GROUP_ITEM | BlockIoFlag::GROUP_LAST).bits(),
                        ..Default::default()
                    },
                    reqid: 1,
                    group: 1,
                    vmoid: vmo_id.id,
                    length: 1,
                    ..Default::default()
                })
                .await
                .unwrap();

            let mut response = BlockFifoResponse::default();
            reader.read_entries(&mut response).await.unwrap();
            assert_eq!(response.status, zx::sys::ZX_OK);
        });

        // The response won't come back until we send on `tx`.
        assert!(fasync::TestExecutor::poll_until_stalled(&mut fut).await.is_pending());

        let mut fut2 = pin!(proxy.get_volume_info());

        // get_volume_info is set up to stall forever.
        assert!(fasync::TestExecutor::poll_until_stalled(&mut fut2).await.is_pending());

        // If we now free up the first future, it should resolve; the stalled call to
        // get_volume_info should not block the fifo response.
        let _ = tx.send(());

        assert!(fasync::TestExecutor::poll_until_stalled(&mut fut).await.is_ready());
    }

    #[fuchsia::test]
    async fn test_request_flow_control() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fvolume::VolumeMarker>();

        // The client will ensure that MAX_REQUESTS are queued up before firing `event`, and the
        // server will block until that happens.
        const MAX_REQUESTS: u64 = FIFO_MAX_REQUESTS as u64;
        let event = Arc::new((event_listener::Event::new(), AtomicBool::new(false)));
        let event_clone = event.clone();
        futures::join!(
            async move {
                let block_server = BlockServer::new(
                    BLOCK_SIZE,
                    Arc::new(MockInterface {
                        read_hook: Some(Box::new(move |_, _, _, _| {
                            let event_clone = event_clone.clone();
                            Box::pin(async move {
                                if !event_clone.1.load(Ordering::SeqCst) {
                                    event_clone.0.listen().await;
                                }
                                Ok(())
                            })
                        })),
                    }),
                );
                block_server.handle_requests(stream).await.unwrap();
            },
            async move {
                let (session_proxy, server) = fidl::endpoints::create_proxy();

                proxy.open_session(server).unwrap();

                let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
                let vmo_id = session_proxy
                    .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                    .await
                    .unwrap()
                    .unwrap();

                let mut fifo =
                    fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());
                let (mut reader, mut writer) = fifo.async_io();

                for i in 0..MAX_REQUESTS {
                    writer
                        .write_entries(&BlockFifoRequest {
                            command: BlockFifoCommand {
                                opcode: BlockOpcode::Read.into_primitive(),
                                ..Default::default()
                            },
                            reqid: (i + 1) as u32,
                            dev_offset: i,
                            vmoid: vmo_id.id,
                            length: 1,
                            ..Default::default()
                        })
                        .await
                        .unwrap();
                }
                assert!(futures::poll!(pin!(writer.write_entries(&BlockFifoRequest {
                    command: BlockFifoCommand {
                        opcode: BlockOpcode::Read.into_primitive(),
                        ..Default::default()
                    },
                    reqid: u32::MAX,
                    dev_offset: MAX_REQUESTS,
                    vmoid: vmo_id.id,
                    length: 1,
                    ..Default::default()
                })))
                .is_pending());
                // OK, let the server start to process.
                event.1.store(true, Ordering::SeqCst);
                event.0.notify(usize::MAX);
                // For each entry we read, make sure we can write a new one in.
                let mut finished_reqids = vec![];
                for i in MAX_REQUESTS..2 * MAX_REQUESTS {
                    let mut response = BlockFifoResponse::default();
                    reader.read_entries(&mut response).await.unwrap();
                    assert_eq!(response.status, zx::sys::ZX_OK);
                    finished_reqids.push(response.reqid);
                    writer
                        .write_entries(&BlockFifoRequest {
                            command: BlockFifoCommand {
                                opcode: BlockOpcode::Read.into_primitive(),
                                ..Default::default()
                            },
                            reqid: (i + 1) as u32,
                            dev_offset: i,
                            vmoid: vmo_id.id,
                            length: 1,
                            ..Default::default()
                        })
                        .await
                        .unwrap();
                }
                let mut response = BlockFifoResponse::default();
                for _ in 0..MAX_REQUESTS {
                    reader.read_entries(&mut response).await.unwrap();
                    assert_eq!(response.status, zx::sys::ZX_OK);
                    finished_reqids.push(response.reqid);
                }
                // Verify that we got a response for each request.  Note that we can't assume FIFO
                // ordering.
                finished_reqids.sort();
                assert_eq!(finished_reqids.len(), 2 * MAX_REQUESTS as usize);
                let mut i = 1;
                for reqid in finished_reqids {
                    assert_eq!(reqid, i);
                    i += 1;
                }
            }
        );
    }

    #[fuchsia::test]
    async fn test_passthrough_io_with_fixed_map() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fvolume::VolumeMarker>();

        let expected_op = Arc::new(Mutex::new(None));
        let expected_op_clone = expected_op.clone();
        futures::join!(
            async {
                let block_server = BlockServer::new(
                    BLOCK_SIZE,
                    Arc::new(IoMockInterface {
                        return_errors: false,
                        do_checks: true,
                        expected_op: expected_op_clone,
                    }),
                );
                block_server.handle_requests(stream).await.unwrap();
            },
            async move {
                let (session_proxy, server) = fidl::endpoints::create_proxy();

                let mappings = [fblock::BlockOffsetMapping {
                    source_block_offset: 0,
                    target_block_offset: 10,
                    length: 20,
                }];
                proxy.open_session_with_offset_map(server, None, Some(&mappings[..])).unwrap();

                let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
                let vmo_id = session_proxy
                    .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                    .await
                    .unwrap()
                    .unwrap();

                let mut fifo =
                    fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());
                let (mut reader, mut writer) = fifo.async_io();

                // READ
                *expected_op.lock() = Some(ExpectedOp::Read(11, 2, 3));
                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            ..Default::default()
                        },
                        vmoid: vmo_id.id,
                        dev_offset: 1,
                        length: 2,
                        vmo_offset: 3,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                let mut response = BlockFifoResponse::default();
                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_OK);

                // WRITE
                *expected_op.lock() = Some(ExpectedOp::Write(14, 5, 6));
                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Write.into_primitive(),
                            ..Default::default()
                        },
                        vmoid: vmo_id.id,
                        dev_offset: 4,
                        length: 5,
                        vmo_offset: 6,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_OK);

                // FLUSH
                *expected_op.lock() = Some(ExpectedOp::Flush);
                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Flush.into_primitive(),
                            ..Default::default()
                        },
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_OK);

                // TRIM
                *expected_op.lock() = Some(ExpectedOp::Trim(17, 3));
                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Trim.into_primitive(),
                            ..Default::default()
                        },
                        dev_offset: 7,
                        length: 3,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_OK);

                // READ past window
                *expected_op.lock() = None;
                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            ..Default::default()
                        },
                        vmoid: vmo_id.id,
                        dev_offset: 19,
                        length: 2,
                        vmo_offset: 3,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_ERR_OUT_OF_RANGE);

                std::mem::drop(proxy);
            }
        );
    }
}
