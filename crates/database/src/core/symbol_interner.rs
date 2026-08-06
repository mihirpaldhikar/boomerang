/*
 * Copyright (c) Mihir Paldhikar
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the “Software”), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED “AS IS”, WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 */

#![allow(dead_code)]

use hashbrown::HashTable;
use hashbrown::hash_table::Entry;
use parking_lot::RwLock;
use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::cell::Cell;
use std::hash::{BuildHasher, Hash};
use std::num::NonZeroU32;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicUsize, Ordering};

const DEFAULT_SHARD_COUNT: usize = 16;
const CACHE_CAPACITY: usize = 1024; // MUST be a power of 2
const _: () = assert!(CACHE_CAPACITY.is_power_of_two());
const CACHE_MASK: u64 = (CACHE_CAPACITY - 1) as u64;

const CHUNK_SHIFT: usize = 12;
const CHUNK_SIZE: usize = 1 << CHUNK_SHIFT;
const CHUNK_MASK: usize = CHUNK_SIZE - 1;

// A Symbol's index is a 24-bit value (see Symbol::INDEX_MASK), so a shard
// can hold at most 2^24 strings. The top-level chunk array must therefore
// cover the full 2^24 index space: 2^24 / CHUNK_SIZE chunks.
const NUM_CHUNKS: usize = 1 << (24 - CHUNK_SHIFT); // 4096

// Default Arena Capacity: 256 MB.
const ARENA_CAPACITY: u32 = 256 * 1024 * 1024;

static NEXT_INTERNER_ID: AtomicU32 = AtomicU32::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct Symbol(NonZeroU32);

impl Symbol {
    const INDEX_MASK: u32 = 0x00FF_FFFF; // Bottom 24 bits.

    fn new(shard_id: u8, index: usize) -> Self {
        assert!(index < Self::INDEX_MASK as usize, "Symbol index overflow");

        let raw = ((shard_id as u32) << 24) | ((index as u32) + 1);
        Symbol(unsafe { NonZeroU32::new_unchecked(raw) })
    }

    #[inline(always)]
    fn shard_id(&self) -> usize {
        (self.0.get() >> 24) as usize
    }

    #[inline(always)]
    fn index(&self) -> usize {
        (self.0.get() & Self::INDEX_MASK) as usize - 1
    }

    #[inline]
    pub fn into_raw(self) -> u32 {
        self.0.get()
    }

    #[inline]
    pub fn from_raw(raw: u32) -> Option<Self> {
        NonZeroU32::new(raw).map(Symbol)
    }
}

struct MemoryPool {
    ptr: NonNull<u8>,
    cursor: AtomicU32,
    capacity: u32,
    layout: Layout,
    poisoned: AtomicBool,
}

unsafe impl Send for MemoryPool {}
unsafe impl Sync for MemoryPool {}

/// Hint that the CPU should start pulling `ptr` into L1 cache now, even
/// though we don't dereference it until later. No-op on architectures
/// without an explicit prefetch instruction — always safe to call on any
/// pointer, valid or not, since it never actually reads memory.
#[inline(always)]
fn prefetch_read(ptr: *const u8) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use std::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
        _mm_prefetch(ptr as *const i8, _MM_HINT_T0);
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        std::arch::asm!(
        "prfm pldl1keep, [{0}]",
        in(reg) ptr,
        options(nostack, preserves_flags)
        );
    }
}

impl MemoryPool {
    fn new(capacity: u32, shard_count: u32) -> Self {
        let layout = Layout::array::<u8>(capacity as usize).expect("valid layout");

        let ptr = unsafe { alloc(layout) };
        let ptr = NonNull::new(ptr).unwrap_or_else(|| handle_alloc_error(layout));

        unsafe { std::ptr::write_unaligned(ptr.as_ptr() as *mut u32, shard_count) };

        Self {
            ptr,
            cursor: AtomicU32::new(4),
            capacity,
            layout,
            poisoned: AtomicBool::new(false),
        }
    }

    #[inline(always)]
    fn base_ptr(&self) -> *const u8 {
        self.ptr.as_ptr() as *const u8
    }

    #[cold]
    #[inline(never)]
    fn pool_exhausted() -> ! {
        panic!("Pool out of memory");
    }

