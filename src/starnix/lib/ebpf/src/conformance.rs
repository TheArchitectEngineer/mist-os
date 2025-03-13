// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(test)]
pub mod test {
    use crate::{
        link_program_dynamic, verify_program, BpfValue, CallingContext, DataWidth, EbpfHelperImpl,
        EbpfProgramContext, FromBpfValue, FunctionSignature, MemoryId, MemoryParameterSize, NoMap,
        NullVerifierLogger, Packet, ProgramArgument, Type, BPF_ABS, BPF_ADD, BPF_ALU, BPF_ALU64,
        BPF_AND, BPF_ARSH, BPF_ATOMIC, BPF_B, BPF_CALL, BPF_CMPXCHG, BPF_DIV, BPF_DW, BPF_END,
        BPF_EXIT, BPF_FETCH, BPF_H, BPF_IMM, BPF_IND, BPF_JA, BPF_JEQ, BPF_JGE, BPF_JGT, BPF_JLE,
        BPF_JLT, BPF_JMP, BPF_JMP32, BPF_JNE, BPF_JSET, BPF_JSGE, BPF_JSGT, BPF_JSLE, BPF_JSLT,
        BPF_LD, BPF_LDX, BPF_LSH, BPF_MEM, BPF_MOD, BPF_MOV, BPF_MUL, BPF_NEG, BPF_OR, BPF_RSH,
        BPF_SRC_IMM, BPF_SRC_REG, BPF_ST, BPF_STX, BPF_SUB, BPF_TO_BE, BPF_TO_LE, BPF_W, BPF_XCHG,
        BPF_XOR,
    };
    use linux_uapi::bpf_insn;
    use pest::iterators::Pair;
    use pest::Parser;
    use pest_derive::Parser;
    use std::collections::HashMap;
    use std::str::FromStr;
    use std::sync::LazyLock;
    use test_case::test_case;
    use zerocopy::{FromBytes, IntoBytes};

    #[derive(Parser)]
    #[grammar = "../../src/starnix/lib/ebpf/src/test_grammar.pest"]
    struct TestGrammar {}

    const HEXADECIMAL_BASE: u32 = 16;

    enum Value {
        Plus(u64),
        Minus(u64),
    }

    impl Value {
        fn as_u64(&self) -> u64 {
            match self {
                Self::Plus(v) => *v,
                Self::Minus(v) => -(*v as i64) as u64,
            }
        }

        fn as_i32(&self) -> i32 {
            match self {
                Self::Plus(v) => (*v as u32) as i32,
                Self::Minus(v) => i32::try_from(-i64::try_from(*v).unwrap()).unwrap(),
            }
        }

        fn as_i16(&self) -> i16 {
            match self {
                Self::Plus(v) => u16::try_from(*v).unwrap() as i16,
                Self::Minus(v) => i16::try_from(-i64::try_from(*v).unwrap()).unwrap(),
            }
        }

        fn as_i32_pair(&self) -> (i32, i32) {
            let v = self.as_u64();
            let (low, high) = (v as i32, (v >> 32) as i32);
            (low, high)
        }
    }

    struct ConformanceParser {}

    impl ConformanceParser {
        fn parse_result(pair: Pair<'_, Rule>) -> u64 {
            assert_eq!(pair.as_rule(), Rule::RESULT);
            Self::parse_value(pair.into_inner().next().unwrap()).as_u64()
        }

        fn parse_asm(pair: Pair<'_, Rule>) -> Vec<bpf_insn> {
            assert_eq!(pair.as_rule(), Rule::ASM_INSTRUCTIONS);
            let mut result: Vec<bpf_insn> = vec![];
            for entry in pair.into_inner() {
                match entry.as_rule() {
                    Rule::ASM_INSTRUCTION => {
                        for instruction in Self::parse_asm_instruction(entry) {
                            result.push(instruction);
                        }
                    }
                    r @ _ => unreachable!("unexpected rule {r:?}"),
                }
            }
            result
        }

        fn parse_deref(pair: Pair<'_, Rule>) -> (u8, i16) {
            assert_eq!(pair.as_rule(), Rule::DEREF);
            let mut inner = pair.into_inner();
            let reg = Self::parse_reg(inner.next().unwrap());
            let offset =
                if let Some(token) = inner.next() { Self::parse_offset_or_exit(token) } else { 0 };
            (reg, offset)
        }

        fn parse_memory_size(value: &str) -> u8 {
            match value {
                "b" => BPF_B,
                "h" => BPF_H,
                "w" => BPF_W,
                "dw" => BPF_DW,
                r @ _ => unreachable!("unexpected memory size {r:?}"),
            }
        }

