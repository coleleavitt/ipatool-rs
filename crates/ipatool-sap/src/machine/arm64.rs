//! Software fallbacks for arm64 instructions Unicorn rejects (LSE atomics, SHA3 `EOR3`).

use super::shims::{Guest, REG_LR, REG_PC, REG_SP, REG_V0, REG_X0, REG_X29, le64};
use crate::error::{Context, Error, Result};

const ATOMIC_MASK: u32 = 0xffe0_fc00;
const CASAL32: u32 = 0x88e0_fc00;
const CASAL64: u32 = 0xc8e0_fc00;
const SWPAL32: u32 = 0xb8e0_8000;
const SWPAL64: u32 = 0xf8e0_8000;
const EOR3_MASK: u32 = 0xffe0_8000;
const EOR3: u32 = 0xce00_0000;

/// Go's `arm64Compatibility.emulateInvalidInstruction`: performs the instruction at PC and
/// advances past it, or reports that it is not one of the emulated forms.
pub(crate) fn emulate_invalid_instruction(engine: &mut impl Guest) -> Result<bool> {
    let pc = engine
        .reg(REG_PC)
        .context("read arm64 instruction address")?;
    let Ok(data) = engine.read_mem(pc, 4) else {
        return Ok(false);
    };
    let instruction = super::shims::le32(&data);
    if instruction & EOR3_MASK == EOR3 {
        emulate_eor3(engine, instruction)?;
        engine
            .set_reg(REG_PC, pc + 4)
            .context("advance arm64 instruction address")?;
        return Ok(true);
    }
    let opcode = instruction & ATOMIC_MASK;
    let width: u64 = match opcode {
        CASAL32 | SWPAL32 => 4,
        CASAL64 | SWPAL64 => 8,
        _ => return Ok(false),
    };
    let source = (instruction >> 16) & 31;
    let base = (instruction >> 5) & 31;
    let destination = instruction & 31;
    let address = read_base_register(engine, base)?;
    let mut memory = engine
        .read_mem(address, width)
        .with_context(|| format!("read arm64 atomic operand at {address:#x}"))?;
    memory.resize(8, 0);
    let old_value = le64(&memory);
    let mut source_value = read_value_register(engine, source)?;
    if width == 4 {
        source_value &= u64::from(u32::MAX);
    }
    if opcode == CASAL32 || opcode == CASAL64 {
        let mut new_value = read_value_register(engine, destination)?;
        if width == 4 {
            new_value &= u64::from(u32::MAX);
        }
        if old_value == source_value {
            write_memory_value(engine, address, new_value, width)?;
        }
        write_value_register(engine, source, old_value)?;
    } else {
        write_memory_value(engine, address, source_value, width)?;
        write_value_register(engine, destination, old_value)?;
    }
    engine
        .set_reg(REG_PC, pc + 4)
        .context("advance arm64 instruction address")?;
    Ok(true)
}

fn emulate_eor3(engine: &mut impl Guest, instruction: u32) -> Result<()> {
    let destination = instruction & 31;
    let first = (instruction >> 5) & 31;
    let third = (instruction >> 10) & 31;
    let second = (instruction >> 16) & 31;
    let first_value = read_vector(engine, first)?;
    let second_value = read_vector(engine, second)?;
    let third_value = read_vector(engine, third)?;
    let mut result = [0u8; 16];
    for (index, byte) in result.iter_mut().enumerate() {
        *byte = first_value[index] ^ second_value[index] ^ third_value[index];
    }
    engine
        .set_reg128(vector(destination), result)
        .with_context(|| format!("write arm64 vector register {destination}"))
}

fn read_vector(engine: &impl Guest, index: u32) -> Result<[u8; 16]> {
    engine
        .reg128(vector(index))
        .with_context(|| format!("read arm64 vector register {index}"))
}

fn vector(index: u32) -> i32 {
    REG_V0 + i32::try_from(index).unwrap_or_default()
}

fn read_base_register(engine: &impl Guest, index: u32) -> Result<u64> {
    let register = if index == 31 {
        REG_SP
    } else {
        general_register(index)
            .ok_or_else(|| Error::msg(format!("invalid arm64 base register {index}")))?
    };
    engine
        .reg(register)
        .with_context(|| format!("read arm64 base register {index}"))
}

fn read_value_register(engine: &impl Guest, index: u32) -> Result<u64> {
    if index == 31 {
        return Ok(0);
    }
    let register = general_register(index)
        .ok_or_else(|| Error::msg(format!("invalid arm64 value register {index}")))?;
    engine
        .reg(register)
        .with_context(|| format!("read arm64 value register {index}"))
}

fn write_value_register(engine: &mut impl Guest, index: u32, value: u64) -> Result<()> {
    if index == 31 {
        return Ok(());
    }
    let register = general_register(index)
        .ok_or_else(|| Error::msg(format!("invalid arm64 value register {index}")))?;
    engine
        .set_reg(register, value)
        .with_context(|| format!("write arm64 value register {index}"))
}

fn write_memory_value(engine: &mut impl Guest, address: u64, value: u64, width: u64) -> Result<()> {
    let data = value.to_le_bytes();
    let width = if width == 4 { 4 } else { 8 };
    engine
        .write_mem(address, &data[..width])
        .with_context(|| format!("write arm64 atomic operand at {address:#x}"))
}

fn general_register(index: u32) -> Option<i32> {
    match index {
        0..=28 => Some(REG_X0 + i32::try_from(index).ok()?),
        29 => Some(REG_X29),
        30 => Some(REG_LR),
        _ => None,
    }
}
