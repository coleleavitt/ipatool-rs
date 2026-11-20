//! Process-wide cache of the immutable parts of shared images: instruction patches,
//! symbol tables and decoded relocations. Every guest supplies its own load bases, import
//! addresses and mutable memory.

use std::collections::HashMap;
use std::mem::size_of;
use std::sync::{Arc, Mutex, PoisonError};

use super::macho::{Bind, Rebase, Symbol};

pub(super) const MAX_IMAGE_PREPARATIONS: usize = 8;
pub(super) const MAX_PREPARATION_BYTES: usize = 64 << 20;

pub(super) type Key = [u8; 32];
pub(super) type Instructions = HashMap<u64, Vec<InstructionPatch>>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ChainedRelocation {
    pub offset: u64,
    pub target: u64,
    pub addend: i64,
    pub symbol: String,
    pub is_bind: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct InstructionPatch {
    pub offset: u64,
    pub value: u32,
}

#[derive(Clone, Default)]
pub(super) struct Preparation {
    pub instructions: Arc<Instructions>,
    pub fixups: Arc<Vec<ChainedRelocation>>,
    pub binds: Arc<Vec<Bind>>,
    pub rebases: Arc<Vec<Rebase>>,
    pub symbols: Option<Arc<Vec<Symbol>>>,
}

impl Preparation {
    fn size(&self) -> usize {
        let mut size = self.fixups.capacity() * size_of::<ChainedRelocation>()
            + self.binds.capacity() * size_of::<Bind>()
            + self.rebases.capacity() * size_of::<Rebase>();
        size += self
            .instructions
            .values()
            .map(|patches| patches.capacity() * size_of::<InstructionPatch>())
            .sum::<usize>();
        size += self
            .fixups
            .iter()
            .map(|fixup| fixup.symbol.len())
            .sum::<usize>();
        size += self.binds.iter().map(|bind| bind.name.len()).sum::<usize>();
        if let Some(symbols) = &self.symbols {
            size += symbols.capacity() * size_of::<Symbol>();
            size += symbols
                .iter()
                .map(|symbol| symbol.name.len() + symbol.indirect_name.len())
                .sum::<usize>();
        }
        size
    }
}

#[derive(Default)]
pub(super) struct State {
    pub entries: Vec<(Key, Preparation)>,
    pub bytes: usize,
}

pub(super) struct PreparationCache(Mutex<State>);

pub(super) static IMAGE_PREPARATIONS: PreparationCache = PreparationCache::new();

impl PreparationCache {
    pub const fn new() -> Self {
        Self(Mutex::new(State {
            entries: Vec::new(),
            bytes: 0,
        }))
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn get(&self, key: &Key) -> Option<Preparation> {
        self.state()
            .entries
            .iter()
            .find(|(entry, _)| entry == key)
            .map(|(_, prepared)| prepared.clone())
    }

    pub fn put(&self, key: Key, prepared: Preparation) {
        let size = prepared.size();
        if size > MAX_PREPARATION_BYTES {
            return;
        }
        let mut state = self.state();
        if state.entries.iter().any(|(entry, _)| *entry == key) {
            return;
        }
        while state.entries.len() >= MAX_IMAGE_PREPARATIONS
            || state.bytes + size > MAX_PREPARATION_BYTES
        {
            let (_, oldest) = state.entries.remove(0);
            state.bytes -= oldest.size();
        }
        state.entries.push((key, prepared));
        state.bytes += size;
    }
}
