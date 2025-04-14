// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::executor::execute;
use crate::verifier::VerifiedEbpfProgram;
use crate::{
    CbpfConfig, DataWidth, EbpfError, EbpfInstruction, MapSchema, MemoryId, StructAccess, Type,
    BPF_CALL, BPF_DW, BPF_JMP, BPF_LDDW, BPF_PSEUDO_MAP_IDX, BPF_SIZE_MASK,
};
use derivative::Derivative;
use std::collections::HashMap;
use std::fmt::Formatter;
use std::mem::size_of;
use zerocopy::{FromBytes, Immutable, IntoBytes};

/// Trait that should be implemented for arguments passed to eBPF programs.
pub trait ProgramArgument: Into<BpfValue> {
    /// Returns eBPF type that corresponds to `Self`. Used when program argument types
    /// are checked statically.
    fn get_type() -> &'static Type;

    /// Returns eBPF type for a specific value of `Self`. For most types this is the
    /// same type that's returned by `get_type()`, but that's not always the case.
    /// In particular for scalar values this will return `Type::ScalarValue` with
    /// the actual value of the scalar and with `unknown_mask = 0`.
    fn get_value_type(&self) -> Type {
        Self::get_type().clone()
    }
}

/// Trait that should be implemented for types that can be converted from `BpfValue`.
/// Used to get a `Packet` when loading a value from the packet.
pub trait FromBpfValue<C>: Sized {
    /// # Safety
    /// Should be called only by the eBPF interpreter when executing verified eBPF code.
    unsafe fn from_bpf_value(context: &mut C, v: BpfValue) -> Self;
}

impl ProgramArgument for () {
    fn get_type() -> &'static Type {
        &Type::UNINITIALIZED
    }
}

impl<C> FromBpfValue<C> for () {
    unsafe fn from_bpf_value(_context: &mut C, _v: BpfValue) -> Self {
        unreachable!();
    }
}

impl ProgramArgument for usize {
    fn get_type() -> &'static Type {
        &Type::UNKNOWN_SCALAR
    }

    fn get_value_type(&self) -> Type {
        Type::from(*self as u64)
    }
}

impl<'a, T, C> FromBpfValue<C> for &'a mut T
where
    &'a mut T: ProgramArgument,
{
    unsafe fn from_bpf_value(_context: &mut C, v: BpfValue) -> Self {
        &mut *v.as_ptr::<T>()
    }
}

impl<'a, T, C> FromBpfValue<C> for &'a T
where
    &'a T: ProgramArgument,
{
    unsafe fn from_bpf_value(_context: &mut C, v: BpfValue) -> Self {
        &*v.as_ptr::<T>()
    }
}

/// A strong reference to an eBPF map held for the lifetime of an eBPF linked
/// with the map. Can be converted to `BpfValue`, which is used by the program
/// to identify the map when it calls map helpers.
pub trait MapReference {
    fn schema(&self) -> &MapSchema;
    fn as_bpf_value(&self) -> BpfValue;
}

/// `MapReference` for `EbpfProgramContext` where maps are not used.
pub enum NoMap {}

impl MapReference for NoMap {
    fn schema(&self) -> &MapSchema {
        unreachable!()
    }
    fn as_bpf_value(&self) -> BpfValue {
        unreachable!()
    }
}

pub trait EbpfProgramContext {
    /// Context for an invocation of an eBPF program.
    type RunContext<'a>;

    /// Packet used by the program.
    type Packet<'a>: Packet + FromBpfValue<Self::RunContext<'a>>;

    /// Arguments passed to the program
    type Arg1<'a>: ProgramArgument;
    type Arg2<'a>: ProgramArgument;
    type Arg3<'a>: ProgramArgument;
    type Arg4<'a>: ProgramArgument;
    type Arg5<'a>: ProgramArgument;

    /// Type used to reference eBPF maps for the lifetime of a program.
    type Map: MapReference;
}

/// Trait that should be implemented by packets passed to eBPF programs.
pub trait Packet {
    fn load(&self, offset: i32, width: DataWidth) -> Option<BpfValue>;
}

impl Packet for () {
    fn load(&self, _offset: i32, _width: DataWidth) -> Option<BpfValue> {
        None
    }
}