    #[inline]
    fn alloc(&self, value: &str) -> u32 {
        if self.poisoned.load(Ordering::Acquire) {
            Self::pool_exhausted()
        }

        // Reject strings whose length can't be represented in the u32 header,
        // or whose encoded size (len + 4-byte header) would overflow u32.
        let len: u32 = value
            .len()
            .try_into()
            .unwrap_or_else(|_| panic!("string too large to intern: {} bytes", value.len()));
        let size = len
            .checked_add(4)
            .unwrap_or_else(|| panic!("string too large to intern: {} bytes", value.len()));

        let offset_result =
            self.cursor
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                    let remaining = self.capacity.checked_sub(current)?;
                    if size > remaining {
                        None
                    } else {
                        current.checked_add(size)
                    }
                });

        match offset_result {
            Ok(offset) => {
                unsafe {
                    let start = self.ptr.as_ptr().add(offset as usize);
                    std::ptr::write_unaligned(start as *mut u32, len);
                    std::ptr::copy_nonoverlapping(value.as_ptr(), start.add(4), len as usize);
                }
                offset
            }
            Err(_) => {
                self.poisoned.store(true, Ordering::Release);
                Self::pool_exhausted();
            }
        }
    }

    #[inline]
    pub fn committed_bytes(&self) -> &[u8] {
        let current_cursor = self.cursor.load(Ordering::Acquire) as usize;

        // The cursor may have overshot `capacity` right before poisoning; clamp
        // so we never build a slice that runs past the allocation.
        let current_cursor = current_cursor.min(self.capacity as usize);

        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), current_cursor) }
    }

    fn read_shard_count_header(bytes: &[u8]) -> Option<u32> {
        let header: [u8; 4] = bytes.get(0..4)?.try_into().ok()?;
        Some(u32::from_ne_bytes(header))
    }
}

impl Drop for MemoryPool {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

#[derive(Clone, Copy)]
#[repr(C)]
struct CacheEntry {
    interner_id: u32,
    offset: u32,
    hash: u32,
    symbol: Symbol,
}

struct ThreadLocalCache {
    entries: [Cell<Option<CacheEntry>>; CACHE_CAPACITY],
}

impl ThreadLocalCache {
    fn new() -> Self {
        Self {
            entries: std::array::from_fn(|_| Cell::new(None)),
        }
    }
}

thread_local! {
    static LOCAL_CACHE: ThreadLocalCache = ThreadLocalCache::new();
    static LAST_CHUNK: Cell<Option<(u32, usize, usize, *const StringChunk)>> = Cell::new(None);
}

struct StringChunk {
    offsets: [AtomicU32; CHUNK_SIZE],
}

impl StringChunk {
    fn new_boxed() -> Box<Self> {
        unsafe {
            let layout = Layout::new::<StringChunk>();
            let raw = alloc(layout);
            let raw = NonNull::new(raw).unwrap_or_else(|| handle_alloc_error(layout));

            std::ptr::write_bytes(raw.as_ptr(), 0, layout.size());

            Box::from_raw(raw.as_ptr() as *mut StringChunk)
        }
    }

    #[cold]
    fn alloc_new_chunk() -> *mut StringChunk {
        Box::into_raw(StringChunk::new_boxed())
    }
}

struct LockFreeStringArray {
    interner_id: u32,
    chunks: Box<[AtomicPtr<StringChunk>]>,
    len: AtomicUsize,
}

unsafe impl Send for LockFreeStringArray {}
unsafe impl Sync for LockFreeStringArray {}

impl LockFreeStringArray {
    fn new(interner_id: u32) -> Self {
        let chunks = (0..NUM_CHUNKS)
            .map(|_| AtomicPtr::new(std::ptr::null_mut()))
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Self {
            interner_id,
            chunks,
            len: AtomicUsize::new(0),
        }
    }

    #[inline]
    fn load_chunk(&self, chunk_idx: usize) -> *const StringChunk {
        let self_addr = self as *const _ as usize;

        LAST_CHUNK.with(|cell| {
            if let Some((cached_id, cached_addr, cached_idx, ptr)) = cell.get() {
                if cached_id == self.interner_id
                    && cached_addr == self_addr
                    && cached_idx == chunk_idx
                {
                    return ptr;
                }
            }

            let ptr = self.chunks[chunk_idx].load(Ordering::Acquire) as *const StringChunk;

            if !ptr.is_null() {
                cell.set(Some((self.interner_id, self_addr, chunk_idx, ptr)));
            }

            ptr
        })
    }

