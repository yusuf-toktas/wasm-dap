//! Evaluation of DWARF location expressions for WebAssembly.
//!
//! gimli's evaluator rejects the `DW_OP_WASM_location` family, so this module
//! implements the subset of the expression language that compilers emit for
//! WebAssembly targets.

use gimli::Reader as _;
use gimli::{Encoding, Operation};

use super::Reader;
use crate::error::{Error, Result};

/// Kind of WebAssembly storage referenced by `DW_OP_WASM_location`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasmStorage {
    Local,
    Global,
    Stack,
}

/// Where a variable lives, or its value when it has no storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// Address in linear memory 0.
    Memory(u64),
    /// The variable occupies a WebAssembly local, global or operand stack slot.
    Wasm(WasmStorage, u32),
    /// The value itself (little-endian bytes).
    Value(Vec<u8>),
    /// Composite location assembled from pieces of `size` bytes each.
    Pieces(Vec<(Location, u64)>),
    /// The value is not available at this point (optimized out).
    Unavailable,
}

/// Machine state accessors needed to evaluate location expressions.
pub trait ExprContext {
    /// Raw little-endian bytes of a WebAssembly local, global or stack value.
    fn wasm_value(&mut self, storage: WasmStorage, index: u32) -> Option<Vec<u8>>;
    /// Value of the current function's frame base.
    fn frame_base(&mut self) -> Option<u64>;
    /// Reads `size` bytes of linear memory.
    fn read_memory(&mut self, address: u64, size: usize) -> Option<Vec<u8>>;
}

fn bytes_to_u64(bytes: &[u8]) -> u64 {
    let mut buf = [0u8; 8];
    let n = bytes.len().min(8);
    buf[..n].copy_from_slice(&bytes[..n]);
    u64::from_le_bytes(buf)
}