/// Simple `Packet` implementation for packets that can be accessed directly.
impl<P: IntoBytes + Immutable> Packet for &P {
    fn load(&self, offset: i32, width: DataWidth) -> Option<BpfValue> {
        let data = (*self).as_bytes();
        if offset < 0 || offset as usize >= data.len() {
            return None;
        }
        let slice = &data[(offset as usize)..];
        match width {
            DataWidth::U8 => u8::read_from_prefix(slice).ok().map(|(v, _)| v.into()),
            DataWidth::U16 => u16::read_from_prefix(slice).ok().map(|(v, _)| v.into()),
            DataWidth::U32 => u32::read_from_prefix(slice).ok().map(|(v, _)| v.into()),
            DataWidth::U64 => u64::read_from_prefix(slice).ok().map(|(v, _)| v.into()),
        }
    }
}

/// A context for a BPF program that's compatible with eBPF and cBPF.
pub trait BpfProgramContext {
    type RunContext<'a>;
    type Packet<'a>: ProgramArgument + Packet + FromBpfValue<Self::RunContext<'a>>;
    type Map: MapReference;
    const CBPF_CONFIG: &'static CbpfConfig;

    fn get_arg_types() -> Vec<Type> {
        vec![<Self::Packet<'_> as ProgramArgument>::get_type().clone()]
    }
}

impl<T: BpfProgramContext + ?Sized> EbpfProgramContext for T {
    type RunContext<'a> = <T as BpfProgramContext>::RunContext<'a>;
    type Packet<'a> = T::Packet<'a>;
    type Arg1<'a> = T::Packet<'a>;
    type Arg2<'a> = ();
    type Arg3<'a> = ();
    type Arg4<'a> = ();
    type Arg5<'a> = ();
    type Map = T::Map;
}

#[derive(Clone, Copy, Debug)]
pub struct BpfValue(u64);

static_assertions::const_assert_eq!(size_of::<BpfValue>(), size_of::<*const u8>());

impl Default for BpfValue {
    fn default() -> Self {
        Self::from(0)
    }
}

impl From<()> for BpfValue {
    fn from(_v: ()) -> Self {
        Self(0)
    }
}

impl From<i32> for BpfValue {
    fn from(v: i32) -> Self {
        Self((v as u32) as u64)
    }
}

impl From<u8> for BpfValue {
    fn from(v: u8) -> Self {
        Self::from(v as u64)
    }
}

impl From<u16> for BpfValue {
    fn from(v: u16) -> Self {
        Self::from(v as u64)
    }
}

impl From<u32> for BpfValue {
    fn from(v: u32) -> Self {
        Self::from(v as u64)
    }
}
impl From<u64> for BpfValue {
    fn from(v: u64) -> Self {
        Self(v)
    }
}

impl From<usize> for BpfValue {
    fn from(v: usize) -> Self {
        Self(v as u64)
    }
}

impl<T> From<*const T> for BpfValue {
    fn from(v: *const T) -> Self {
        Self(v as u64)
    }
}

impl<T> From<*mut T> for BpfValue {
    fn from(v: *mut T) -> Self {
        Self(v as u64)
    }
}

impl<T> From<&'_ T> for BpfValue {
    fn from(v: &'_ T) -> Self {
        Self((v as *const T) as u64)
    }
}

impl<T> From<&'_ mut T> for BpfValue {
    fn from(v: &'_ mut T) -> Self {
        Self((v as *const T) as u64)
    }
}
impl From<BpfValue> for u8 {
    fn from(v: BpfValue) -> u8 {
        v.0 as u8
    }
}

impl From<BpfValue> for u16 {
    fn from(v: BpfValue) -> u16 {
        v.0 as u16
    }
}

impl From<BpfValue> for u32 {
    fn from(v: BpfValue) -> u32 {
        v.0 as u32
    }
}

impl From<BpfValue> for u64 {
    fn from(v: BpfValue) -> u64 {
        v.0
    }
}

impl From<BpfValue> for usize {
    fn from(v: BpfValue) -> usize {
        v.0 as usize
    }
}

impl BpfValue {
    pub fn as_u8(&self) -> u8 {
        self.0 as u8
    }

    pub fn as_u16(&self) -> u16 {
        self.0 as u16
    }

    pub fn as_u32(&self) -> u32 {
        self.0 as u32
    }

    pub fn as_i32(&self) -> i32 {
        self.0 as i32
    }

    pub fn as_u64(&self) -> u64 {
        self.0
    }

    pub fn as_usize(&self) -> usize {
        self.0 as usize
    }

    pub fn as_ptr<T>(&self) -> *mut T {
        self.0 as *mut T
    }
}