    fn get(&self, index: usize, base_ptr: *const u8) -> Option<&'static str> {
        if index >= self.len.load(Ordering::Acquire) {
            return None;
        }

        let chunk_idx = index >> CHUNK_SHIFT;
        let slot_idx = index & CHUNK_MASK;

        let chunk_ptr = self.load_chunk(chunk_idx);
        if chunk_ptr.is_null() {
            return None;
        }

        self.get_from_chunk(chunk_ptr, slot_idx, base_ptr)
    }

    #[inline]
    fn get_from_chunk(
        &self,
        chunk_ptr: *const StringChunk,
        slot_idx: usize,
        base_ptr: *const u8,
    ) -> Option<&'static str> {
        let chunk = unsafe { &*chunk_ptr };

        let offset = chunk.offsets[slot_idx].load(Ordering::Acquire);
        if offset == 0 {
            return None;
        }

        let ptr = unsafe { base_ptr.add(offset as usize) };

        // Kick off the fetch for the length header + first ~64 bytes of the
        // string as early as possible, before the caller does anything else
        // with `ptr`. Hides some of the DRAM round-trip behind whatever
        // instructions the compiler schedules between here and the actual read.
        prefetch_read(ptr);

        unsafe {
            let len = std::ptr::read_unaligned(ptr as *const u32) as usize;
            let slice = std::slice::from_raw_parts(ptr.add(4), len);
            Some(std::str::from_utf8_unchecked(slice))
        }
    }

    #[inline]
    unsafe fn get_unchecked(&self, index: usize, base_ptr: *const u8) -> &'static str {
        let chunk_idx = index >> CHUNK_SHIFT;
        let slot_idx = index & CHUNK_MASK;

        let chunk_ptr = self.load_chunk(chunk_idx);
        let chunk = unsafe { &*chunk_ptr };

        let offset = chunk.offsets[slot_idx].load(Ordering::Acquire);
        let ptr = unsafe { base_ptr.add(offset as usize) };

        prefetch_read(ptr);

        unsafe {
            let len = std::ptr::read_unaligned(ptr as *const u32) as usize;
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(ptr.add(4), len))
        }
    }

    fn push(&self, offset: u32) -> usize {
        let index = self.len.fetch_add(1, Ordering::AcqRel);
        let chunk_idx = index >> CHUNK_SHIFT;
        let slot_idx = index & CHUNK_MASK;

        let mut chunk_ptr = self.chunks[chunk_idx].load(Ordering::Acquire);
        if chunk_ptr.is_null() {
            let new_chunk = StringChunk::alloc_new_chunk();
            match self.chunks[chunk_idx].compare_exchange(
                std::ptr::null_mut(),
                new_chunk,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => chunk_ptr = new_chunk,
                Err(existing) => {
                    unsafe {
                        drop(Box::from_raw(new_chunk));
                    }
                    chunk_ptr = existing;
                }
            }
        }

        let chunk = unsafe { &*chunk_ptr };
        chunk.offsets[slot_idx].store(offset, Ordering::Release);
        index
    }
}

impl Drop for LockFreeStringArray {
    fn drop(&mut self) {
        for chunk_ptr in self.chunks.iter() {
            let ptr = chunk_ptr.load(Ordering::Relaxed);
            if !ptr.is_null() {
                unsafe {
                    drop(Box::from_raw(ptr));
                }
            }
        }
    }
}

struct ShardInner {
    entries: HashTable<(u64, u32, Symbol)>,
}

impl ShardInner {
    #[inline(always)]
    fn prefix4(bytes: &[u8]) -> u32 {
        let mut buf = [0u8; 4];
        let n = bytes.len().min(4);
        buf[..n].copy_from_slice(&bytes[..n]);
        u32::from_ne_bytes(buf) // native-endian: only ever compared, never serialized
    }
}

struct Shard {
    inner: RwLock<ShardInner>,
    strings: LockFreeStringArray,
}