        fn parse_mem_instruction(pair: Pair<'_, Rule>) -> Vec<bpf_insn> {
            assert_eq!(pair.as_rule(), Rule::MEM_INSTRUCTION);
            let mut inner = pair.into_inner();
            let op = inner.next().unwrap();
            match op.as_rule() {
                Rule::STORE_REG_OP => {
                    let (dst_reg, offset) = Self::parse_deref(inner.next().unwrap());
                    let src_reg = Self::parse_reg(inner.next().unwrap());
                    let mut instruction = bpf_insn::default();
                    instruction.set_dst_reg(dst_reg);
                    instruction.set_src_reg(src_reg);
                    instruction.off = offset;
                    instruction.code =
                        BPF_MEM | BPF_STX | Self::parse_memory_size(&op.as_str()[3..]);
                    vec![instruction]
                }
                Rule::STORE_IMM_OP => {
                    let (dst_reg, offset) = Self::parse_deref(inner.next().unwrap());
                    let imm = Self::parse_value(inner.next().unwrap()).as_i32();
                    let mut instruction = bpf_insn::default();
                    instruction.set_dst_reg(dst_reg);
                    instruction.imm = imm;
                    instruction.off = offset;
                    instruction.code =
                        BPF_MEM | BPF_ST | Self::parse_memory_size(&op.as_str()[2..]);
                    vec![instruction]
                }
                Rule::LOAD_OP => {
                    let dst_reg = Self::parse_reg(inner.next().unwrap());
                    let (src_reg, offset) = Self::parse_deref(inner.next().unwrap());
                    let mut instruction = bpf_insn::default();
                    instruction.set_dst_reg(dst_reg);
                    instruction.set_src_reg(src_reg);
                    instruction.off = offset;
                    instruction.code =
                        BPF_MEM | BPF_LDX | Self::parse_memory_size(&op.as_str()[3..]);
                    vec![instruction]
                }
                Rule::LDDW_OP => {
                    let mut instructions: Vec<bpf_insn> = vec![];
                    let dst_reg = Self::parse_reg(inner.next().unwrap());
                    let value = Self::parse_value(inner.next().unwrap());
                    let (low, high) = value.as_i32_pair();
                    let mut instruction = bpf_insn::default();
                    instruction.set_dst_reg(dst_reg);
                    instruction.imm = low;
                    instruction.code = BPF_IMM | BPF_LD | BPF_DW;
                    instructions.push(instruction);
                    let mut instruction = bpf_insn::default();
                    instruction.imm = high;
                    instructions.push(instruction);
                    instructions
                }
                Rule::LOAD_PACKET_OP => {
                    let mut instructions: Vec<bpf_insn> = vec![];
                    let mut instruction = bpf_insn::default();
                    let mut is_ind = false;
                    while let Some(inner) = inner.next() {
                        match inner.as_rule() {
                            Rule::REG_NUMBER => {
                                instruction.set_src_reg(Self::parse_reg(inner));
                                is_ind = true;
                            }
                            Rule::OFFSET => {
                                instruction.imm = Self::parse_value(inner).as_i32();
                            }
                            r @ _ => unreachable!("unexpected rule {r:?}"),
                        }
                    }
                    instruction.code = BPF_LD | Self::parse_memory_size(&op.as_str()[3..]);
                    if is_ind {
                        instruction.code |= BPF_IND;
                    } else {
                        instruction.code |= BPF_ABS;
                    }
                    instructions.push(instruction);
                    instructions
                }
                r @ _ => unreachable!("unexpected rule {r:?}"),
            }
        }

        fn parse_asm_instruction(pair: Pair<'_, Rule>) -> Vec<bpf_insn> {
            assert_eq!(pair.as_rule(), Rule::ASM_INSTRUCTION);
            if let Some(entry) = pair.into_inner().next() {
                let mut instruction = bpf_insn::default();
                instruction.code = 0;
                match entry.as_rule() {
                    Rule::ALU_INSTRUCTION => {
                        vec![Self::parse_alu_instruction(entry)]
                    }
                    Rule::ATOMIC_INSTRUCTION => {
                        vec![Self::parse_atomic_instruction(entry)]
                    }
                    Rule::JMP_INSTRUCTION => {
                        vec![Self::parse_jmp_instruction(entry)]
                    }
                    Rule::MEM_INSTRUCTION => Self::parse_mem_instruction(entry),
                    r @ _ => unreachable!("unexpected rule {r:?}"),
                }
            } else {
                vec![]
            }
        }

        fn parse_alu_binary_op(value: &str) -> u8 {
            let mut code: u8 = 0;
            let op = if &value[value.len() - 2..] == "32" {
                code |= BPF_ALU;
                &value[..value.len() - 2]
            } else {
                code |= BPF_ALU64;
                value
            };
            code |= match op {
                "add" => BPF_ADD,
                "sub" => BPF_SUB,
                "mul" => BPF_MUL,
                "div" => BPF_DIV,
                "or" => BPF_OR,
                "and" => BPF_AND,
                "lsh" => BPF_LSH,
                "rsh" => BPF_RSH,
                "mod" => BPF_MOD,
                "xor" => BPF_XOR,
                "mov" => BPF_MOV,
                "arsh" => BPF_ARSH,
                _ => unreachable!("unexpected operation {op}"),
            };
            code
        }

        fn parse_alu_unary_op(value: &str) -> (u8, i32) {
            let (code, imm) = match value {
                "neg" => (BPF_ALU64 | BPF_NEG, 0),
                "neg32" => (BPF_ALU | BPF_NEG, 0),
                "be16" => (BPF_ALU | BPF_END | BPF_TO_BE, 16),
                "be32" => (BPF_ALU | BPF_END | BPF_TO_BE, 32),
                "be64" => (BPF_ALU | BPF_END | BPF_TO_BE, 64),
                "le16" => (BPF_ALU | BPF_END | BPF_TO_LE, 16),
                "le32" => (BPF_ALU | BPF_END | BPF_TO_LE, 32),
                "le64" => (BPF_ALU | BPF_END | BPF_TO_LE, 64),
                _ => unreachable!("unexpected operation {value}"),
            };
            (code, imm)
        }

