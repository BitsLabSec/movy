use std::fmt::Display;

use alloy_primitives::U256;
use color_eyre::eyre::eyre;
use move_binary_format::file_format::Bytecode;
use move_vm_types::values::IntegerValue;
use movy_types::error::MovyError;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Magic {
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    U128(u128),
    U256(U256),
    Bytes(Vec<u8>),
}

impl Display for Magic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::U8(v) => f.write_fmt(format_args!("U8({})", v)),
            Self::U16(v) => f.write_fmt(format_args!("U16({})", v)),
            Self::U32(v) => f.write_fmt(format_args!("U32({})", v)),
            Self::U64(v) => f.write_fmt(format_args!("U64({})", v)),
            Self::U128(v) => f.write_fmt(format_args!("U128({})", v)),
            Self::U256(v) => f.write_fmt(format_args!("U256({})", v)),
            Self::Bytes(v) => f.write_fmt(format_args!("Bytes({})", const_hex::encode(v))),
        }
    }
}

impl From<IntegerValue> for Magic {
    fn from(value: IntegerValue) -> Self {
        match value {
            IntegerValue::U8(v) => Self::U8(v),
            IntegerValue::U16(v) => Self::U16(v),
            IntegerValue::U32(v) => Self::U32(v),
            IntegerValue::U64(v) => Self::U64(v),
            IntegerValue::U128(v) => Self::U128(v),
            IntegerValue::U256(v) => Self::U256(U256::from_be_bytes(v.to_be_bytes())),
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone, Copy)]
pub enum CmpOp {
    LT,
    LE,
    GT,
    GE,
    NEQ,
    EQ,
}

impl Display for CmpOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LE => f.write_str("<="),
            Self::LT => f.write_str("<"),
            Self::GE => f.write_str(">="),
            Self::GT => f.write_str(">"),
            Self::NEQ => f.write_str("!="),
            Self::EQ => f.write_str("=="),
        }
    }
}

impl TryFrom<&Bytecode> for CmpOp {
    type Error = MovyError;
    fn try_from(value: &Bytecode) -> Result<Self, Self::Error> {
        match value {
            Bytecode::Le => Ok(Self::LE),
            Bytecode::Lt => Ok(Self::LT),
            Bytecode::Ge => Ok(Self::GE),
            Bytecode::Gt => Ok(Self::GT),
            Bytecode::Neq => Ok(Self::NEQ),
            Bytecode::Eq => Ok(Self::EQ),
            _ => Err(eyre!("{:?} can not convert into Cmpop", value).into()),
        }
    }
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub struct CmpLog {
    pub lhs: Magic,
    pub rhs: Magic,
    pub op: CmpOp,
    pub constraint: Option<z3::ast::Bool>,
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub struct ShlLog {
    pub lhs: Magic,
    pub rhs: Magic,
    pub constraint: Option<z3::ast::Bool>,
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub struct CastLog {
    pub lhs: Magic,
    pub constraint: Option<z3::ast::Bool>,
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub enum Log {
    CmpLog(CmpLog),
    ShlLog(ShlLog),
    CastLog(CastLog),
}

impl Display for CmpLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}{}{}", &self.lhs, &self.op, &self.rhs))
    }
}