pub struct SymbolInterner {
    interner_id: u32,
    pool: MemoryPool,
    shards: Box<[Shard]>,
    hash_builder: rustc_hash::FxBuildHasher,
    shard_mask: u64,
}

impl Default for SymbolInterner {
    fn default() -> Self {
        Self::new()
    }
}

impl SymbolInterner {
    pub fn new() -> Self {
        Self::with_capacity_and_shards(0, DEFAULT_SHARD_COUNT)
    }

    pub fn with_capacity(symbols: usize) -> Self {
        Self::with_capacity_and_shards(symbols, DEFAULT_SHARD_COUNT)
    }

    pub fn with_capacity_and_shards(symbols: usize, count: usize) -> Self {
        let interner_id = NEXT_INTERNER_ID.fetch_add(1, Ordering::Relaxed);

        let count = count.clamp(1, 256).next_power_of_two();
        let per_shard = symbols.div_ceil(count);

        let shards = (0..count)
            .map(|_| Shard {
                inner: RwLock::new(ShardInner {
                    entries: HashTable::with_capacity(per_shard),
                }),
                strings: LockFreeStringArray::new(interner_id),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Self {
            interner_id,
            pool: MemoryPool::new(ARENA_CAPACITY, count as u32),
            shards,
            hash_builder: rustc_hash::FxBuildHasher::default(),
            shard_mask: (count - 1) as u64,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        self.pool.committed_bytes()
    }

    pub fn offset_of(&self, symbol: Symbol) -> Option<u32> {
        let shard_id = symbol.shard_id();
        if shard_id >= self.shards.len() {
            return None;
        }

        let chunk_idx = symbol.index() >> CHUNK_SHIFT;
        let slot_idx = symbol.index() & CHUNK_MASK;

        let chunk_ptr = self.shards[shard_id].strings.load_chunk(chunk_idx);
        if chunk_ptr.is_null() {
            return None;
        }

        let offset = unsafe { (*chunk_ptr).offsets[slot_idx].load(Ordering::Acquire) };
        if offset == 0 { None } else { Some(offset) }
    }

    pub fn resolve_raw_offset(bytes: &[u8], offset: u32) -> Option<&str> {
        let offset = offset as usize;

        if offset == 0 || offset + 4 > bytes.len() {
            return None;
        }

        let len_bytes: [u8; 4] = bytes[offset..offset + 4].try_into().ok()?;
        let len = u32::from_ne_bytes(len_bytes) as usize;

        if offset + 4 + len > bytes.len() {
            return None;
        }

        std::str::from_utf8(&bytes[offset + 4..offset + 4 + len]).ok()
    }

    pub fn from_bytes(bytes: &[u8]) -> Self {
        let shard_count =
            MemoryPool::read_shard_count_header(bytes).unwrap_or(DEFAULT_SHARD_COUNT as u32);

        let estimated_symbols = bytes.len() / 16;
        let interner = Self::with_capacity_and_shards(estimated_symbols, shard_count as usize);

        if bytes.len() <= 4 {
            return interner;
        }

        let mut cursor = 4;

        while cursor + 4 <= bytes.len() {
            let len_bytes: [u8; 4] = bytes[cursor..cursor + 4].try_into().unwrap();
            let len = u32::from_ne_bytes(len_bytes) as usize;

            if cursor + 4 + len > bytes.len() {
                break;
            }

            let str_slice = &bytes[cursor + 4..cursor + 4 + len];
            if let Ok(text) = std::str::from_utf8(str_slice) {
                interner.get_or_intern(text);
            }

            cursor += 4 + len;
        }

        interner
    }

    pub fn get_or_intern(&self, value: &str) -> Symbol {
        let hash = self.hash_builder.hash_one(value);
        let prefix = ShardInner::prefix4(value.as_bytes());

        let cache_index = (hash & CACHE_MASK) as usize;
        let hash_trunc = hash as u32;
        let cache_line_base = ((hash & CACHE_MASK) & !3) as usize;

        let cached = LOCAL_CACHE.with(|cache| {
            for i in 0..4 {
                if let Some(entry) = cache.entries[cache_line_base + i].get() {
                    if entry.interner_id == self.interner_id && entry.hash == hash_trunc {
                        let ptr = unsafe { self.pool.base_ptr().add(entry.offset as usize) };
                        let len = unsafe { std::ptr::read_unaligned(ptr as *const u32) as usize };
                        let cached_value = unsafe {
                            std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                                ptr.add(4),
                                len,
                            ))
                        };
                        if cached_value == value {
                            return Some(entry.symbol);
                        }
                    }
                }
            }
            None
        });

        if let Some(symbol) = cached {
            return symbol;
        }

        let shard_id = (hash & self.shard_mask) as usize;
        let shard = &self.shards[shard_id];
        let mut resolved = None;

        {
            let inner = shard.inner.read();
            let mut found_text = None;

            if let Some(&(_, _, symbol)) =
                inner
                    .entries
                    .find(hash, |&(cand_hash, cand_prefix, symbol)| {
                        if cand_hash != hash || cand_prefix != prefix {
                            return false;
                        }

                        if let Some(text) = shard.strings.get(symbol.index(), self.pool.base_ptr())
                        {
                            if text.as_bytes() == value.as_bytes() {
                                found_text = Some(text);
                                return true;
                            }
                        }
                        false
                    })
            {
                resolved = Some((found_text.unwrap(), symbol));
            }
        }

        let (text, symbol) = if let Some(res) = resolved {
            res
        } else {
            let mut inner = shard.inner.write();

            match inner.entries.entry(
                hash,
                |&(cand_hash, cand_prefix, symbol)| {
                    cand_hash == hash
                        && cand_prefix == prefix
                        && shard.strings.get(symbol.index(), self.pool.base_ptr()) == Some(value)
                },
                |&(cached_hash, _, _)| cached_hash,
            ) {
                Entry::Occupied(entry) => {
                    let symbol = entry.get().2;

                    // SAFETY: symbol came from a live hashtable entry, so its index was
                    // successfully pushed to shard.strings and is < len. No need to
                    // re-pay the len.load(Acquire) + branch that `get()` does.
                    let text = unsafe {
                        shard
                            .strings
                            .get_unchecked(symbol.index(), self.pool.base_ptr())
                    };

                    (text, symbol)
                }
                Entry::Vacant(entry) => {
                    let offset = self.pool.alloc(value);
                    let index = shard.strings.push(offset);
                    let symbol = Symbol::new(shard_id as u8, index);
                    entry.insert((hash, prefix, symbol));
                    let ptr = unsafe { self.pool.base_ptr().add(offset as usize) };
                    let slice = unsafe { std::slice::from_raw_parts(ptr.add(4), value.len()) };
                    (unsafe { std::str::from_utf8_unchecked(slice) }, symbol)
                }
            }
        };

        let offset = (text.as_ptr() as usize - self.pool.base_ptr() as usize) as u32;

        LOCAL_CACHE.with(|cache| {
            cache.entries[cache_index].set(Some(CacheEntry {
                interner_id: self.interner_id,
                offset,
                hash: hash_trunc,
                symbol,
            }));
        });

        symbol
    }

    pub fn get(&self, value: &str) -> Option<Symbol> {
        let hash = self.hash_builder.hash_one(value);
        let prefix = ShardInner::prefix4(value.as_bytes());

        let cache_index = (hash & CACHE_MASK) as usize;
        let hash_trunc = hash as u32;
        let cache_line_base = ((hash & CACHE_MASK) & !3) as usize;

        let cached = LOCAL_CACHE.with(|cache| {
            for i in 0..4 {
                if let Some(entry) = cache.entries[cache_line_base + i].get() {
                    if entry.interner_id == self.interner_id && entry.hash == hash_trunc {
                        let ptr = unsafe { self.pool.base_ptr().add(entry.offset as usize) };
                        let len = unsafe { std::ptr::read_unaligned(ptr as *const u32) as usize };
                        let cached_value = unsafe {
                            std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                                ptr.add(4),
                                len,
                            ))
                        };
                        if cached_value == value {
                            return Some(entry.symbol);
                        }
                    }
                }
            }
            None
        });