        fn parse_reg(pair: Pair<'_, Rule>) -> u8 {
            assert_eq!(pair.as_rule(), Rule::REG_NUMBER);
            u8::from_str(&pair.as_str()).expect("parse register")
        }

        fn parse_num(pair: Pair<'_, Rule>) -> u64 {
            assert_eq!(pair.as_rule(), Rule::NUM);
            let num = pair.into_inner().next().unwrap();
            match num.as_rule() {
                Rule::DECNUM => num.as_str().parse().unwrap(),
                Rule::HEXSUFFIX => u64::from_str_radix(num.as_str(), HEXADECIMAL_BASE).unwrap(),
                r @ _ => unreachable!("unexpected rule {r:?}"),
            }
        }

        fn parse_value(pair: Pair<'_, Rule>) -> Value {
            assert!(pair.as_rule() == Rule::IMM || pair.as_rule() == Rule::OFFSET);
            let mut inner = pair.into_inner();
            let mut negative = false;
            let maybe_sign = inner.next().unwrap();
            let num = {
                match maybe_sign.as_rule() {
                    Rule::SIGN => {
                        negative = maybe_sign.as_str() == "-";
                        inner.next().unwrap()
                    }
                    Rule::NUM => maybe_sign,
                    r @ _ => unreachable!("unexpected rule {r:?}"),
                }
            };
            let num = Self::parse_num(num);
            if negative {
                Value::Minus(num)
            } else {
                Value::Plus(num)
            }
        }

        fn parse_src(pair: Pair<'_, Rule>, instruction: &mut bpf_insn) {
            match pair.as_rule() {
                Rule::REG_NUMBER => {
                    instruction.set_src_reg(Self::parse_reg(pair));
                    instruction.code |= BPF_SRC_REG;
                }
                Rule::IMM => {
                    instruction.imm = Self::parse_value(pair).as_i32();
                    instruction.code |= BPF_SRC_IMM;
                }
                r @ _ => unreachable!("unexpected rule {r:?}"),
            }
        }

        fn parse_alu_instruction(pair: Pair<'_, Rule>) -> bpf_insn {
            let mut instruction = bpf_insn::default();
            let mut inner = pair.into_inner();
            let op = inner.next().unwrap();
            match op.as_rule() {
                Rule::BINARY_OP => {
                    instruction.code = Self::parse_alu_binary_op(op.as_str());
                    instruction.set_dst_reg(Self::parse_reg(inner.next().unwrap()));
                    Self::parse_src(inner.next().unwrap(), &mut instruction);
                }
                Rule::UNARY_OP => {
                    instruction.set_dst_reg(Self::parse_reg(inner.next().unwrap()));
                    let (code, imm) = Self::parse_alu_unary_op(op.as_str());
                    instruction.code = code;
                    instruction.imm = imm;
                }
                r @ _ => unreachable!("unexpected rule {r:?}"),
            }
            instruction
        }

        fn parse_atomic_instruction(pair: Pair<'_, Rule>) -> bpf_insn {
            let mut instruction = bpf_insn::default();
            let mut inner = pair.into_inner();
            let (op, fetch) = {
                let next = inner.next().unwrap();
                let fetch = next.as_rule() == Rule::FETCH;
                if fetch {
                    (inner.next().unwrap(), fetch)
                } else {
                    (next, false)
                }
            };
            assert_eq!(op.as_rule(), Rule::ATOMIC_OP);
            let (op, is_32) = {
                let op = op.as_str();
                if op.ends_with("32") {
                    (&op[0..op.len() - 2], true)
                } else {
                    (&op[..], false)
                }
            };
            instruction.code = BPF_ATOMIC | BPF_STX;
            if is_32 {
                instruction.code |= BPF_W;
            } else {
                instruction.code |= BPF_DW;
            };
            let mut imm = match op {
                "add" => BPF_ADD,
                "and" => BPF_AND,
                "or" => BPF_OR,
                "xor" => BPF_XOR,
                "xchg" => BPF_XCHG,
                "cmpxchg" => BPF_CMPXCHG,
                _ => unreachable!("unexpected operation {op}"),
            };
            if fetch {
                imm |= BPF_FETCH;
            }
            instruction.imm = imm as i32;
            let (dst_reg, offset) = Self::parse_deref(inner.next().unwrap());
            let src_reg = Self::parse_reg(inner.next().unwrap());
            instruction.set_dst_reg(dst_reg);
            instruction.set_src_reg(src_reg);
            instruction.off = offset;
            instruction
        }

        fn parse_offset_or_exit(pair: Pair<'_, Rule>) -> i16 {
            match pair.as_rule() {
                Rule::OFFSET => Self::parse_value(pair).as_i16(),
                // This has no equivalent in ebpf. Ensure the verification fails if it takes
                // branch.
                Rule::EXIT => -1,
                r @ _ => unreachable!("unexpected rule {r:?}"),
            }
        }

