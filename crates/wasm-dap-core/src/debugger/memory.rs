//! Memory references and linear-memory access.
//!
//! Linear memory 0 is referenced by plain hexadecimal addresses
//! (`0x00001000`); other memories use `mem<N>:0x<address>`. Code locations use
//! the same hexadecimal form with module offsets.

use wasmtime::{AsContextMut, Instance, StoreContextMut};

use crate::error::{Error, Result};
use crate::runtime::HostState;

pub fn format_code_address(pc: u32) -> String {
    format!("0x{pc:08x}")
}

pub fn format_memory_reference(memory: u32, address: u64) -> String {
    if memory == 0 {
        format!("0x{address:08x}")
    } else {
        format!("mem{memory}:0x{address:08x}")
    }
}

/// Parses a hexadecimal (`0x...`) or decimal address.
pub fn parse_address(text: &str) -> Option<u64> {
    let text = text.trim();
    match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => text.parse().ok(),
    }
}

/// Parses a memory reference into (memory index, address).
pub fn parse_memory_reference(text: &str) -> Option<(u32, u64)> {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix("mem")
        && let Some((index, address)) = rest.split_once(':')
    {
        return Some((index.parse().ok()?, parse_address(address)?));
    }
    Some((0, parse_address(text)?))
}

/// Copies up to `count` bytes starting at `address`, stopping at the end of memory.
pub(crate) fn read(
    store: &mut StoreContextMut<'_, HostState>,
    instance: Instance,
    memory: u32,
    address: u64,
    count: usize,
) -> Result<Vec<u8>> {
    if let Some(mem) = instance.debug_memory(store.as_context_mut(), memory) {
        let data = mem.data(&*store);
        let start = usize::try_from(address)
            .unwrap_or(usize::MAX)
            .min(data.len());
        let end = start.saturating_add(count).min(data.len());
        return Ok(data[start..end].to_vec());
    }
    if let Some(shared) = instance.debug_shared_memory(store.as_context_mut(), memory) {
        let data = shared.data();
        let start = usize::try_from(address)
            .unwrap_or(usize::MAX)
            .min(data.len());
        let end = start.saturating_add(count).min(data.len());
        // SAFETY: the debuggee is paused, so no thread of this store mutates
        // the memory while it is copied.
        return Ok(data[start..end]
            .iter()
            .map(|cell| unsafe { *cell.get() })
            .collect());
    }
    Err(Error::Session(format!("the module has no memory {memory}")))
}

/// Reads exactly `count` bytes or fails.
pub(crate) fn read_exact(
    store: &mut StoreContextMut<'_, HostState>,
    instance: Instance,
    address: u64,
    count: usize,
) -> Result<Vec<u8>> {
    let bytes = read(store, instance, 0, address, count)?;
    if bytes.len() != count {
        return Err(Error::Session(format!(
            "address {address:#x} is outside linear memory"
        )));
    }
    Ok(bytes)
}

/// Writes bytes into linear memory, failing if the range is out of bounds.
pub(crate) fn write(
    store: &mut StoreContextMut<'_, HostState>,
    instance: Instance,
    memory: u32,
    address: u64,
    bytes: &[u8],
) -> Result<()> {
    let mem = instance
        .debug_memory(store.as_context_mut(), memory)
        .ok_or_else(|| Error::Session(format!("memory {memory} cannot be written")))?;
    let data = mem.data_mut(&mut *store);
    let start = usize::try_from(address)
        .ok()
        .filter(|s| *s <= data.len())
        .ok_or_else(|| Error::Session(format!("address {address:#x} is outside memory")))?;
    let end = start
        .checked_add(bytes.len())
        .filter(|e| *e <= data.len())
        .ok_or_else(|| Error::Session("write extends past the end of memory".into()))?;
    data[start..end].copy_from_slice(bytes);
    Ok(())
}

/// Current size of a memory in bytes.
pub(crate) fn size(
    store: &mut StoreContextMut<'_, HostState>,
    instance: Instance,
    memory: u32,
) -> Option<u64> {
    if let Some(mem) = instance.debug_memory(store.as_context_mut(), memory) {
        return Some(mem.data_size(&*store) as u64);
    }
    instance
        .debug_shared_memory(store.as_context_mut(), memory)
        .map(|m| m.data_size() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_references_round_trip() {
        assert_eq!(format_memory_reference(0, 0x1000), "0x00001000");
        assert_eq!(parse_memory_reference("0x00001000"), Some((0, 0x1000)));
        assert_eq!(format_memory_reference(2, 16), "mem2:0x00000010");
        assert_eq!(parse_memory_reference("mem2:0x00000010"), Some((2, 16)));
        assert_eq!(parse_memory_reference("4096"), Some((0, 4096)));
        assert_eq!(parse_memory_reference("bogus"), None);
    }
}