        if let Some(symbol) = cached {
            return Some(symbol);
        }

        let shard_id = (hash & self.shard_mask) as usize;
        let shard = &self.shards[shard_id];

        let inner = shard.inner.read();

        let symbol = inner
            .entries
            .find(hash, |&(cand_hash, cand_prefix, symbol)| {
                cand_hash == hash
                    && cand_prefix == prefix
                    && shard.strings.get(symbol.index(), self.pool.base_ptr()) == Some(value)
            })
            .map(|&(_, _, symbol)| symbol)?;

        let text = shard.strings.get(symbol.index(), self.pool.base_ptr())?;

        let offset = (text.as_ptr() as usize - self.pool.base_ptr() as usize) as u32;

        LOCAL_CACHE.with(|cache| {
            cache.entries[cache_index].set(Some(CacheEntry {
                interner_id: self.interner_id,
                offset,
                hash: hash_trunc,
                symbol,
            }));
        });

        Some(symbol)
    }

    pub fn resolve(&self, symbol: Symbol) -> Option<&str> {
        let shard_id = symbol.shard_id();

        if shard_id >= self.shards.len() {
            return None;
        }

        self.shards[shard_id]
            .strings
            .get(symbol.index(), self.pool.base_ptr())
    }

    pub unsafe fn resolve_unchecked(&self, symbol: Symbol) -> &str {
        unsafe {
            let shard = self.shards.get_unchecked(symbol.shard_id());
            shard
                .strings
                .get_unchecked(symbol.index(), self.pool.base_ptr())
        }
    }

    pub fn contains(&self, value: &str) -> bool {
        self.get(value).is_some()
    }

    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| shard.strings.len.load(Ordering::Acquire))
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn test_symbol_interner_basic() {
        let interner = SymbolInterner::with_capacity(100);

        let a = interner.get_or_intern("hello");
        let b = interner.get_or_intern("hello");
        let c = interner.get_or_intern("world");

        assert_eq!(a, b, "Identical strings should return the same symbol");
        assert_ne!(a, c, "Different strings should return different symbols");
        assert_eq!(interner.len(), 2);
        assert_eq!(interner.resolve(a), Some("hello"));
        assert_eq!(interner.resolve(c), Some("world"));
    }

    #[test]
    fn test_empty_string() {
        let interner = SymbolInterner::new();
        let a = interner.get_or_intern("");
        let b = interner.get_or_intern("");

        assert_eq!(a, b);
        assert_eq!(interner.resolve(a), Some(""));
    }

    #[test]
    fn test_unicode_strings() {
        let interner = SymbolInterner::new();
        let strings = ["🦀", "你好", "Привет", "こんにちは", "مرحبا"];

        let symbols: Vec<_> = strings.iter().map(|&s| interner.get_or_intern(s)).collect();

        for (i, &s) in strings.iter().enumerate() {
            assert_eq!(interner.resolve(symbols[i]), Some(s));
        }
    }

    #[test]
    fn test_long_strings() {
        let interner = SymbolInterner::new();
        let long_str_1 = "A".repeat(10_000);
        let long_str_2 = "B".repeat(10_000);

        let sym1 = interner.get_or_intern(&long_str_1);
        let sym2 = interner.get_or_intern(&long_str_2);

        assert_eq!(interner.resolve(sym1), Some(long_str_1.as_str()));
        assert_eq!(interner.resolve(sym2), Some(long_str_2.as_str()));
    }

    #[test]
    fn test_capacity_growth() {
        let interner = SymbolInterner::with_capacity(1);
        let count = 5_000;

        let mut symbols = Vec::with_capacity(count);
        for i in 0..count {
            symbols.push(interner.get_or_intern(&format!("grow-{i}")));
        }

        assert_eq!(interner.len(), count);
        for (i, &sym) in symbols.iter().enumerate() {
            assert_eq!(interner.resolve(sym), Some(format!("grow-{i}").as_str()));
        }
    }

    #[test]
    fn test_resolve_lock_free_matches_get() {
        let interner = SymbolInterner::new();
        let symbols: Vec<_> = (0..10_000)
            .map(|i| interner.get_or_intern(&format!("sym-{i}")))
            .collect();

        for (i, sym) in symbols.iter().enumerate() {
            assert_eq!(interner.resolve(*sym), Some(format!("sym-{i}").as_str()));
        }
    }

    #[test]
    fn test_concurrent_get_or_intern_same_value() {
        let interner = Arc::new(SymbolInterner::new());
        let num_threads = 16;
        let barrier = Arc::new(Barrier::new(num_threads));

        let handles: Vec<_> = (0..num_threads)
            .map(|_| {
                let interner = Arc::clone(&interner);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait(); // Maximize contention
                    interner.get_or_intern("contended")
                })
            })
            .collect();

        let mut symbols = Vec::new();
        for h in handles {
            symbols.push(h.join().unwrap());
        }

        let first = symbols[0];
        for &sym in &symbols[1..] {
            assert_eq!(first, sym);
        }
        assert_eq!(interner.len(), 1);
    }

    #[test]
    fn test_multiple_writers_distinct_values() {
        let interner = Arc::new(SymbolInterner::new());
        let num_threads = 8;
        let items_per_thread = 1_000;
        let barrier = Arc::new(Barrier::new(num_threads));

        let handles: Vec<_> = (0..num_threads)
            .map(|t_id| {
                let interner = Arc::clone(&interner);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    let mut local_syms = Vec::with_capacity(items_per_thread);
                    for i in 0..items_per_thread {
                        let s = format!("thread-{t_id}-item-{i}");
                        local_syms.push((s.clone(), interner.get_or_intern(&s)));
                    }
                    local_syms
                })
            })
            .collect();

        for h in handles {
            let local_syms = h.join().unwrap();
            for (string, sym) in local_syms {
                assert_eq!(interner.resolve(sym), Some(string.as_str()));
            }
        }

        assert_eq!(interner.len(), num_threads * items_per_thread);
    }

    #[test]
    fn test_multiple_writers_overlapping_values() {
        let interner = Arc::new(SymbolInterner::new());
        let num_threads = 8;
        let items = 1_000;
        let barrier = Arc::new(Barrier::new(num_threads));

        let handles: Vec<_> = (0..num_threads)
            .map(|_| {
                let interner = Arc::clone(&interner);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    for i in 0..items {
                        interner.get_or_intern(&format!("shared-{i}"));
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(interner.len(), items);
    }

    #[test]
    fn test_true_concurrent_resolve_while_interning() {
        let interner = Arc::new(SymbolInterner::new());

        let pre_interned: Vec<_> = (0..1_000)
            .map(|i| {
                let s = format!("pre-val-{i}");
                (interner.get_or_intern(&s), s)
            })
            .collect();

        let barrier = Arc::new(Barrier::new(5));

        let writer = {
            let interner = Arc::clone(&interner);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                for i in 0..5_000 {
                    interner.get_or_intern(&format!("new-val-{i}"));
                }
            })
        };

        let readers: Vec<_> = (0..4)
            .map(|_| {
                let interner = Arc::clone(&interner);
                let barrier = Arc::clone(&barrier);
                let symbols = pre_interned.clone();
                thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..10 {
                        for (sym, expected_str) in &symbols {
                            assert_eq!(interner.resolve(*sym), Some(expected_str.as_str()));
                        }
                    }
                })
            })
            .collect();

        writer.join().unwrap();
        for r in readers {
            r.join().unwrap();
        }
    }

    #[test]
    fn test_stress_mixed_workload() {
        let interner = Arc::new(SymbolInterner::with_capacity(10));
        let num_threads = 8;
        let iterations = 2_000;
        let barrier = Arc::new(Barrier::new(num_threads));

        let handles: Vec<_> = (0..num_threads)
            .map(|t_id| {
                let interner = Arc::clone(&interner);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    let mut my_symbols = Vec::new();

                    for i in 0..iterations {
                        let unique_str = format!("mix-{t_id}-{i}");
                        let sym = interner.get_or_intern(&unique_str);
                        my_symbols.push((sym, unique_str));

                        let shared_str = format!("shared-{}", i % 100);
                        interner.get_or_intern(&shared_str);

                        if i > 0 {
                            let resolve_idx = i / 2;
                            let (old_sym, ref old_str) = my_symbols[resolve_idx];
                            assert_eq!(interner.resolve(old_sym), Some(old_str.as_str()));
                        }
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(interner.len(), (num_threads * iterations) + 100);
    }
}
