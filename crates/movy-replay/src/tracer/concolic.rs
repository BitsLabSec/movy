use std::{cmp::Ordering, collections::BTreeMap, str::FromStr};

use log::{trace, warn};
use move_binary_format::file_format::Bytecode;
use move_core_types::{language_storage::TypeTag, u256::U256};
use move_trace_format::format::{Effect, ExtraInstructionInformation, TraceEvent, TypeTagWithRefs};
use move_vm_stack::Stack;
use move_vm_types::values::{Reference, VMValueCast, Value};
use z3::ast::{Ast, Bool, Int};

#[derive(Clone, Debug, PartialEq, Eq)]
enum PrimitiveValue {
    Bool(bool),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    U128(u128),
    U256(U256),
}

fn try_value_as<T>(v: &Value) -> Option<T>
where
    Value: VMValueCast<T>,
{
    v.copy_value().ok()?.value_as::<T>().ok()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SymbolValue {
    Value(Int),
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ConcolicState {
    pub stack: Vec<SymbolValue>,
    pub locals: Vec<Vec<SymbolValue>>,
    pub args: Vec<BTreeMap<usize, Int>>,
    pub disable: bool,
}

impl PrimitiveValue {
    fn bitwidth(&self) -> u32 {
        match self {
            PrimitiveValue::Bool(_) => 1,
            PrimitiveValue::U8(_) => 8,
            PrimitiveValue::U16(_) => 16,
            PrimitiveValue::U32(_) => 32,
            PrimitiveValue::U64(_) => 64,
            PrimitiveValue::U128(_) => 128,
            PrimitiveValue::U256(_) => 256,
        }
    }

    fn as_u256(&self) -> U256 {
        match self {
            PrimitiveValue::Bool(b) => {
                if *b {
                    U256::one()
                } else {
                    U256::zero()
                }
            }
            PrimitiveValue::U8(u) => U256::from(*u),
            PrimitiveValue::U16(u) => U256::from(*u),
            PrimitiveValue::U32(u) => U256::from(*u),
            PrimitiveValue::U64(u) => U256::from(*u),
            PrimitiveValue::U128(u) => U256::from(*u),
            PrimitiveValue::U256(u) => *u,
        }
    }
}

fn extract_primitive_value(v: &Value) -> PrimitiveValue {
    if let Some(reference) = try_value_as::<Reference>(v) {
        let inner = reference
            .read_ref()
            .expect("failed to read reference for comparison");
        return extract_primitive_value(&inner);
    }

    if let Some(b) = try_value_as::<bool>(v) {
        return PrimitiveValue::Bool(b);
    }
    if let Some(u) = try_value_as::<u8>(v) {
        return PrimitiveValue::U8(u);
    }
    if let Some(u) = try_value_as::<u16>(v) {
        return PrimitiveValue::U16(u);
    }
    if let Some(u) = try_value_as::<u32>(v) {
        return PrimitiveValue::U32(u);
    }
    if let Some(u) = try_value_as::<u64>(v) {
        return PrimitiveValue::U64(u);
    }
    if let Some(u) = try_value_as::<u128>(v) {
        return PrimitiveValue::U128(u);
    }
    if let Some(u) = try_value_as::<U256>(v) {
        return PrimitiveValue::U256(u);
    }

    panic!("Unsupported value type {:?} for comparison", v);
}

fn compare_value_impl(v1: &PrimitiveValue, v2: &PrimitiveValue) -> Ordering {
    match (v1, v2) {
        (PrimitiveValue::Bool(l), PrimitiveValue::Bool(r)) => l.cmp(r),
        (PrimitiveValue::U8(l), PrimitiveValue::U8(r)) => l.cmp(r),
        (PrimitiveValue::U16(l), PrimitiveValue::U16(r)) => l.cmp(r),
        (PrimitiveValue::U32(l), PrimitiveValue::U32(r)) => l.cmp(r),
        (PrimitiveValue::U64(l), PrimitiveValue::U64(r)) => l.cmp(r),
        (PrimitiveValue::U128(l), PrimitiveValue::U128(r)) => l.cmp(r),
        (PrimitiveValue::U256(l), PrimitiveValue::U256(r)) => l.cmp(r),
        _ => panic!(
            "Unsupported value type {:?} and {:?} for comparison",
            v1, v2
        ),
    }
}

pub fn compare_value(v1: &Value, v2: &Value) -> Ordering {
    let p1 = extract_primitive_value(v1);
    let p2 = extract_primitive_value(v2);
    compare_value_impl(&p1, &p2)
}

pub fn value_to_u256(v: &Value) -> U256 {
    extract_primitive_value(v).as_u256()
}

pub fn value_bitwidth(v: &Value) -> u32 {
    extract_primitive_value(v).bitwidth()
}

fn int_two_pow(bits: u32) -> Int {
    let v = U256::one() << bits;
    Int::from_str(&v.to_string()).unwrap()
}

fn int_mod_2n(x: &Int, bits: u32) -> Int {
    x.modulo(int_two_pow(bits))
}

/// Convert U256 to Int numeral.
fn int_from_u256(u: U256) -> Int {
    Int::from_str(&u.to_string()).unwrap()
}

/// Integer-only AND with a constant bitmask:
/// Returns (x & mask) under w-bit semantics, using only Int + div/mod by powers of two.
/// Implementation uses "run decomposition": split mask's 1-bits into contiguous runs [a..=b],
/// and for each run extract that window from x, then place it back.
pub fn int_bvand_const(x: &Int, mask: U256, bits: u32) -> Int {
    // Normalize x to w-bit domain (BitVec semantics).
    let x0 = int_mod_2n(x, bits);

    // Quick exits.
    if mask == U256::zero() {
        return Int::from_u64(0);
    }
    // Restrict mask to w bits (BV mask)
    let mask_w = mask & ((U256::one() << bits) - U256::one());

    // Iterate over runs of 1s in mask_w.
    let mut m = mask_w;
    let mut i: u32 = 0;
    let mut terms: Vec<Int> = Vec::new();

    while m != U256::zero() {
        // skip zeros
        while m != U256::zero() && (m & U256::one()) == U256::zero() {
            m = m.checked_shr(1).unwrap();
            i += 1;
        }
        if m == U256::zero() {
            break;
        }
        // start of a run
        let a = i;
        // consume ones
        while (m & U256::one()) == U256::one() {
            m = m.checked_shr(1).unwrap();
            i += 1;
        }
        let b = i - 1; // inclusive end
        let L = b - a + 1;

        // term(a,b) = (((x0 mod 2^(b+1)) div 2^a) mod 2^L) * 2^a
        let term =
            (x0.clone() % int_two_pow(b + 1) / int_two_pow(a)) % int_two_pow(L) * int_two_pow(a);
        terms.push(term);
    }

    // Sum all terms (if no runs, it's zero which we handled above).
    let mut acc = Int::from_u64(0);
    for t in terms {
        acc += t;
    }
    acc
}

/// Integer-only OR with a constant bitmask:
/// Returns (x | mask) under w-bit semantics, using only Int ops.
/// Uses identity: x | M = (x & ~M_w) + M_w, where ~M_w is bitwise-not of M within w bits.
/// We reuse int_bvand_const for the "clear then add" pattern.
pub fn int_bvor_const(x: &Int, mask: U256, bits: u32) -> Int {
    // mask limited to w bits
    let full = (U256::one() << bits) - U256::one();
    let mask_w = mask & full;
    let not_mask_w = full ^ mask_w;

    // Keep x's bits where mask is 0, then force-on mask bits by addition.
    let kept = int_bvand_const(x, not_mask_w, bits);
    kept + int_from_u256(mask_w)
}

/// Integer-only NOT under w-bit semantics:
/// r = ~x  (within w bits)  ==  (2^w - 1) - (x mod 2^w)
pub fn int_bvnot(x: &Int, bits: u32) -> Int {
    let x0 = int_mod_2n(x, bits);
    let full = int_two_pow(bits) - 1;
    full - x0
}

/// Integer-only XOR with a constant mask under w-bit semantics:
/// r = x ^ mask = (x & ~mask_w) + (~x & mask_w)
pub fn int_bvxor_const(x: &Int, mask: U256, bits: u32) -> Int {
    let full = (U256::one() << bits) - U256::one();
    let mask_w = mask & full;
    let not_mask_w = full ^ mask_w;

    let part_keep = int_bvand_const(x, not_mask_w, bits);
    let x_not = int_bvnot(x, bits);
    let part_flip = int_bvand_const(&x_not, mask_w, bits);

    // Disjoint bit regions; sum is exact. Normalize just in case.
    let sum = part_keep + part_flip;
    int_mod_2n(&sum, bits)
}

impl ConcolicState {
    pub fn new() -> Self {
        Self {
            stack: Vec::new(),
            locals: Vec::new(),
            args: Vec::new(),
            disable: false,
        }
    }

    #[inline]
    fn max_u_bits(n: u32) -> Int {
        if n <= 63 {
            Int::from_u64((1u64 << n) - 1)
        } else {
            let two_pow_n_minus_1 = match n {
                64 => "18446744073709551615",
                128 => "340282366920938463463374607431768211455",
                256 => {
                    "115792089237316195423570985008687907853269984665640564039457584007913129639935"
                }
                _ => unreachable!("add more cases or compute big ints as needed"),
            };
            Int::from_str(two_pow_n_minus_1).unwrap()
        }
    }

    fn resolve_arg(cmd_index: usize, param_index: usize, ty: &TypeTagWithRefs) -> SymbolValue {
        if ty.ref_type.is_some() {
            return SymbolValue::Unknown;
        }
        match ty.type_ {
            TypeTag::Bool
            | TypeTag::U8
            | TypeTag::U16
            | TypeTag::U32
            | TypeTag::U64
            | TypeTag::U128
            | TypeTag::U256 => {
                let int = Int::new_const(format!("{}.{}", cmd_index, param_index));
                SymbolValue::Value(int)
            }
            _ => SymbolValue::Unknown,
        }
    }

    fn resolve_value(value: &Value) -> Int {
        match extract_primitive_value(value) {
            PrimitiveValue::Bool(b) => {
                let int_val = if b { 1 } else { 0 };
                Int::from_u64(int_val)
            }
            PrimitiveValue::U8(u) => Int::from_u64(u as u64),
            PrimitiveValue::U16(u) => Int::from_u64(u as u64),
            PrimitiveValue::U32(u) => Int::from_u64(u as u64),
            PrimitiveValue::U64(u) => Int::from_u64(u),
            PrimitiveValue::U128(u) => Int::from_str(&u.to_string()).unwrap(),
            PrimitiveValue::U256(u) => Int::from_str(&u.to_string()).unwrap(),
        }
    }

    pub fn notify_event(&mut self, event: &TraceEvent, stack: Option<&Stack>) -> Option<Bool> {
        if self.disable {
            return None;
        }
        if let Some(s) = stack {
            if self.stack.len() != s.value.len() && s.value.is_empty() {
                self.stack.clear();
            }
            if let TraceEvent::Effect(v) = event
                && let Effect::ExecutionError(_) = v.as_ref()
            {
                self.stack.pop();
            }
            assert_eq!(
                self.stack.len(),
                s.value.len(),
                "stack: {:?}, stack from trace: {:?}, event: {:?}",
                self.stack,
                s.value,
                event
            );
        } else {
            trace!("No stack available for event: {:?}", event);
        }

        let mut process_binary_op = || {
            let (rhs, lhs) = (self.stack.pop().unwrap(), self.stack.pop().unwrap());
            let mut stack_iter = stack.unwrap().last_n(2).unwrap();
            let true_lhs = stack_iter.next().unwrap();
            let true_rhs = stack_iter.next().unwrap();
            let (new_l, new_r) = match (lhs, rhs) {
                (SymbolValue::Value(l), SymbolValue::Value(r)) => (l, r),
                (SymbolValue::Value(l), SymbolValue::Unknown) => {
                    let new_r = Self::resolve_value(true_rhs);
                    (l, new_r)
                }
                (SymbolValue::Unknown, SymbolValue::Value(r)) => {
                    let new_l = Self::resolve_value(true_lhs);
                    (new_l, r)
                }
                (SymbolValue::Unknown, SymbolValue::Unknown) => {
                    return None;
                }
            };
            Some((new_l, new_r))
        };

        match event {
            TraceEvent::External(v) => {
                trace!("External event: {:?}", v);
                if v.as_str().is_some_and(|s| s == "MoveCallStart") {
                    self.locals.clear();
                    self.stack.clear();
                }
            }
            TraceEvent::OpenFrame { frame, gas_left: _ } => {
                trace!("Open frame: {:?}", frame);
                trace!("Current stack: {:?}", stack.map(|s| &s.value));
                let param_count = frame.parameters.len();
                if self.locals.is_empty() {
                    let mut locals = if frame.locals_types.is_empty() {
                        vec![SymbolValue::Unknown; param_count]
                    } else {
                        frame
                            .locals_types
                            .iter()
                            .enumerate()
                            .map(|(i, ty)| Self::resolve_arg(self.args.len(), i, ty))
                            .collect::<Vec<_>>()
                    };
                    if locals.len() < param_count {
                        locals.resize(param_count, SymbolValue::Unknown);
                    }
                    self.args.push(
                        locals
                            .iter()
                            .take(param_count)
                            .enumerate()
                            .filter_map(|(i, v)| match v {
                                SymbolValue::Value(bv) => Some((i, bv.clone())),
                                SymbolValue::Unknown => None,
                            })
                            .collect(),
                    );
                    self.locals.push(locals);
                    trace!("args: {:?}", self.args);
                } else {
                    // TODO: need input_unresolved_tys and return_unresolved_tys
                    let arg_len = param_count.min(self.stack.len());
                    let skip_idx = self.stack.len() - arg_len;
                    let mut locals: Vec<_> = self.stack.drain(skip_idx..).collect();
                    self.stack.truncate(skip_idx);
                    if frame.locals_types.len() > locals.len() {
                        locals.extend(std::iter::repeat_n(
                            SymbolValue::Unknown,
                            frame.locals_types.len() - locals.len(),
                        ));
                    }
                    self.locals.push(locals);
                    if frame.is_native {
                        for _ in 0..frame.return_types.len() {
                            self.stack.push(SymbolValue::Unknown);
                        }
                    }
                }
            }
            TraceEvent::CloseFrame {
                frame_id: _,
                return_: _,
                gas_left: _,
            } => {
                trace!("Close frame. Current stack: {:?}", stack.map(|s| &s.value));
                self.locals.pop();
            }
            TraceEvent::BeforeInstruction {
                type_parameters: _,
                pc,
                gas_left: _,
                instruction,
                extra,
            } => {
                match instruction {
                    Bytecode::Pop
                    | Bytecode::BrTrue(_)
                    | Bytecode::BrFalse(_)
                    | Bytecode::Abort
                    | Bytecode::VecImmBorrow(_)
                    | Bytecode::VecMutBorrow(_) => {
                        self.stack.pop();
                    }
                    Bytecode::LdU8(_)
                    | Bytecode::LdU16(_)
                    | Bytecode::LdU32(_)
                    | Bytecode::LdU64(_)
                    | Bytecode::LdU128(_)
                    | Bytecode::LdU256(_)
                    | Bytecode::LdConst(_) => {
                        self.stack.push(SymbolValue::Unknown);
                    }
                    Bytecode::LdFalse => {
                        self.stack.push(SymbolValue::Value(Int::from_u64(0)));
                    }
                    Bytecode::LdTrue => {
                        self.stack.push(SymbolValue::Value(Int::from_u64(1)));
                    }
                    Bytecode::CastU8 => {
                        if let Some(v) = self.stack.last() {
                            if let SymbolValue::Value(int) = v {
                                return Some(int.le(Self::max_u_bits(8)));
                            }
                        } else {
                            warn!("Stack underflow at pc {}", pc);
                        }
                    }
                    Bytecode::CastU16 => {
                        if let Some(v) = self.stack.last() {
                            if let SymbolValue::Value(int) = v {
                                return Some(int.le(Self::max_u_bits(16)));
                            }
                        } else {
                            warn!("Stack underflow at pc {}", pc);
                        }
                    }
                    Bytecode::CastU32 => {
                        if let Some(v) = self.stack.last() {
                            if let SymbolValue::Value(int) = v {
                                return Some(int.le(Self::max_u_bits(32)));
                            }
                        } else {
                            warn!("Stack underflow at pc {}", pc);
                        }
                    }
                    Bytecode::CastU64 => {
                        if let Some(v) = self.stack.last() {
                            if let SymbolValue::Value(int) = v {
                                return Some(int.le(Self::max_u_bits(64)));
                            }
                        } else {
                            warn!("Stack underflow at pc {}", pc);
                        }
                    }
                    Bytecode::CastU128 => {
                        if let Some(v) = self.stack.last() {
                            if let SymbolValue::Value(int) = v {
                                return Some(int.le(Self::max_u_bits(128)));
                            }
                        } else {
                            warn!("Stack underflow at pc {}", pc);
                        }
                    }
                    Bytecode::Add => {
                        if let Some((l, r)) = process_binary_op() {
                            // overflow check not implemented yet
                            let sum = l + r;
                            self.stack.push(SymbolValue::Value(sum));
                        } else {
                            self.stack.push(SymbolValue::Unknown);
                        }
                    }
                    Bytecode::Sub => {
                        if let Some((l, r)) = process_binary_op() {
                            // overflow check not implemented yet
                            let diff = l - r;
                            self.stack.push(SymbolValue::Value(diff));
                        } else {
                            self.stack.push(SymbolValue::Unknown);
                        }
                    }
                    Bytecode::Mul => {
                        if let Some((l, r)) = process_binary_op() {
                            // overflow check not implemented yet
                            let prod = l * r;
                            self.stack.push(SymbolValue::Value(prod));
                        } else {
                            self.stack.push(SymbolValue::Unknown);
                        }
                    }
                    Bytecode::Div => {
                        if let Some((l, r)) = process_binary_op() {
                            // overflow check not implemented yet
                            let quot = l / r;
                            self.stack.push(SymbolValue::Value(quot));
                        } else {
                            self.stack.push(SymbolValue::Unknown);
                        }
                    }
                    Bytecode::Mod => {
                        if let Some((l, r)) = process_binary_op() {
                            // overflow check not implemented yet
                            let rem = l % r;
                            self.stack.push(SymbolValue::Value(rem));
                        } else {
                            self.stack.push(SymbolValue::Unknown);
                        }
                    }
                    Bytecode::And | Bytecode::BitAnd => {
                        let (rhs, lhs) = (self.stack.pop().unwrap(), self.stack.pop().unwrap());
                        let mut stack_iter = stack.unwrap().last_n(2).unwrap();
                        let true_lhs = stack_iter.next().unwrap();
                        let true_rhs = stack_iter.next().unwrap();

                        let bit_width = value_bitwidth(true_lhs);
                        let (true_l, true_r) = (value_to_u256(true_lhs), value_to_u256(true_rhs));
                        match (lhs, rhs) {
                            (SymbolValue::Value(l), SymbolValue::Value(r)) => {
                                self.stack.push(SymbolValue::Unknown);
                            }
                            (SymbolValue::Value(l), SymbolValue::Unknown) => {
                                let and = int_bvand_const(&l, true_r, bit_width);
                                self.stack.push(SymbolValue::Value(and));
                            }
                            (SymbolValue::Unknown, SymbolValue::Value(r)) => {
                                let and = int_bvand_const(&r, true_l, bit_width);
                                self.stack.push(SymbolValue::Value(and));
                            }
                            (SymbolValue::Unknown, SymbolValue::Unknown) => {
                                self.stack.push(SymbolValue::Unknown);
                            }
                        }
                    }
                    Bytecode::Or | Bytecode::BitOr => {
                        let (rhs, lhs) = (self.stack.pop().unwrap(), self.stack.pop().unwrap());
                        let mut stack_iter = stack.unwrap().last_n(2).unwrap();
                        let true_lhs = stack_iter.next().unwrap();
                        let true_rhs = stack_iter.next().unwrap();

                        let bit_width = value_bitwidth(true_lhs);
                        let (true_l, true_r) = (value_to_u256(true_lhs), value_to_u256(true_rhs));
                        match (lhs, rhs) {
                            (SymbolValue::Value(l), SymbolValue::Value(r)) => {
                                self.stack.push(SymbolValue::Unknown);
                            }
                            (SymbolValue::Value(l), SymbolValue::Unknown) => {
                                let or = int_bvor_const(&l, true_r, bit_width);
                                self.stack.push(SymbolValue::Value(or));
                            }
                            (SymbolValue::Unknown, SymbolValue::Value(r)) => {
                                let or = int_bvor_const(&r, true_l, bit_width);
                                self.stack.push(SymbolValue::Value(or));
                            }
                            (SymbolValue::Unknown, SymbolValue::Unknown) => {
                                self.stack.push(SymbolValue::Unknown);
                            }
                        }
                    }
                    Bytecode::Xor => {
                        let (rhs, lhs) = (self.stack.pop().unwrap(), self.stack.pop().unwrap());
                        let mut stack_iter = stack.unwrap().last_n(2).unwrap();
                        let true_lhs = stack_iter.next().unwrap();
                        let true_rhs = stack_iter.next().unwrap();

                        let bit_width = value_bitwidth(true_lhs);
                        let (true_l, true_r) = (value_to_u256(true_lhs), value_to_u256(true_rhs));
                        match (lhs, rhs) {
                            (SymbolValue::Value(l), SymbolValue::Value(r)) => {
                                self.stack.push(SymbolValue::Unknown);
                            }
                            (SymbolValue::Value(l), SymbolValue::Unknown) => {
                                let xor = int_bvxor_const(&l, true_r, bit_width);
                                self.stack.push(SymbolValue::Value(xor));
                            }
                            (SymbolValue::Unknown, SymbolValue::Value(r)) => {
                                let xor = int_bvxor_const(&r, true_l, bit_width);
                                self.stack.push(SymbolValue::Value(xor));
                            }
                            (SymbolValue::Unknown, SymbolValue::Unknown) => {
                                self.stack.push(SymbolValue::Unknown);
                            }
                        }
                    }
                    Bytecode::Shl => {
                        let (rhs, lhs) = (self.stack.pop().unwrap(), self.stack.pop().unwrap());
                        let mut stack_iter = stack.unwrap().last_n(2).unwrap();
                        let true_lhs = stack_iter.next().unwrap();
                        let true_rhs = stack_iter.next().unwrap();
                        let bit_width = value_bitwidth(true_lhs);
                        let true_r = value_to_u256(true_rhs).unchecked_as_u32();
                        let threshold = Self::max_u_bits(bit_width);
                        match (lhs, rhs) {
                            (SymbolValue::Value(l), SymbolValue::Value(r)) => {
                                self.stack.push(SymbolValue::Unknown);
                            }
                            (SymbolValue::Value(l), SymbolValue::Unknown) => {
                                let shl = l * int_two_pow(true_r);
                                let shl_mod = shl.modulo(int_two_pow(bit_width));
                                self.stack.push(SymbolValue::Value(shl_mod));
                                return Some(shl.gt(&threshold)); // cause overflow
                            }
                            (SymbolValue::Unknown, SymbolValue::Value(r)) => {
                                self.stack.push(SymbolValue::Unknown);
                            }
                            (SymbolValue::Unknown, SymbolValue::Unknown) => {
                                self.stack.push(SymbolValue::Unknown);
                            }
                        }
                    }
                    Bytecode::Shr => {
                        let (rhs, lhs) = (self.stack.pop().unwrap(), self.stack.pop().unwrap());
                        let mut stack_iter = stack.unwrap().last_n(2).unwrap();
                        let true_lhs = stack_iter.next().unwrap();
                        let true_rhs = stack_iter.next().unwrap();

                        let true_r = value_to_u256(true_rhs).unchecked_as_u32();
                        match (lhs, rhs) {
                            (SymbolValue::Value(l), SymbolValue::Value(r)) => {
                                self.stack.push(SymbolValue::Unknown);
                            }
                            (SymbolValue::Value(l), SymbolValue::Unknown) => {
                                let shr = l / int_two_pow(true_r);
                                self.stack.push(SymbolValue::Value(shr));
                            }
                            (SymbolValue::Unknown, SymbolValue::Value(r)) => {
                                self.stack.push(SymbolValue::Unknown);
                            }
                            (SymbolValue::Unknown, SymbolValue::Unknown) => {
                                self.stack.push(SymbolValue::Unknown);
                            }
                        }
                    }
                    Bytecode::Not => {
                        if let Some(v) = self.stack.pop() {
                            match v {
                                SymbolValue::Value(n) => {
                                    let bit_width = value_bitwidth(
                                        stack.unwrap().last_n(1).unwrap().next().unwrap(),
                                    );
                                    let not_n = int_bvnot(&n, bit_width);
                                    self.stack.push(SymbolValue::Value(not_n));
                                }
                                SymbolValue::Unknown => {
                                    self.stack.push(SymbolValue::Unknown);
                                }
                            }
                        } else {
                            warn!("Stack underflow at pc {}", pc);
                        }
                    }
                    Bytecode::CopyLoc(idx)
                    | Bytecode::MutBorrowLoc(idx)
                    | Bytecode::ImmBorrowLoc(idx) => {
                        if let Some(locals) = self.locals.last() {
                            if let Some(v) = locals.get(*idx as usize) {
                                self.stack.push(v.clone());
                            } else {
                                warn!("Local index out of bounds at pc {}", pc);
                                self.stack.push(SymbolValue::Unknown);
                            }
                        } else {
                            warn!("No locals available at pc {}", pc);
                            self.stack.push(SymbolValue::Unknown);
                        }
                    }
                    Bytecode::MoveLoc(idx) => {
                        if let Some(locals) = self.locals.last_mut() {
                            if let Some(v) = locals.get(*idx as usize) {
                                self.stack.push(v.clone());
                                locals[*idx as usize] = SymbolValue::Unknown; // moved-from
                            } else {
                                warn!("Local index out of bounds at pc {}", pc);
                                self.stack.push(SymbolValue::Unknown);
                            }
                        } else {
                            warn!("No locals available at pc {}", pc);
                            self.stack.push(SymbolValue::Unknown);
                        }
                    }
                    Bytecode::StLoc(idx) => {
                        if let Some(v) = self.stack.pop() {
                            if let Some(locals) = self.locals.last_mut() {
                                if let Some(slot) = locals.get_mut(*idx as usize) {
                                    *slot = v;
                                } else {
                                    for _ in locals.len()..=*idx as usize {
                                        locals.push(SymbolValue::Unknown);
                                    }
                                    locals[*idx as usize] = v;
                                }
                            } else {
                                warn!("No locals available at pc {}", pc);
                            }
                        } else {
                            warn!("Stack underflow at pc {}", pc);
                        }
                    }
                    Bytecode::WriteRef | Bytecode::VecPushBack(_) => {
                        self.stack.pop();
                        self.stack.pop();
                    }
                    Bytecode::Eq => {
                        let (rhs, lhs) = (self.stack.pop().unwrap(), self.stack.pop().unwrap());
                        let mut stack_iter = stack.unwrap().last_n(2).unwrap();
                        let true_lhs = stack_iter.next().unwrap();
                        let true_rhs = stack_iter.next().unwrap();
                        let (new_l, new_r) = match (lhs, rhs) {
                            (SymbolValue::Value(l), SymbolValue::Value(r)) => (l, r),
                            (SymbolValue::Value(l), SymbolValue::Unknown) => {
                                let new_r = Self::resolve_value(true_rhs);
                                (l, new_r)
                            }
                            (SymbolValue::Unknown, SymbolValue::Value(r)) => {
                                let new_l = Self::resolve_value(true_lhs);
                                (new_l, r)
                            }
                            (SymbolValue::Unknown, SymbolValue::Unknown) => {
                                self.stack.push(SymbolValue::Unknown);
                                return None;
                            }
                        };
                        if matches!(compare_value(true_lhs, true_rhs), Ordering::Equal) {
                            let eq = new_l._eq(&new_r);
                            let int = eq.ite(&Int::from_u64(1), &Int::from_u64(0));
                            self.stack.push(SymbolValue::Value(int));
                            return Some(eq);
                        } else {
                            // different values are not equal
                            let neq = new_l._eq(&new_r).not();
                            let int = neq.ite(&Int::from_u64(1), &Int::from_u64(0));
                            self.stack.push(SymbolValue::Value(int));
                            return Some(neq);
                        }
                    }
                    Bytecode::Neq => {
                        let (rhs, lhs) = (self.stack.pop().unwrap(), self.stack.pop().unwrap());
                        let mut stack_iter = stack.unwrap().last_n(2).unwrap();
                        let true_lhs = stack_iter.next().unwrap();
                        let true_rhs = stack_iter.next().unwrap();
                        let (new_l, new_r) = match (lhs, rhs) {
                            (SymbolValue::Value(l), SymbolValue::Value(r)) => (l, r),
                            (SymbolValue::Value(l), SymbolValue::Unknown) => {
                                let new_r = Self::resolve_value(true_rhs);
                                (l, new_r)
                            }
                            (SymbolValue::Unknown, SymbolValue::Value(r)) => {
                                let new_l = Self::resolve_value(true_lhs);
                                (new_l, r)
                            }
                            (SymbolValue::Unknown, SymbolValue::Unknown) => {
                                self.stack.push(SymbolValue::Unknown);
                                return None;
                            }
                        };
                        if !matches!(compare_value(true_lhs, true_rhs), Ordering::Equal) {
                            let neq = new_l._eq(&new_r).not();
                            let bv = neq.ite(&Int::from_u64(1), &Int::from_u64(0));
                            self.stack.push(SymbolValue::Value(bv));
                            return Some(neq);
                        } else {
                            // same values are equal
                            let eq = new_l._eq(&new_r);
                            let bv = eq.ite(&Int::from_u64(1), &Int::from_u64(0));
                            self.stack.push(SymbolValue::Value(bv));
                            return Some(eq.not());
                        }
                    }
                    Bytecode::Lt => {
                        let (rhs, lhs) = (self.stack.pop().unwrap(), self.stack.pop().unwrap());
                        let mut stack_iter = stack.unwrap().last_n(2).unwrap();
                        let true_lhs = stack_iter.next().unwrap();
                        let true_rhs = stack_iter.next().unwrap();
                        let (new_l, new_r) = match (lhs, rhs) {
                            (SymbolValue::Value(l), SymbolValue::Value(r)) => (l, r),
                            (SymbolValue::Value(l), SymbolValue::Unknown) => {
                                let new_r = Self::resolve_value(true_rhs);
                                (l, new_r)
                            }
                            (SymbolValue::Unknown, SymbolValue::Value(r)) => {
                                let new_l = Self::resolve_value(true_lhs);
                                (new_l, r)
                            }
                            (SymbolValue::Unknown, SymbolValue::Unknown) => {
                                self.stack.push(SymbolValue::Unknown);
                                return None;
                            }
                        };
                        if matches!(compare_value(true_lhs, true_rhs), Ordering::Less) {
                            let lt = new_l.lt(&new_r);
                            let int = lt.ite(&Int::from_u64(1), &Int::from_u64(0));
                            self.stack.push(SymbolValue::Value(int));
                            return Some(lt);
                        } else {
                            // not less than
                            let nlt = new_l.lt(&new_r).not();
                            let int = nlt.ite(&Int::from_u64(1), &Int::from_u64(0));
                            self.stack.push(SymbolValue::Value(int));
                            return Some(nlt);
                        }
                    }
                    Bytecode::Le => {
                        let (rhs, lhs) = (self.stack.pop().unwrap(), self.stack.pop().unwrap());
                        let mut stack_iter = stack.unwrap().last_n(2).unwrap();
                        let true_lhs = stack_iter.next().unwrap();
                        let true_rhs = stack_iter.next().unwrap();
                        let (new_l, new_r) = match (lhs, rhs) {
                            (SymbolValue::Value(l), SymbolValue::Value(r)) => (l, r),
                            (SymbolValue::Value(l), SymbolValue::Unknown) => {
                                let new_r = Self::resolve_value(true_rhs);
                                (l, new_r)
                            }
                            (SymbolValue::Unknown, SymbolValue::Value(r)) => {
                                let new_l = Self::resolve_value(true_lhs);
                                (new_l, r)
                            }
                            (SymbolValue::Unknown, SymbolValue::Unknown) => {
                                self.stack.push(SymbolValue::Unknown);
                                return None;
                            }
                        };
                        if !matches!(compare_value(true_lhs, true_rhs), Ordering::Greater) {
                            let lt = new_l.le(&new_r);
                            let int = lt.ite(&Int::from_u64(1), &Int::from_u64(0));
                            self.stack.push(SymbolValue::Value(int));
                            return Some(lt);
                        } else {
                            // not less than
                            let nlt = new_l.le(&new_r).not();
                            let int = nlt.ite(&Int::from_u64(1), &Int::from_u64(0));
                            self.stack.push(SymbolValue::Value(int));
                            return Some(nlt);
                        }
                    }
                    Bytecode::Gt => {
                        let (rhs, lhs) = (self.stack.pop().unwrap(), self.stack.pop().unwrap());
                        let mut stack_iter = stack.unwrap().last_n(2).unwrap();
                        let true_lhs = stack_iter.next().unwrap();
                        let true_rhs = stack_iter.next().unwrap();
                        let (new_l, new_r) = match (lhs, rhs) {
                            (SymbolValue::Value(l), SymbolValue::Value(r)) => (l, r),
                            (SymbolValue::Value(l), SymbolValue::Unknown) => {
                                let new_r = Self::resolve_value(true_rhs);
                                (l, new_r)
                            }
                            (SymbolValue::Unknown, SymbolValue::Value(r)) => {
                                let new_l = Self::resolve_value(true_lhs);
                                (new_l, r)
                            }
                            (SymbolValue::Unknown, SymbolValue::Unknown) => {
                                self.stack.push(SymbolValue::Unknown);
                                return None;
                            }
                        };
                        if matches!(compare_value(true_lhs, true_rhs), Ordering::Greater) {
                            let lt = new_l.gt(&new_r);
                            let int = lt.ite(&Int::from_u64(1), &Int::from_u64(0));
                            self.stack.push(SymbolValue::Value(int));
                            return Some(lt);
                        } else {
                            // not less than
                            let nlt = new_l.gt(&new_r).not();
                            let int = nlt.ite(&Int::from_u64(1), &Int::from_u64(0));
                            self.stack.push(SymbolValue::Value(int));
                            return Some(nlt);
                        }
                    }
                    Bytecode::Ge => {
                        let (rhs, lhs) = (self.stack.pop().unwrap(), self.stack.pop().unwrap());
                        let mut stack_iter = stack.unwrap().last_n(2).unwrap();
                        let true_lhs = stack_iter.next().unwrap();
                        let true_rhs = stack_iter.next().unwrap();
                        let (new_l, new_r) = match (lhs, rhs) {
                            (SymbolValue::Value(l), SymbolValue::Value(r)) => (l, r),
                            (SymbolValue::Value(l), SymbolValue::Unknown) => {
                                let new_r = Self::resolve_value(true_rhs);
                                (l, new_r)
                            }
                            (SymbolValue::Unknown, SymbolValue::Value(r)) => {
                                let new_l = Self::resolve_value(true_lhs);
                                (new_l, r)
                            }
                            (SymbolValue::Unknown, SymbolValue::Unknown) => {
                                self.stack.push(SymbolValue::Unknown);
                                return None;
                            }
                        };
                        if !matches!(compare_value(true_lhs, true_rhs), Ordering::Less) {
                            let lt = new_l.ge(&new_r);
                            let int = lt.ite(&Int::from_u64(1), &Int::from_u64(0));
                            self.stack.push(SymbolValue::Value(int));
                            return Some(lt);
                        } else {
                            // not less than
                            let nlt = new_l.ge(&new_r).not();
                            let int = nlt.ite(&Int::from_u64(1), &Int::from_u64(0));
                            self.stack.push(SymbolValue::Value(int));
                            return Some(nlt);
                        }
                    }
                    Bytecode::VecPack(_, len) => {
                        for _ in 0..*len {
                            self.stack.pop();
                        }
                        self.stack.push(SymbolValue::Unknown); // represent the vector as unknown
                    }
                    Bytecode::VecUnpack(_, len) => {
                        self.stack.pop();
                        for _ in 0..*len {
                            self.stack.push(SymbolValue::Unknown); // represent each element as unknown
                        }
                    }
                    Bytecode::VecSwap(_) => {
                        self.stack.pop();
                        self.stack.pop();
                        self.stack.pop();
                    }
                    Bytecode::Pack(_) | Bytecode::PackGeneric(_) => {
                        match extra.as_ref().unwrap() {
                            ExtraInstructionInformation::Pack(count)
                            | ExtraInstructionInformation::PackGeneric(count) => {
                                for _ in 0..*count {
                                    self.stack.pop();
                                }
                                self.stack.push(SymbolValue::Unknown); // represent the struct as unknown
                            }
                            _ => unreachable!(),
                        }
                    }
                    Bytecode::Unpack(_) | Bytecode::UnpackGeneric(_) => {
                        self.stack.pop();
                        match extra.as_ref().unwrap() {
                            ExtraInstructionInformation::Unpack(count)
                            | ExtraInstructionInformation::UnpackGeneric(count) => {
                                for _ in 0..*count {
                                    self.stack.push(SymbolValue::Unknown); // represent each field as unknown
                                }
                            }
                            _ => unreachable!(),
                        }
                    }
                    Bytecode::PackVariant(_) | Bytecode::PackVariantGeneric(_) => {
                        match extra.as_ref().unwrap() {
                            ExtraInstructionInformation::PackVariant(count)
                            | ExtraInstructionInformation::PackVariantGeneric(count) => {
                                for _ in 0..*count {
                                    self.stack.pop();
                                }
                                self.stack.push(SymbolValue::Unknown); // represent the enum as unknown
                            }
                            _ => unreachable!(),
                        }
                    }
                    Bytecode::UnpackVariant(_)
                    | Bytecode::UnpackVariantImmRef(_)
                    | Bytecode::UnpackVariantMutRef(_)
                    | Bytecode::UnpackVariantGeneric(_)
                    | Bytecode::UnpackVariantGenericImmRef(_)
                    | Bytecode::UnpackVariantGenericMutRef(_) => {
                        self.stack.pop();
                        if extra.is_none() {
                            warn!("Missing extra info for unpack variant at pc {}", pc);
                            self.disable = true;
                            return None;
                        }
                        match extra.as_ref().unwrap() {
                            ExtraInstructionInformation::UnpackVariant(count)
                            | ExtraInstructionInformation::UnpackVariantGeneric(count) => {
                                for _ in 0..*count {
                                    self.stack.push(SymbolValue::Unknown); // represent each field as unknown
                                }
                            }
                            _ => unreachable!(),
                        }
                    }
                    _ => {}
                }
            }
            _ => {
                trace!("Unsupported event: {:?}", event);
            }
        }
        None
    }
}