impl From<BpfValue> for () {
    fn from(_v: BpfValue) -> Self {
        ()
    }
}

#[derive(Derivative)]
#[derivative(Clone(bound = ""))]
pub struct EbpfHelperImpl<C: EbpfProgramContext>(
    pub  for<'a> fn(
        &mut C::RunContext<'a>,
        BpfValue,
        BpfValue,
        BpfValue,
        BpfValue,
        BpfValue,
    ) -> BpfValue,
);

/// A mapping for a field in a struct where the original ebpf program knows a different offset and
/// data size than the one it receives from the kernel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FieldMapping {
    /// The offset of the field as known by the original ebpf program.
    pub source_offset: usize,
    /// The actual offset of the field in the data provided by the kernel.
    pub target_offset: usize,
}

#[derive(Clone, Debug)]
pub struct StructMapping {
    /// Memory ID used in the struct definition.
    pub memory_id: MemoryId,

    /// The list of mappings in the buffer. The verifier must rewrite the actual ebpf to ensure
    /// the right offset and operand are use to access the mapped fields. Mappings are allowed
    /// only for pointer fields.
    pub fields: Vec<FieldMapping>,
}

pub trait ArgumentTypeChecker<C: EbpfProgramContext>: Sized {
    fn link(program: &VerifiedEbpfProgram) -> Result<Self, EbpfError>;
    fn run_time_check<'a>(
        &self,
        arg1: &C::Arg1<'a>,
        arg2: &C::Arg2<'a>,
        arg3: &C::Arg3<'a>,
        arg4: &C::Arg4<'a>,
        arg5: &C::Arg5<'a>,
    ) -> Result<(), EbpfError>;
}

pub struct StaticTypeChecker();

impl<C: EbpfProgramContext> ArgumentTypeChecker<C> for StaticTypeChecker {
    fn link(program: &VerifiedEbpfProgram) -> Result<Self, EbpfError> {
        let arg_types = [
            C::Arg1::get_type(),
            C::Arg2::get_type(),
            C::Arg3::get_type(),
            C::Arg4::get_type(),
            C::Arg5::get_type(),
        ];
        for i in 0..5 {
            let verified_type = program.args.get(i).unwrap_or(&Type::UNINITIALIZED);
            if !arg_types[i].is_subtype(verified_type) {
                return Err(EbpfError::ProgramLinkError(format!(
                    "Type of argument {} doesn't match. Verified type: {:?}. Context type: {:?}",
                    i + 1,
                    verified_type,
                    arg_types[i],
                )));
            }
        }

        Ok(Self())
    }

    fn run_time_check<'a>(
        &self,
        _arg1: &C::Arg1<'a>,
        _arg2: &C::Arg2<'a>,
        _arg3: &C::Arg3<'a>,
        _arg4: &C::Arg4<'a>,
        _arg5: &C::Arg5<'a>,
    ) -> Result<(), EbpfError> {
        // No-op since argument types were checked in `link()`.
        Ok(())
    }
}

pub struct DynamicTypeChecker {
    types: Vec<Type>,
}

impl<C: EbpfProgramContext> ArgumentTypeChecker<C> for DynamicTypeChecker {
    fn link(program: &VerifiedEbpfProgram) -> Result<Self, EbpfError> {
        Ok(Self { types: program.args.clone() })
    }

    fn run_time_check<'a>(
        &self,
        arg1: &C::Arg1<'a>,
        arg2: &C::Arg2<'a>,
        arg3: &C::Arg3<'a>,
        arg4: &C::Arg4<'a>,
        arg5: &C::Arg5<'a>,
    ) -> Result<(), EbpfError> {
        let arg_types = [
            arg1.get_value_type(),
            arg2.get_value_type(),
            arg3.get_value_type(),
            arg4.get_value_type(),
            arg5.get_value_type(),
        ];
        for i in 0..5 {
            let verified_type = self.types.get(i).unwrap_or(&Type::UNINITIALIZED);
            if !&arg_types[i].is_subtype(verified_type) {
                return Err(EbpfError::ProgramLinkError(format!(
                    "Type of argument {} doesn't match. Verified type: {:?}. Value type: {:?}",
                    i + 1,
                    verified_type,
                    arg_types[i],
                )));
            }
        }

        Ok(())
    }
}

/// An abstraction over an eBPF program and its registered helper functions.
pub struct EbpfProgram<C: EbpfProgramContext, T: ArgumentTypeChecker<C> = StaticTypeChecker> {
    pub(crate) code: Vec<EbpfInstruction>,