/// Evaluates a location expression.
pub fn evaluate(
    expr: &gimli::Expression<Reader>,
    encoding: Encoding,
    ctx: &mut dyn ExprContext,
) -> Result<Location> {
    let mut ops = Vec::new();
    let mut iter = expr.clone().operations(encoding);
    while let Some(op) = iter.next()? {
        ops.push(op);
    }

    let mut stack: Vec<u64> = Vec::new();
    let mut pieces: Vec<(Location, u64)> = Vec::new();
    let mut pending: Option<Location> = None;

    let pop = |stack: &mut Vec<u64>| -> Result<u64> {
        stack
            .pop()
            .ok_or_else(|| Error::Dwarf("DWARF expression stack underflow".into()))
    };

    let mut i = 0;
    while i < ops.len() {
        let next_is_terminal = matches!(ops.get(i + 1), None | Some(Operation::Piece { .. }));
        match &ops[i] {
            Operation::WasmLocal { index }
            | Operation::WasmGlobal { index }
            | Operation::WasmStack { index } => {
                let storage = match &ops[i] {
                    Operation::WasmLocal { .. } => WasmStorage::Local,
                    Operation::WasmGlobal { .. } => WasmStorage::Global,
                    _ => WasmStorage::Stack,
                };
                if next_is_terminal {
                    pending = Some(Location::Wasm(storage, *index));
                } else {
                    let value = ctx
                        .wasm_value(storage, *index)
                        .ok_or_else(|| Error::Dwarf("WebAssembly value unavailable".into()))?;
                    stack.push(bytes_to_u64(&value));
                }
            }
            Operation::FrameOffset { offset } => {
                let base = ctx
                    .frame_base()
                    .ok_or_else(|| Error::Dwarf("frame base unavailable".into()))?;
                stack.push(base.wrapping_add(*offset as u64));
            }
            Operation::Address { address } => stack.push(*address),
            Operation::UnsignedConstant { value } => stack.push(*value),
            Operation::SignedConstant { value } => stack.push(*value as u64),
            Operation::PlusConstant { value } => {
                let top = pop(&mut stack)?;
                stack.push(top.wrapping_add(*value));
            }
            Operation::Plus => binary(&mut stack, u64::wrapping_add)?,
            Operation::Minus => binary(&mut stack, u64::wrapping_sub)?,
            Operation::Mul => binary(&mut stack, u64::wrapping_mul)?,
            Operation::Div => binary(&mut stack, |a, b| {
                if b == 0 {
                    0
                } else {
                    ((a as i64).wrapping_div(b as i64)) as u64
                }
            })?,
            Operation::Mod => binary(&mut stack, |a, b| if b == 0 { 0 } else { a % b })?,
            Operation::And => binary(&mut stack, |a, b| a & b)?,
            Operation::Or => binary(&mut stack, |a, b| a | b)?,
            Operation::Xor => binary(&mut stack, |a, b| a ^ b)?,
            Operation::Shl => binary(&mut stack, |a, b| a.wrapping_shl(b as u32))?,
            Operation::Shr => binary(&mut stack, |a, b| a.wrapping_shr(b as u32))?,
            Operation::Shra => binary(&mut stack, |a, b| {
                ((a as i64).wrapping_shr(b as u32)) as u64
            })?,
            Operation::Neg => {
                let top = pop(&mut stack)?;
                stack.push((top as i64).wrapping_neg() as u64);
            }
            Operation::Not => {
                let top = pop(&mut stack)?;
                stack.push(!top);
            }
            Operation::Abs => {
                let top = pop(&mut stack)?;
                stack.push((top as i64).unsigned_abs());
            }
            Operation::Drop => {
                pop(&mut stack)?;
            }
            Operation::Pick { index } => {
                let len = stack.len();
                let idx = len
                    .checked_sub(1 + *index as usize)
                    .ok_or_else(|| Error::Dwarf("DWARF pick out of range".into()))?;
                stack.push(stack[idx]);
            }
            Operation::Swap => {
                let a = pop(&mut stack)?;
                let b = pop(&mut stack)?;
                stack.push(a);
                stack.push(b);
            }
            Operation::Rot => {
                let a = pop(&mut stack)?;
                let b = pop(&mut stack)?;
                let c = pop(&mut stack)?;
                stack.push(a);
                stack.push(c);
                stack.push(b);
            }
            Operation::Deref { size, space, .. } => {
                if *space {
                    pop(&mut stack)?;
                }
                let address = pop(&mut stack)?;
                let bytes = ctx
                    .read_memory(address, usize::from(*size))
                    .ok_or_else(|| Error::Dwarf(format!("cannot read memory at {address:#x}")))?;
                stack.push(bytes_to_u64(&bytes));
            }
            Operation::StackValue => {
                let top = pop(&mut stack)?;
                pending = Some(Location::Value(top.to_le_bytes().to_vec()));
            }
            Operation::ImplicitValue { data } => {
                pending = Some(Location::Value(data.to_slice()?.into_owned()));
            }
            Operation::Piece {
                size_in_bits,
                bit_offset,
            } => {
                if bit_offset.is_some_and(|b| b != 0) || size_in_bits % 8 != 0 {
                    return Err(Error::Unsupported("bit pieces in DWARF locations".into()));
                }
                let location = match pending.take() {
                    Some(loc) => loc,
                    None => match stack.pop() {
                        Some(address) => Location::Memory(address),
                        None => Location::Unavailable,
                    },
                };
                pieces.push((location, size_in_bits / 8));
            }
            Operation::Nop => {}
            Operation::EntryValue { .. }
            | Operation::ParameterRef { .. }
            | Operation::ImplicitPointer { .. } => return Ok(Location::Unavailable),
            other => {
                return Err(Error::Unsupported(format!(
                    "DWARF operation {other:?} in a location expression"
                )));
            }
        }
        i += 1;
    }

    if !pieces.is_empty() {
        return Ok(Location::Pieces(pieces));
    }
    if let Some(location) = pending {
        return Ok(location);
    }
    Ok(match stack.pop() {
        Some(address) => Location::Memory(address),
        None => Location::Unavailable,
    })
}

fn binary(stack: &mut Vec<u64>, f: impl Fn(u64, u64) -> u64) -> Result<()> {
    let b = stack
        .pop()
        .ok_or_else(|| Error::Dwarf("DWARF expression stack underflow".into()))?;
    let a = stack
        .pop()
        .ok_or_else(|| Error::Dwarf("DWARF expression stack underflow".into()))?;
    stack.push(f(a, b));
    Ok(())
}