        fn parse_jmp_op(value: &str) -> u8 {
            let mut code: u8 = 0;
            // Special case for operation ending by 32 but not being BPF_ALU necessarily
            let op = if &value[value.len() - 2..] == "32" {
                code |= BPF_JMP32;
                &value[..value.len() - 2]
            } else {
                code |= BPF_JMP;
                value
            };
            code |= match op {
                "jeq" => BPF_JEQ,
                "jgt" => BPF_JGT,
                "jge" => BPF_JGE,
                "jlt" => BPF_JLT,
                "jle" => BPF_JLE,
                "jset" => BPF_JSET,
                "jne" => BPF_JNE,
                "jsgt" => BPF_JSGT,
                "jsge" => BPF_JSGE,
                "jslt" => BPF_JSLT,
                "jsle" => BPF_JSLE,
                _ => unreachable!("unexpected operation {op}"),
            };
            code
        }
        fn parse_jmp_instruction(pair: Pair<'_, Rule>) -> bpf_insn {
            let mut instruction = bpf_insn::default();
            let mut inner = pair.into_inner();
            let op = inner.next().unwrap();
            match op.as_rule() {
                Rule::JMP_CONDITIONAL => {
                    let mut inner = op.into_inner();
                    instruction.code = Self::parse_jmp_op(inner.next().unwrap().as_str());
                    instruction.set_dst_reg(Self::parse_reg(inner.next().unwrap()));
                    Self::parse_src(inner.next().unwrap(), &mut instruction);
                    instruction.off = Self::parse_offset_or_exit(inner.next().unwrap());
                }
                Rule::JMP => {
                    let mut inner = op.into_inner();
                    instruction.code = BPF_JMP | BPF_JA;
                    instruction.off = Self::parse_offset_or_exit(inner.next().unwrap());
                }
                Rule::CALL => {
                    let mut inner = op.into_inner();
                    instruction.code = BPF_JMP | BPF_CALL;
                    instruction.imm = Self::parse_value(inner.next().unwrap()).as_i32();
                }
                Rule::EXIT => {
                    instruction.code = BPF_JMP | BPF_EXIT;
                }
                r @ _ => unreachable!("unexpected rule {r:?}"),
            }
            instruction
        }
    }

    struct TestEbpfRunContext {
        buffer_size: usize,
    }

    const DATA_OFFSET: usize = 8;

    // The pointer to the buffer passed to the test as an argument.
    #[derive(Clone, Copy)]
    struct TestBuffer {
        ptr: *const u8,
        size: usize,
    }

    impl TestBuffer {
        fn new(memory: &mut Option<Vec<u8>>) -> Self {
            match memory {
                Some(data) => Self { ptr: data.as_mut_ptr(), size: data.len() },
                None => Self { ptr: std::ptr::null(), size: 0 },
            }
        }
    }

    impl From<TestBuffer> for BpfValue {
        fn from(v: TestBuffer) -> Self {
            (v.ptr as u64).into()
        }
    }

    static TEST_ARG_MEMORY_ID: LazyLock<MemoryId> = LazyLock::new(|| MemoryId::new());

    impl ProgramArgument for TestBuffer {
        fn get_type() -> &'static Type {
            // Shouldn't be called - argument types are checked dynamically, see
            // `link_program_dynamic`.
            unreachable!();
        }