    /// List of references to the maps used by the program. This field is not used directly,
    /// but it's kept here to ensure that the maps outlive the program.
    #[allow(dead_code)]
    pub(crate) maps: Vec<C::Map>,

    pub(crate) helpers: HashMap<u32, EbpfHelperImpl<C>>,
    type_checker: T,
}

impl<C: EbpfProgramContext, T: ArgumentTypeChecker<C>> std::fmt::Debug for EbpfProgram<C, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.debug_struct("EbpfProgram").field("code", &self.code).finish()
    }
}

impl<C: EbpfProgramContext, T: ArgumentTypeChecker<C>> EbpfProgram<C, T> {
    pub fn code(&self) -> &[EbpfInstruction] {
        &self.code[..]
    }
}

impl<C, T: ArgumentTypeChecker<C>> EbpfProgram<C, T>
where
    C: for<'a> EbpfProgramContext<Arg2<'a> = (), Arg3<'a> = (), Arg4<'a> = (), Arg5<'a> = ()>,
{
    pub fn run_with_1_argument<'a>(
        &self,
        run_context: &mut C::RunContext<'a>,
        arg1: C::Arg1<'a>,
    ) -> u64 {
        self.type_checker
            .run_time_check(&arg1, &(), &(), &(), &())
            .expect("Failed argument type check");
        execute(&self.code[..], &self.helpers, run_context, &[arg1.into()])
    }
}

impl<C, T: ArgumentTypeChecker<C>> EbpfProgram<C, T>
where
    C: for<'a> EbpfProgramContext<Arg3<'a> = (), Arg4<'a> = (), Arg5<'a> = ()>,
{
    pub fn run_with_2_arguments<'a>(
        &self,
        run_context: &mut C::RunContext<'a>,
        arg1: C::Arg1<'a>,
        arg2: C::Arg2<'a>,
    ) -> u64 {
        self.type_checker
            .run_time_check(&arg1, &arg2, &(), &(), &())
            .expect("Failed argument type check");
        execute(&self.code[..], &self.helpers, run_context, &[arg1.into(), arg2.into()])
    }
}

impl<C: BpfProgramContext, T: ArgumentTypeChecker<C>> EbpfProgram<C, T>
where
    for<'a> C: BpfProgramContext,
{
    /// Executes the current program on the specified `packet`.
    /// The program receives a pointer to the `packet` and the size of the packet as the first
    /// two arguments.
    pub fn run<'a>(
        &self,
        run_context: &mut <C as EbpfProgramContext>::RunContext<'a>,
        packet: C::Packet<'a>,
    ) -> u64 {
        self.run_with_1_argument(run_context, packet)
    }
}