/// Evaluates a subprogram's `DW_AT_frame_base` to the frame base address.
pub fn evaluate_frame_base(
    expr: &gimli::Expression<Reader>,
    encoding: Encoding,
    ctx: &mut dyn ExprContext,
) -> Result<u64> {
    match evaluate(expr, encoding, ctx)? {
        Location::Wasm(storage, index) => ctx
            .wasm_value(storage, index)
            .map(|b| bytes_to_u64(&b))
            .ok_or_else(|| Error::Dwarf("frame base value unavailable".into())),
        Location::Value(bytes) => Ok(bytes_to_u64(&bytes)),
        Location::Memory(address) => Ok(address),
        other => Err(Error::Dwarf(format!("unsupported frame base {other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    struct Ctx;

    impl ExprContext for Ctx {
        fn wasm_value(&mut self, storage: WasmStorage, index: u32) -> Option<Vec<u8>> {
            match (storage, index) {
                (WasmStorage::Local, 1) => Some(1000u32.to_le_bytes().to_vec()),
                (WasmStorage::Global, 0) => Some(65536u32.to_le_bytes().to_vec()),
                (WasmStorage::Local, 3) => Some((-7i32).to_le_bytes().to_vec()),
                _ => None,
            }
        }
        fn frame_base(&mut self) -> Option<u64> {
            Some(1000)
        }
        fn read_memory(&mut self, address: u64, size: usize) -> Option<Vec<u8>> {
            (address == 2000).then(|| 42u64.to_le_bytes()[..size].to_vec())
        }
    }

    fn encoding() -> Encoding {
        Encoding {
            address_size: 4,
            format: gimli::Format::Dwarf32,
            version: 4,
        }
    }

    fn locate(bytes: &[u8]) -> Location {
        let reader = Reader::new(Arc::from(bytes), gimli::LittleEndian);
        evaluate(&gimli::Expression(reader), encoding(), &mut Ctx).unwrap()
    }

    #[test]
    fn frame_offset_yields_memory_address() {
        // DW_OP_fbreg 12
        assert_eq!(locate(&[0x91, 12]), Location::Memory(1012));
    }

    #[test]
    fn wasm_local_location_and_stack_value() {
        // DW_OP_WASM_location 0x0 3
        assert_eq!(
            locate(&[0xed, 0x00, 0x03]),
            Location::Wasm(WasmStorage::Local, 3)
        );
        // DW_OP_WASM_location 0x0 1; DW_OP_stack_value
        assert_eq!(
            locate(&[0xed, 0x00, 0x01, 0x9f]),
            Location::Value(1000u64.to_le_bytes().to_vec())
        );
    }

    #[test]
    fn frame_base_from_wasm_local() {
        let reader = Reader::new(
            Arc::from(&[0xedu8, 0x00, 0x01, 0x9f][..]),
            gimli::LittleEndian,
        );
        let base = evaluate_frame_base(&gimli::Expression(reader), encoding(), &mut Ctx).unwrap();
        assert_eq!(base, 1000);
    }

    #[test]
    fn address_plus_constant_and_deref() {
        // DW_OP_addr 0x7d0; DW_OP_deref_size 4; DW_OP_stack_value
        assert_eq!(
            locate(&[0x03, 0xd0, 0x07, 0x00, 0x00, 0x94, 0x04, 0x9f]),
            Location::Value(42u64.to_le_bytes().to_vec())
        );
        // DW_OP_addr 0x10; DW_OP_plus_uconst 4
        assert_eq!(
            locate(&[0x03, 0x10, 0, 0, 0, 0x23, 0x04]),
            Location::Memory(0x14)
        );
    }

    #[test]
    fn pieces_are_collected() {
        // DW_OP_WASM_location 0 3; DW_OP_piece 4; DW_OP_fbreg 0; DW_OP_piece 4
        let loc = locate(&[0xed, 0x00, 0x03, 0x93, 0x04, 0x91, 0x00, 0x93, 0x04]);
        assert_eq!(
            loc,
            Location::Pieces(vec![
                (Location::Wasm(WasmStorage::Local, 3), 4),
                (Location::Memory(1000), 4)
            ])
        );
    }
}