        fn get_value_type(&self) -> Type {
            if self.ptr.is_null() {
                Type::from(0)
            } else {
                Type::PtrToMemory {
                    id: TEST_ARG_MEMORY_ID.clone(),
                    offset: 0,
                    buffer_size: self.size as u64,
                }
            }
        }
    }

    impl Packet for TestBuffer {
        fn load(&self, offset: i32, width: DataWidth) -> Option<BpfValue> {
            let TestBuffer { ptr: packet_ptr, size: packet_size } = self;
            if offset < 0 || offset as usize + width.bytes() > *packet_size {
                return None;
            }
            // SAFETY: Packet size is checked above.
            let addr = unsafe { packet_ptr.add(DATA_OFFSET + offset as usize) };
            let value = match width {
                DataWidth::U8 => BpfValue::from(unsafe { *addr }),
                DataWidth::U16 => {
                    BpfValue::from(unsafe { std::ptr::read_unaligned(addr as *const u16) })
                }
                DataWidth::U32 => {
                    BpfValue::from(unsafe { std::ptr::read_unaligned(addr as *const u32) })
                }
                DataWidth::U64 => {
                    BpfValue::from(unsafe { std::ptr::read_unaligned(addr as *const u64) })
                }
            };
            Some(value)
        }
    }

    impl FromBpfValue<TestEbpfRunContext> for TestBuffer {
        unsafe fn from_bpf_value(context: &mut TestEbpfRunContext, v: BpfValue) -> Self {
            Self { ptr: v.as_ptr::<u8>(), size: context.buffer_size }
        }
    }

    struct TestEbpfProgramContext {}

    impl EbpfProgramContext for TestEbpfProgramContext {
        type RunContext<'a> = TestEbpfRunContext;
        type Packet<'a> = TestBuffer;
        type Arg1<'a> = TestBuffer;
        type Arg2<'a> = usize;
        type Arg3<'a> = ();
        type Arg4<'a> = ();
        type Arg5<'a> = ();
        type Map = NoMap;
    }

    struct TestCase {
        code: Vec<bpf_insn>,
        result: Option<u64>,
        memory: Option<Vec<u8>>,
    }

    impl TestCase {
        fn parse(content: &str) -> Option<Self> {
            let mut pairs =
                TestGrammar::parse(Rule::rules, content).expect("Parsing must be successful");
            let mut code: Option<Vec<bpf_insn>> = None;
            let mut result: Option<Option<u64>> = None;
            let mut memory: Option<Vec<u8>> = None;
            let mut raw: Option<Vec<bpf_insn>> = None;
            for entry in pairs.next().unwrap().into_inner() {
                match entry.as_rule() {
                    Rule::ASM_INSTRUCTIONS => {
                        assert!(code.is_none());
                        code = Some(ConformanceParser::parse_asm(entry));
                    }
                    Rule::RESULT => {
                        if result.is_none() {
                            result = Some(Some(ConformanceParser::parse_result(entry)));
                        }
                    }
                    Rule::ERROR => {
                        result = Some(None);
                    }
                    Rule::MEMORY => {
                        assert!(memory.is_none());
                        let mut bytes = vec![];
                        for byte_pair in entry.into_inner() {
                            assert_eq!(byte_pair.as_rule(), Rule::MEMORY_DATA);
                            bytes.push(
                                u8::from_str_radix(byte_pair.as_str(), HEXADECIMAL_BASE).unwrap(),
                            );
                        }
                        memory = Some(bytes);
                    }
                    Rule::RAW => {
                        assert!(raw.is_none());
                        let mut instructions = vec![];
                        for byte_str in entry.into_inner() {
                            assert_eq!(byte_str.as_rule(), Rule::RAW_VALUE);
                            let value =
                                u64::from_str_radix(byte_str.as_str(), HEXADECIMAL_BASE).unwrap();
                            instructions.push(bpf_insn::read_from_bytes(value.as_bytes()).unwrap());
                        }
                        raw = Some(instructions);
                    }
                    Rule::EOI => (),
                    r @ _ => unreachable!("unexpected rule {r:?}"),
                }
            }
            assert!(raw.is_some() || code.is_some());
            if raw.is_some() && code.is_some() {
                // Check equality
                let raw = raw.as_ref().unwrap();
                let code = code.as_ref().unwrap();
                assert_eq!(raw.len(), code.len());
                for (raw, code) in raw.iter().zip(code.iter()) {
                    assert_eq!(raw.as_bytes(), code.as_bytes());
                }
                if result.is_none() {
                    // Special case that only tests the assembler.
                    return None;
                }
            }
            let code = if let Some(code) = code { code } else { raw.unwrap() };
            Some(TestCase { code, result: result.unwrap(), memory })
        }
    }

    fn gather_bytes(
        _context: &mut TestEbpfRunContext,
        a: BpfValue,
        b: BpfValue,
        c: BpfValue,
        d: BpfValue,
        e: BpfValue,
    ) -> BpfValue {
        let a = u64::from(a) & 0xff;
        let b = u64::from(b) & 0xff;
        let c = u64::from(c) & 0xff;
        let d = u64::from(d) & 0xff;
        let e = u64::from(e) & 0xff;
        BpfValue::from((a << 32) | (b << 24) | (c << 16) | (d << 8) | e)
    }

    fn memfrob(
        _context: &mut TestEbpfRunContext,
        ptr: BpfValue,
        n: BpfValue,
        _: BpfValue,
        _: BpfValue,
        _: BpfValue,
    ) -> BpfValue {
        let n = n.as_usize();
        let slice = unsafe { std::slice::from_raw_parts_mut(ptr.as_ptr::<u8>(), n) };
        for c in slice.iter_mut() {
            *c ^= 42;
        }
        slice.as_mut_ptr().into()
    }

    fn trash_registers(
        _context: &mut TestEbpfRunContext,
        _: BpfValue,
        _: BpfValue,
        _: BpfValue,
        _: BpfValue,
        _: BpfValue,
    ) -> BpfValue {
        0.into()
    }

    fn sqrti(
        _context: &mut TestEbpfRunContext,
        v: BpfValue,
        _: BpfValue,
        _: BpfValue,
        _: BpfValue,
        _: BpfValue,
    ) -> BpfValue {
        BpfValue::from((u64::from(v) as f64).sqrt() as u64)
    }

    fn strcmp_ext(
        _context: &mut TestEbpfRunContext,
        s1: BpfValue,
        s2: BpfValue,
        _: BpfValue,
        _: BpfValue,
        _: BpfValue,
    ) -> BpfValue {
        let mut s1 = s1.as_ptr::<u8>();
        let mut s2 = s2.as_ptr::<u8>();
        loop {
            let c1 = unsafe { *s1 };
            let c2 = unsafe { *s2 };
            if c1 != c2 {
                if c2 > c1 {
                    return 1.into();
                } else {
                    return u64::MAX.into();
                }
            }
            if c1 == 0 {
                return 0.into();
            }
            s1 = unsafe { s1.offset(1) };
            s2 = unsafe { s2.offset(1) };
        }
    }

    fn null_or(
        _context: &mut TestEbpfRunContext,
        s1: BpfValue,
        _: BpfValue,
        _: BpfValue,
        _: BpfValue,
        _: BpfValue,
    ) -> BpfValue {
        s1
    }

    fn read_only(
        _context: &mut TestEbpfRunContext,
        s1: BpfValue,
        _: BpfValue,
        _: BpfValue,
        _: BpfValue,
        _: BpfValue,
    ) -> BpfValue {
        let s1 = s1.as_ptr::<u64>();
        let v1 = unsafe { *s1 };
        v1.into()
    }

    fn write_only(
        _context: &mut TestEbpfRunContext,
        s1: BpfValue,
        s2: BpfValue,
        _: BpfValue,
        _: BpfValue,
        _: BpfValue,
    ) -> BpfValue {
        let s1 = s1.as_ptr::<u64>();
        unsafe { *s1 = s2.into() };
        0.into()
    }

    fn malloc(
        _context: &mut TestEbpfRunContext,
        size: BpfValue,
        _: BpfValue,
        _: BpfValue,
        _: BpfValue,
        _: BpfValue,
    ) -> BpfValue {
        unsafe { libc::malloc(usize::from(size) as libc::size_t) }.into()
    }

    fn free(
        _context: &mut TestEbpfRunContext,
        ptr: BpfValue,
        _: BpfValue,
        _: BpfValue,
        _: BpfValue,
        _: BpfValue,
    ) -> BpfValue {
        unsafe { libc::free(usize::from(ptr) as *mut libc::c_void) };
        0.into()
    }

    pub fn parse_asm(data: &str) -> Vec<bpf_insn> {
        let mut pairs =
            TestGrammar::parse(Rule::ASM_INSTRUCTIONS, data).expect("Parsing must be successful");
        ConformanceParser::parse_asm(pairs.next().unwrap())
    }

    #[test]
    fn test_parse_asm() {
        let code = "exit\n";
        assert_eq!(parse_asm(code).len(), 1);
    }

    macro_rules! ubpf_test_data {
        ($file_name:tt) => {
            include_str!(concat!("../../../../../third_party/ubpf/src/tests/", $file_name))
        };
    }

    macro_rules! local_test_data {
        ($file_name:tt) => {
            include_str!(concat!("tests/", $file_name))
        };
    }

    #[test_case(ubpf_test_data!("add64.data"))]
    #[test_case(ubpf_test_data!("add.data"))]
    #[test_case(ubpf_test_data!("alu64-arith.data"))]
    #[test_case(ubpf_test_data!("alu64-bit.data"))]
    #[test_case(ubpf_test_data!("alu64.data"))]
    #[test_case(ubpf_test_data!("alu-arith.data"))]
    #[test_case(ubpf_test_data!("alu-bit.data"))]
    #[test_case(ubpf_test_data!("alu.data"))]
    #[test_case(ubpf_test_data!("arsh32-high-shift.data"))]
    #[test_case(ubpf_test_data!("arsh64.data"))]
    #[test_case(ubpf_test_data!("arsh.data"))]
    #[test_case(ubpf_test_data!("arsh-reg.data"))]
    #[test_case(ubpf_test_data!("be16.data"))]
    #[test_case(ubpf_test_data!("be16-high.data"))]
    #[test_case(ubpf_test_data!("be32.data"))]
    #[test_case(ubpf_test_data!("be32-high.data"))]
    #[test_case(ubpf_test_data!("be64.data"))]
    #[test_case(ubpf_test_data!("call.data"))]
    #[test_case(ubpf_test_data!("call-memfrob.data"))]
    #[test_case(ubpf_test_data!("call-save.data"))]
    #[test_case(ubpf_test_data!("div32-by-zero-reg.data"))]
    #[test_case(ubpf_test_data!("div32-high-divisor.data"))]
    #[test_case(ubpf_test_data!("div32-imm.data"))]
    #[test_case(ubpf_test_data!("div32-reg.data"))]
    #[test_case(ubpf_test_data!("div64-by-zero-imm.data"))]
    #[test_case(ubpf_test_data!("div64-by-zero-reg.data"))]
    #[test_case(ubpf_test_data!("div64-imm.data"))]
    #[test_case(ubpf_test_data!("div64-negative-imm.data"))]
    #[test_case(ubpf_test_data!("div64-negative-reg.data"))]
    #[test_case(ubpf_test_data!("div64-reg.data"))]
    #[test_case(ubpf_test_data!("div-by-zero-imm.data"))]
    #[test_case(ubpf_test_data!("div-by-zero-reg.data"))]
    #[test_case(ubpf_test_data!("early-exit.data"))]
    #[test_case(ubpf_test_data!("err-call-bad-imm.data"))]
    #[test_case(ubpf_test_data!("err-call-unreg.data"))]
    #[test_case(ubpf_test_data!("err-endian-size.data"))]
    #[test_case(ubpf_test_data!("err-incomplete-lddw2.data"))]
    #[test_case(ubpf_test_data!("err-incomplete-lddw.data"))]
    #[test_case(ubpf_test_data!("err-infinite-loop.data"))]
    #[test_case(ubpf_test_data!("err-invalid-reg-dst.data"))]
    #[test_case(ubpf_test_data!("err-invalid-reg-src.data"))]
    #[test_case(ubpf_test_data!("err-jmp-lddw.data"))]
    #[test_case(ubpf_test_data!("err-jmp-out.data"))]
    #[test_case(ubpf_test_data!("err-lddw-invalid-src.data"))]
    #[test_case(ubpf_test_data!("err-stack-oob.data"))]
    #[test_case(ubpf_test_data!("err-too-many-instructions.data"))]
    #[test_case(ubpf_test_data!("err-unknown-opcode.data"))]
    #[test_case(ubpf_test_data!("exit.data"))]
    #[test_case(ubpf_test_data!("exit-not-last.data"))]
    #[test_case(ubpf_test_data!("ja.data"))]
    #[test_case(ubpf_test_data!("jeq-imm.data"))]
    #[test_case(ubpf_test_data!("jeq-reg.data"))]
    #[test_case(ubpf_test_data!("jge-imm.data"))]
    #[test_case(ubpf_test_data!("jgt-imm.data"))]
    #[test_case(ubpf_test_data!("jgt-reg.data"))]
    #[test_case(ubpf_test_data!("jit-bounce.data"))]
    #[test_case(ubpf_test_data!("jle-imm.data"))]
    #[test_case(ubpf_test_data!("jle-reg.data"))]
    #[test_case(ubpf_test_data!("jlt-imm.data"))]
    #[test_case(ubpf_test_data!("jlt-reg.data"))]
    #[test_case(ubpf_test_data!("jmp.data"))]
    #[test_case(ubpf_test_data!("jne-reg.data"))]
    #[test_case(ubpf_test_data!("jset-imm.data"))]
    #[test_case(ubpf_test_data!("jset-reg.data"))]
    #[test_case(ubpf_test_data!("jsge-imm.data"))]
    #[test_case(ubpf_test_data!("jsge-reg.data"))]
    #[test_case(ubpf_test_data!("jsgt-imm.data"))]
    #[test_case(ubpf_test_data!("jsgt-reg.data"))]
    #[test_case(ubpf_test_data!("jsle-imm.data"))]
    #[test_case(ubpf_test_data!("jsle-reg.data"))]
    #[test_case(ubpf_test_data!("jslt-imm.data"))]
    #[test_case(ubpf_test_data!("jslt-reg.data"))]
    #[test_case(ubpf_test_data!("lddw2.data"))]
    #[test_case(ubpf_test_data!("lddw.data"))]
    #[test_case(ubpf_test_data!("ldxb-all.data"))]
    #[test_case(ubpf_test_data!("ldxb.data"))]
    #[test_case(ubpf_test_data!("ldx.data"))]
    #[test_case(ubpf_test_data!("ldxdw.data"))]
    #[test_case(ubpf_test_data!("ldxh-all2.data"))]
    #[test_case(ubpf_test_data!("ldxh-all.data"))]
    #[test_case(ubpf_test_data!("ldxh.data"))]
    #[test_case(ubpf_test_data!("ldxh-same-reg.data"))]
    #[test_case(ubpf_test_data!("ldxw-all.data"))]
    #[test_case(ubpf_test_data!("ldxw.data"))]
    #[test_case(ubpf_test_data!("le16.data"))]
    #[test_case(ubpf_test_data!("le32.data"))]
    #[test_case(ubpf_test_data!("le64.data"))]
    #[test_case(ubpf_test_data!("lsh-reg.data"))]
    #[test_case(ubpf_test_data!("mem-len.data"))]
    #[test_case(ubpf_test_data!("mod32.data"))]
    #[test_case(ubpf_test_data!("mod64-by-zero-imm.data"))]
    #[test_case(ubpf_test_data!("mod64-by-zero-reg.data"))]
    #[test_case(ubpf_test_data!("mod64.data"))]
    #[test_case(ubpf_test_data!("mod-by-zero-imm.data"))]
    #[test_case(ubpf_test_data!("mod-by-zero-reg.data"))]
    #[test_case(ubpf_test_data!("mod.data"))]
    #[test_case(ubpf_test_data!("mov64-sign-extend.data"))]
    #[test_case(ubpf_test_data!("mov.data"))]
    #[test_case(ubpf_test_data!("mul32-imm.data"))]
    #[test_case(ubpf_test_data!("mul32-reg.data"))]
    #[test_case(ubpf_test_data!("mul32-reg-overflow.data"))]
    #[test_case(ubpf_test_data!("mul64-imm.data"))]
    #[test_case(ubpf_test_data!("mul64-reg.data"))]
    #[test_case(ubpf_test_data!("mul-loop.data"))]
    #[test_case(ubpf_test_data!("neg64.data"))]
    #[test_case(ubpf_test_data!("neg.data"))]
    #[test_case(ubpf_test_data!("prime.data"))]
    #[test_case(ubpf_test_data!("rsh32.data"))]
    #[test_case(ubpf_test_data!("rsh-reg.data"))]
    #[test_case(ubpf_test_data!("stack2.data"))]
    #[test_case(ubpf_test_data!("stack3.data"))]
    #[test_case(ubpf_test_data!("stack.data"))]
    #[test_case(ubpf_test_data!("stb.data"))]
    #[test_case(ubpf_test_data!("st.data"))]
    #[test_case(ubpf_test_data!("stdw.data"))]
    #[test_case(ubpf_test_data!("sth.data"))]
    #[test_case(ubpf_test_data!("string-stack.data"))]
    #[test_case(ubpf_test_data!("stw.data"))]
    #[test_case(ubpf_test_data!("stxb-all2.data"))]
    #[test_case(ubpf_test_data!("stxb-all.data"))]
    #[test_case(ubpf_test_data!("stxb-chain.data"))]
    #[test_case(ubpf_test_data!("stxb.data"))]
    #[test_case(ubpf_test_data!("stx.data"))]
    #[test_case(ubpf_test_data!("stxdw.data"))]
    #[test_case(ubpf_test_data!("stxh.data"))]
    #[test_case(ubpf_test_data!("stxw.data"))]
    #[test_case(ubpf_test_data!("subnet.data"))]
    #[test_case(local_test_data!("err_offset_overflow.data"))]
    #[test_case(local_test_data!("err_read_only_helper.data"))]
    #[test_case(local_test_data!("err_write_r10.data"))]
    #[test_case(local_test_data!("exponential_verification.data"))]
    #[test_case(local_test_data!("forget_release.data"))]
    #[test_case(local_test_data!("malloc_double_free.data"))]
    #[test_case(local_test_data!("malloc_use_free.data"))]
    #[test_case(local_test_data!("null_checks_propagated.data"))]
    #[test_case(local_test_data!("packet_access.data"))]
    #[test_case(local_test_data!("read_only_helper.data"))]
    #[test_case(local_test_data!("stack_access.data"))]
    #[test_case(local_test_data!("write_only_helper.data"))]
    fn test_ebpf_conformance(content: &str) {
        let Some(mut test_case) = TestCase::parse(content) else {
            // Special case that only test the test framework.
            return;
        };

        let test_memory = TestBuffer::new(&mut test_case.memory);
        let args = vec![test_memory.get_value_type(), Type::from(test_memory.size as u64)];
        let packet_type = test_case.memory.is_some().then_some(test_memory.get_value_type());

        let malloc_id = MemoryId::new();
        let mut helpers = HashMap::<u32, FunctionSignature>::new();
        let mut helper_impls = HashMap::<u32, EbpfHelperImpl<TestEbpfProgramContext>>::new();
        let mut add_helper = |id, signature, impl_| {
            helpers.insert(id, signature);
            helper_impls.insert(id, EbpfHelperImpl(impl_));
        };

        add_helper(
            0,
            FunctionSignature {
                args: vec![
                    Type::ScalarValueParameter,
                    Type::ScalarValueParameter,
                    Type::ScalarValueParameter,
                    Type::ScalarValueParameter,
                    Type::ScalarValueParameter,
                ],
                return_value: Type::UNKNOWN_SCALAR,
                invalidate_array_bounds: false,
            },
            gather_bytes,
        );
        add_helper(
            1,
            FunctionSignature {
                args: vec![
                    Type::MemoryParameter {
                        size: MemoryParameterSize::Reference { index: 1 },
                        input: true,
                        output: true,
                    },
                    Type::ScalarValueParameter,
                ],
                return_value: Type::AliasParameter { parameter_index: 0 },
                invalidate_array_bounds: false,
            },
            memfrob,
        );
        add_helper(
            2,
            FunctionSignature {
                args: vec![],
                return_value: Type::UNKNOWN_SCALAR,
                invalidate_array_bounds: false,
            },
            trash_registers,
        );
        add_helper(
            3,
            FunctionSignature {
                args: vec![Type::ScalarValueParameter],
                return_value: Type::UNKNOWN_SCALAR,
                invalidate_array_bounds: false,
            },
            sqrti,
        );
        add_helper(
            4,
            FunctionSignature {
                // Args cannot be correctly verified as the verifier cannot check the string
                // are correctly 0 terminated.
                args: vec![],
                return_value: Type::UNKNOWN_SCALAR,
                invalidate_array_bounds: false,
            },
            strcmp_ext,
        );
        add_helper(
            100,
            FunctionSignature {
                args: vec![Type::ScalarValueParameter],
                return_value: Type::NullOrParameter(Box::new(Type::UNKNOWN_SCALAR)),
                invalidate_array_bounds: false,
            },
            null_or,
        );
        add_helper(
            101,
            FunctionSignature {
                args: vec![Type::MemoryParameter {
                    size: MemoryParameterSize::Value(8),
                    input: true,
                    output: false,
                }],
                return_value: Type::UNKNOWN_SCALAR,
                invalidate_array_bounds: false,
            },
            read_only,
        );
        add_helper(
            102,
            FunctionSignature {
                args: vec![
                    Type::MemoryParameter {
                        size: MemoryParameterSize::Value(8),
                        input: false,
                        output: true,
                    },
                    Type::ScalarValueParameter,
                ],
                return_value: Type::UNKNOWN_SCALAR,
                invalidate_array_bounds: false,
            },
            write_only,
        );
        add_helper(
            103,
            FunctionSignature {
                args: vec![Type::ScalarValueParameter],
                return_value: Type::NullOrParameter(Box::new(Type::ReleasableParameter {
                    id: malloc_id.clone(),
                    inner: Box::new(Type::MemoryParameter {
                        size: MemoryParameterSize::Reference { index: 0 },
                        input: true,
                        output: true,
                    }),
                })),
                invalidate_array_bounds: false,
            },
            malloc,
        );
        add_helper(
            104,
            FunctionSignature {
                args: vec![Type::ReleaseParameter { id: malloc_id.clone() }],
                return_value: Type::UNKNOWN_SCALAR,
                invalidate_array_bounds: false,
            },
            free,
        );

        let verified_program = verify_program(
            test_case.code,
            CallingContext { maps: vec![], helpers, args, packet_type },
            &mut NullVerifierLogger,
        );

        if let Some(value) = test_case.result {
            let verified_program = verified_program.expect("program must be loadable");
            let program = link_program_dynamic::<TestEbpfProgramContext>(
                &verified_program,
                &[],
                vec![],
                helper_impls,
            )
            .expect("failed to link a test program");

            let mut context = TestEbpfRunContext { buffer_size: test_memory.size };
            let result = program.run_with_2_arguments(&mut context, test_memory, test_memory.size);
            assert_eq!(result, value);
        } else {
            assert!(verified_program.is_err());
        }
    }
}