/// Rewrites the code to ensure mapped fields are correctly handled. Returns
/// runnable `EbpfProgram<C>`.
pub fn link_program_internal<C: EbpfProgramContext, T: ArgumentTypeChecker<C>>(
    program: &VerifiedEbpfProgram,
    struct_mappings: &[StructMapping],
    maps: Vec<C::Map>,
    helpers: HashMap<u32, EbpfHelperImpl<C>>,
) -> Result<EbpfProgram<C, T>, EbpfError> {
    let type_checker = T::link(program)?;

    let mut code = program.code.clone();

    // Update offsets in the instructions that access structs.
    for StructAccess { pc, memory_id, field_offset, is_32_bit_ptr_load } in
        program.struct_access_instructions.iter()
    {
        let field_mapping =
            struct_mappings.iter().find(|m| m.memory_id == *memory_id).and_then(|struct_map| {
                struct_map.fields.iter().find(|m| m.source_offset == *field_offset)
            });

        if let Some(field_mapping) = field_mapping {
            let instruction = &mut code[*pc];

            // Note that `instruction.off` may be different from `field.source_offset`. It's adjuststed
            // by the difference between `target_offset` and `source_offset` to ensure the instructions
            // will access the right field.
            let offset_diff = i16::try_from(
                i64::try_from(field_mapping.target_offset).unwrap()
                    - i64::try_from(field_mapping.source_offset).unwrap(),
            )
            .unwrap();

            instruction.off = instruction.off.checked_add(offset_diff).ok_or_else(|| {
                EbpfError::ProgramLinkError(format!("Struct field offset overflow at PC {}", *pc))
            })?;

            // 32-bit pointer loads must be updated to 64-bit loads.
            if *is_32_bit_ptr_load {
                instruction.code = (instruction.code & !BPF_SIZE_MASK) | BPF_DW;
            }
        } else {
            if *is_32_bit_ptr_load {
                return Err(EbpfError::ProgramLinkError(format!(
                    "32-bit field isn't mapped at pc  {}",
                    *pc,
                )));
            }
        }
    }

    for pc in 0..code.len() {
        let instruction = &mut code[pc];

        // Check that we have implementations for all helper calls.
        if instruction.code == (BPF_JMP | BPF_CALL) {
            let helper_id = instruction.imm as u32;
            if helpers.get(&helper_id).is_none() {
                return Err(EbpfError::ProgramLinkError(format!(
                    "Missing implementation for helper with id={}",
                    helper_id,
                )));
            }
        }

        // Link maps.
        if instruction.code == BPF_LDDW {
            // If the instruction references BPF_PSEUDO_MAP_FD, then we need to look up the map fd
            // and create a reference from this program to that object.
            match instruction.src_reg() {
                0 => (),
                BPF_PSEUDO_MAP_IDX => {
                    let map_index = usize::try_from(instruction.imm)
                        .expect("negative map index in a verified program");
                    let map = maps.get(map_index).ok_or_else(|| {
                        EbpfError::ProgramLinkError(format!("Invalid map_index: {}", map_index))
                    })?;
                    assert!(*map.schema() == program.maps[map_index]);

                    let map_ptr = map.as_bpf_value().as_u64();
                    let (high, low) = ((map_ptr >> 32) as i32, map_ptr as i32);
                    instruction.set_src_reg(0);
                    instruction.imm = low;

                    // The code was verified, so this is not expected to overflow.
                    let next_instruction = &mut code[pc + 1];
                    next_instruction.imm = high;
                }
                value => {
                    return Err(EbpfError::ProgramLinkError(format!(
                        "Unsupported value for src_reg in lddw: {}",
                        value,
                    )));
                }
            }
        }
    }

    Ok(EbpfProgram { code, maps, helpers, type_checker })
}

/// Rewrites the code to ensure mapped fields are correctly handled. Returns
/// runnable `EbpfProgram<C>`.
pub fn link_program<C: EbpfProgramContext>(
    program: &VerifiedEbpfProgram,
    struct_mappings: &[StructMapping],
    maps: Vec<C::Map>,
    helpers: HashMap<u32, EbpfHelperImpl<C>>,
) -> Result<EbpfProgram<C>, EbpfError> {
    link_program_internal::<C, StaticTypeChecker>(program, struct_mappings, maps, helpers)
}

/// Same as above, but allows to check argument types in runtime instead of in link time.
pub fn link_program_dynamic<C: EbpfProgramContext>(
    program: &VerifiedEbpfProgram,
    struct_mappings: &[StructMapping],
    maps: Vec<C::Map>,
    helpers: HashMap<u32, EbpfHelperImpl<C>>,
) -> Result<EbpfProgram<C, DynamicTypeChecker>, EbpfError> {
    link_program_internal::<C, DynamicTypeChecker>(program, struct_mappings, maps, helpers)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::api::*;
    use crate::conformance::test::parse_asm;
    use crate::{
        verify_program, CallingContext, FieldDescriptor, FieldMapping, FieldType,
        NullVerifierLogger, ProgramArgument, StructDescriptor, Type,
    };
    use std::mem::offset_of;
    use std::sync::{Arc, LazyLock};
    use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

    #[repr(C)]
    #[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
    struct TestArgument {
        // A field that should not be writable by the program.
        pub read_only_field: u32,
        pub _padding1: u32,
        /// Pointer to an array.
        pub data: u64,
        /// End of the array.
        pub data_end: u64,
        // A field that can be updated by the program.
        pub mutable_field: u32,
        pub _padding2: u32,
    }

    static TEST_ARG_TYPE: LazyLock<Type> = LazyLock::new(|| {
        let data_memory_id = MemoryId::new();
        let descriptor = Arc::new(StructDescriptor {
            fields: vec![
                FieldDescriptor {
                    offset: offset_of!(TestArgument, read_only_field),
                    field_type: FieldType::Scalar { size: 4 },
                },
                FieldDescriptor {
                    offset: offset_of!(TestArgument, data),
                    field_type: FieldType::PtrToArray {
                        is_32_bit: false,
                        id: data_memory_id.clone(),
                    },
                },
                FieldDescriptor {
                    offset: offset_of!(TestArgument, data_end),
                    field_type: FieldType::PtrToEndArray {
                        is_32_bit: false,
                        id: data_memory_id.clone(),
                    },
                },
                FieldDescriptor {
                    offset: offset_of!(TestArgument, mutable_field),
                    field_type: FieldType::MutableScalar { size: 4 },
                },
            ],
        });

        Type::PtrToStruct { id: MemoryId::new(), offset: 0.into(), descriptor }
    });

    impl Default for TestArgument {
        fn default() -> Self {
            Self {
                read_only_field: 1,
                _padding1: 0,
                data: 0,
                data_end: 0,
                mutable_field: 2,
                _padding2: 0,
            }
        }
    }

    impl TestArgument {
        fn from_data(data: &[u64]) -> Self {
            let ptr_range = data.as_ptr_range();
            Self {
                data: ptr_range.start as u64,
                data_end: ptr_range.end as u64,
                ..Default::default()
            }
        }
    }

    impl ProgramArgument for &'_ mut TestArgument {
        fn get_type() -> &'static Type {
            &*TEST_ARG_TYPE
        }
    }

    // A version of TestArgument with 32-bit remapped pointers. It's used to define struct layout
    // for eBPF programs, but not used in the Rust code directly.
    #[repr(C)]
    struct TestArgument32 {
        pub read_only_field: u32,
        pub data: u32,
        pub data_end: u32,
        pub mutable_field: u32,
    }

    #[repr(C)]
    struct TestArgument32BitMapped(TestArgument);

    static TEST_ARG_32_BIT_MEMORY_ID: LazyLock<MemoryId> = LazyLock::new(|| MemoryId::new());
    static TEST_ARG_32_BIT_TYPE: LazyLock<Type> = LazyLock::new(|| {
        let data_memory_id = MemoryId::new();
        let descriptor = Arc::new(StructDescriptor {
            fields: vec![
                FieldDescriptor {
                    offset: offset_of!(TestArgument32, read_only_field),
                    field_type: FieldType::Scalar { size: 4 },
                },
                FieldDescriptor {
                    offset: offset_of!(TestArgument32, data),
                    field_type: FieldType::PtrToArray {
                        is_32_bit: true,
                        id: data_memory_id.clone(),
                    },
                },
                FieldDescriptor {
                    offset: offset_of!(TestArgument32, data_end),
                    field_type: FieldType::PtrToEndArray {
                        is_32_bit: true,
                        id: data_memory_id.clone(),
                    },
                },
                FieldDescriptor {
                    offset: offset_of!(TestArgument32, mutable_field),
                    field_type: FieldType::MutableScalar { size: 4 },
                },
            ],
        });

        Type::PtrToStruct { id: TEST_ARG_32_BIT_MEMORY_ID.clone(), offset: 0.into(), descriptor }
    });

    impl ProgramArgument for &'_ TestArgument32BitMapped {
        fn get_type() -> &'static Type {
            &*TEST_ARG_32_BIT_TYPE
        }
    }

    impl TestArgument32BitMapped {
        fn get_mapping() -> StructMapping {
            StructMapping {
                memory_id: TEST_ARG_32_BIT_MEMORY_ID.clone(),
                fields: vec![
                    FieldMapping {
                        source_offset: offset_of!(TestArgument32, data),
                        target_offset: offset_of!(TestArgument, data),
                    },
                    FieldMapping {
                        source_offset: offset_of!(TestArgument32, data_end),
                        target_offset: offset_of!(TestArgument, data_end),
                    },
                    FieldMapping {
                        source_offset: offset_of!(TestArgument32, mutable_field),
                        target_offset: offset_of!(TestArgument, mutable_field),
                    },
                ],
            }
        }
    }

    struct TestEbpfProgramContext {}

    impl EbpfProgramContext for TestEbpfProgramContext {
        type RunContext<'a> = ();

        type Packet<'a> = ();
        type Arg1<'a> = &'a mut TestArgument;
        type Arg2<'a> = ();
        type Arg3<'a> = ();
        type Arg4<'a> = ();
        type Arg5<'a> = ();

        type Map = NoMap;
    }

    fn initialize_test_program(
        code: Vec<EbpfInstruction>,
    ) -> Result<EbpfProgram<TestEbpfProgramContext>, EbpfError> {
        let verified_program = verify_program(
            code,
            CallingContext { args: vec![TEST_ARG_TYPE.clone()], ..Default::default() },
            &mut NullVerifierLogger,
        )?;
        link_program(&verified_program, &[], vec![], HashMap::default())
    }

    struct TestEbpfProgramContext32BitMapped {}

    impl EbpfProgramContext for TestEbpfProgramContext32BitMapped {
        type RunContext<'a> = ();

        type Packet<'a> = ();
        type Arg1<'a> = &'a TestArgument32BitMapped;
        type Arg2<'a> = ();
        type Arg3<'a> = ();
        type Arg4<'a> = ();
        type Arg5<'a> = ();

        type Map = NoMap;
    }

    fn initialize_test_program_for_32bit_arg(
        code: Vec<EbpfInstruction>,
    ) -> Result<EbpfProgram<TestEbpfProgramContext32BitMapped>, EbpfError> {
        let verified_program = verify_program(
            code,
            CallingContext { args: vec![TEST_ARG_32_BIT_TYPE.clone()], ..Default::default() },
            &mut NullVerifierLogger,
        )?;
        link_program(
            &verified_program,
            &[TestArgument32BitMapped::get_mapping()],
            vec![],
            HashMap::default(),
        )
    }

    #[test]
    fn test_data_end() {
        let program = r#"
        mov %r0, 0
        ldxdw %r2, [%r1+16]
        ldxdw %r1, [%r1+8]
        # ensure data contains at least 8 bytes
        mov %r3, %r1
        add %r3, 0x8
        jgt %r3, %r2, +1
        # read 8 bytes from data
        ldxdw %r0, [%r1]
        exit
        "#;
        let program = initialize_test_program(parse_asm(program)).expect("load");

        let v = [42];
        let mut data = TestArgument::from_data(&v[..]);
        assert_eq!(program.run_with_1_argument(&mut (), &mut data), v[0]);
    }

    #[test]
    fn test_past_data_end() {
        let program = r#"
        mov %r0, 0
        ldxdw %r2, [%r1+16]
        ldxdw %r1, [%r1+6]
        # ensure data contains at least 4 bytes
        mov %r3, %r1
        add %r3, 0x4
        jgt %r3, %r2, +1
        # read 8 bytes from data
        ldxdw %r0, [%r1]
        exit
        "#;
        initialize_test_program(parse_asm(program)).expect_err("incorrect program");
    }

    #[test]
    fn test_mapping() {
        let program = r#"
          # Return `TestArgument32.mutable_field`
          ldxw %r0, [%r1+12]
          exit
        "#;
        let program = initialize_test_program_for_32bit_arg(parse_asm(program)).expect("load");

        let mut data = TestArgument32BitMapped(TestArgument::default());
        assert_eq!(program.run_with_1_argument(&mut (), &mut data), data.0.mutable_field as u64);
    }

    #[test]
    fn test_mapping_partial_load() {
        // Verify that we can access middle of a remapped scalar field.
        let program = r#"
          # Returns two upper bytes of `TestArgument32.mutable_filed`
          ldxh %r0, [%r1+14]
          exit
        "#;
        let program = initialize_test_program_for_32bit_arg(parse_asm(program)).expect("load");

        let mut data = TestArgument32BitMapped(TestArgument::default());
        data.0.mutable_field = 0x12345678;
        assert_eq!(program.run_with_1_argument(&mut (), &mut data), 0x1234 as u64);
    }

    #[test]
    fn test_mapping_ptr() {
        let program = r#"
        mov %r0, 0
        # Load data and data_end as 32 bits pointers in TestArgument32
        ldxw %r2, [%r1+8]
        ldxw %r1, [%r1+4]
        # ensure data contains at least 8 bytes
        mov %r3, %r1
        add %r3, 0x8
        jgt %r3, %r2, +1
        # read 8 bytes from data
        ldxdw %r0, [%r1]
        exit
        "#;
        let program = initialize_test_program_for_32bit_arg(parse_asm(program)).expect("load");

        let v = [42];
        let mut data = TestArgument32BitMapped(TestArgument::from_data(&v[..]));
        assert_eq!(program.run_with_1_argument(&mut (), &mut data), v[0]);
    }

    #[test]
    fn test_mapping_with_offset() {
        let program = r#"
        mov %r0, 0
        add %r1, 0x8
        # Load data and data_end as 32 bits pointers in TestArgument32
        ldxw %r2, [%r1]
        ldxw %r1, [%r1-4]
        # ensure data contains at least 8 bytes
        mov %r3, %r1
        add %r3, 0x8
        jgt %r3, %r2, +1
        # read 8 bytes from data
        ldxdw %r0, [%r1]
        exit
        "#;
        let program = initialize_test_program_for_32bit_arg(parse_asm(program)).expect("load");

        let v = [42];
        let mut data = TestArgument32BitMapped(TestArgument::from_data(&v[..]));
        assert_eq!(program.run_with_1_argument(&mut (), &mut data), v[0]);
    }

    #[test]
    fn test_ptr_diff() {
        let program = r#"
          mov %r0, %r1
          add %r0, 0x2
          # Substract 2 ptr to memory
          sub %r0, %r1

          mov %r2, %r10
          add %r2, 0x3
          # Substract 2 ptr to stack
          sub %r2, %r10
          add %r0, %r2

          ldxdw %r2, [%r1+16]
          ldxdw %r1, [%r1+8]
          # Substract ptr to array and ptr to array end
          sub %r2, %r1
          add %r0, %r2

          mov %r2, %r1
          add %r2, 0x4
          # Substract 2 ptr to array
          sub %r2, %r1
          add %r0, %r2

          exit
        "#;
        let code = parse_asm(program);

        let program = initialize_test_program(code).expect("load");

        let v = [42];
        let mut data = TestArgument::from_data(&v[..]);
        assert_eq!(program.run_with_1_argument(&mut (), &mut data), 17);
    }

    #[test]
    fn test_invalid_packet_load() {
        let program = r#"
        mov %r6, %r2
        mov %r0, 0
        ldpw
        exit
        "#;
        let args = vec![
            Type::PtrToMemory { id: MemoryId::new(), offset: 0.into(), buffer_size: 16 },
            Type::PtrToMemory { id: MemoryId::new(), offset: 0.into(), buffer_size: 16 },
        ];
        let verify_result = verify_program(
            parse_asm(program),
            CallingContext { args, ..Default::default() },
            &mut NullVerifierLogger,
        );

        assert_eq!(
            verify_result.expect_err("validation should fail"),
            EbpfError::ProgramVerifyError("R6 is not a packet at pc 2".to_string())
        );
    }

    #[test]
    fn test_invalid_field_size() {
        // Load with a field size too large fails validation.
        let program = r#"
          ldxdw %r0, [%r1]
          exit
        "#;
        initialize_test_program(parse_asm(program)).expect_err("incorrect program");
    }

    #[test]
    fn test_unknown_field() {
        // Load outside of the know fields fails validation.
        let program = r#"
          ldxw %r0, [%r1 + 4]
          exit
        "#;
        initialize_test_program(parse_asm(program)).expect_err("incorrect program");
    }

    #[test]
    fn test_partial_ptr_field() {
        // Partial loads of ptr fields are not allowed.
        let program = r#"
          ldxw %r0, [%r1 + 8]
          exit
        "#;
        initialize_test_program(parse_asm(program)).expect_err("incorrect program");
    }

    #[test]
    fn test_readonly_field() {
        // Store to a read only field fails validation.
        let program = r#"
          stw [%r1], 0x42
          exit
        "#;
        initialize_test_program(parse_asm(program)).expect_err("incorrect program");
    }

    #[test]
    fn test_store_mutable_field() {
        // Store to a mutable field is allowed.
        let program = r#"
          stw [%r1 + 24], 0x42
          mov %r0, 1
          exit
        "#;
        let program = initialize_test_program(parse_asm(program)).expect("load");

        let mut data = TestArgument::default();
        assert_eq!(program.run_with_1_argument(&mut (), &mut data), 1);
        assert_eq!(data.mutable_field, 0x42);
    }

    #[test]
    fn test_fake_array_bounds_check() {
        // Verify that negative offsets in memory ptrs are handled properly and cannot be used to
        // bypass array bounds checks.
        let program = r#"
        mov %r0, 0
        ldxdw %r2, [%r1+16]
        ldxdw %r1, [%r1+8]
        # Subtract 8 from `data` and pretend checking array bounds.
        mov %r3, %r1
        sub %r3, 0x8
        jgt %r3, %r2, +1
        # Read 8 bytes from `data`. This should be rejected by the verifier.
        ldxdw %r0, [%r1]
        exit
        "#;
        initialize_test_program(parse_asm(program)).expect_err("incorrect program");
    }
}
