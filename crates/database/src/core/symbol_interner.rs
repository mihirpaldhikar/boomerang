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
use hashbrown::HashTable;
use hashbrown::hash_table::Entry;
use parking_lot::RwLock;
use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::cell::{Cell, RefCell};
use std::hash::{BuildHasher, Hash};
use std::num::NonZeroU32;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

const DEFAULT_SHARD_COUNT: usize = 16;
const MIN_BUCKET_BYTES: usize = 4 * 1024;
const MAX_BUCKET_BYTES: usize = 256 * 1024;
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

static NEXT_INTERNER_ID: AtomicUsize = AtomicUsize::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct Symbol(NonZeroU32);

impl Symbol {
    const INDEX_MASK: u32 = 0x00FF_FFFF; // Bottom 24 bits.

    fn new(shard_id: u8, index: usize) -> Self {
        assert!(index < Self::INDEX_MASK as usize, "Shard index overflow");

        let raw = ((shard_id as u32) << 24) | ((index as u32) + 1);
        Symbol(unsafe { NonZeroU32::new_unchecked(raw) })
    }

    #[inline]
    fn shard_id(&self) -> usize {
        (self.0.get() >> 24) as usize
    }

    #[inline]
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

struct Bucket {
    ptr: NonNull<u8>,
    layout: Layout,
    cursor: usize,
}

// Safety: Bucket owns its raw memory buffer, so it can be safely shared between threads.
unsafe impl Send for Bucket {}
unsafe impl Sync for Bucket {}

impl Bucket {
    fn new(capacity: usize) -> Self {
        let layout = Layout::array::<u8>(capacity).expect("valid layout");
        assert!(capacity > 0, "capacity must be greater than 0");

        let ptr = unsafe { alloc(layout) };
        let ptr = NonNull::new(ptr).unwrap_or_else(|| handle_alloc_error(layout));

        Self {
            ptr,
            layout,
            cursor: 0,
        }
    }

    #[inline]
    fn capacity(&self) -> usize {
        self.layout.size()
    }

    fn alloc(&mut self, value: &str) -> Option<&'static str> {
        let len = value.len();

        if self.capacity() - self.cursor < len + 4 {
            return None;
        }

        unsafe {
            let start = self.ptr.as_ptr().add(self.cursor);

            std::ptr::write_unaligned(start as *mut u32, len as u32);

            let string_start = start.add(4);
            std::ptr::copy_nonoverlapping(value.as_ptr(), string_start, len);

            self.cursor += len + 4;

            let slice = std::slice::from_raw_parts(string_start, len);
            Some(std::str::from_utf8_unchecked(slice))
        }
    }
}

impl Drop for Bucket {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

struct Storage {
    buckets: Vec<Bucket>,
}

impl Storage {
    fn alloc(&mut self, value: &str) -> &'static str {
        let len = value.len();

        if let Some(bucket) = self.buckets.last_mut() {
            if let Some(text) = bucket.alloc(value) {
                return text;
            }
        }

        let next_cap = self
            .buckets
            .last()
            .map_or(MIN_BUCKET_BYTES, |bucket| {
                (bucket.capacity() * 2).min(MAX_BUCKET_BYTES)
            })
            .max(len + 4);

        let mut new_bucket = Bucket::new(next_cap);
        let text = new_bucket
            .alloc(value)
            .expect("new bucket has enough capacity");

        self.buckets.push(new_bucket);

        text
    }
}

struct ThreadLocalCache {
    entries: [Option<(usize, u64, *const u8, usize, Symbol)>; CACHE_CAPACITY],
}

impl ThreadLocalCache {
    fn new() -> Self {
        Self {
            entries: [None; CACHE_CAPACITY],
        }
    }
}

thread_local! {
    static LOCAL_CACHE: RefCell<ThreadLocalCache> = RefCell::new(ThreadLocalCache::new());
    static LAST_CHUNK: Cell<Option<(usize, usize, usize, *const StringChunk)>> = Cell::new(None);
}

struct StringChunk {
    ptrs: [AtomicPtr<u8>; CHUNK_SIZE],
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
}

struct LockFreeStringArray {
    interner_id: usize,
    chunks: Box<[AtomicPtr<StringChunk>]>,
    len: AtomicUsize,
}

unsafe impl Send for LockFreeStringArray {}
unsafe impl Sync for LockFreeStringArray {}

impl LockFreeStringArray {
    fn new(interner_id: usize) -> Self {
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
            cell.set(Some((self.interner_id, self_addr, chunk_idx, ptr)));
            ptr
        })
    }

    fn get(&self, index: usize) -> Option<&'static str> {
        if index >= self.len.load(Ordering::Acquire) {
            return None;
        }

        let chunk_idx = index >> CHUNK_SHIFT;
        let slot_idx = index & CHUNK_MASK;

        let chunk_ptr = self.load_chunk(chunk_idx);
        if chunk_ptr.is_null() {
            let fresh = self.chunks[chunk_idx].load(Ordering::Acquire);
            if fresh.is_null() {
                return None;
            }
            return self.get_from_chunk(fresh as *const StringChunk, slot_idx);
        }

        self.get_from_chunk(chunk_ptr, slot_idx)
    }

    #[inline]
    fn get_from_chunk(
        &self,
        chunk_ptr: *const StringChunk,
        slot_idx: usize,
    ) -> Option<&'static str> {
        let chunk = unsafe { &*chunk_ptr };

        let ptr = chunk.ptrs[slot_idx].load(Ordering::Acquire);
        if ptr.is_null() {
            return None;
        }

        let len = unsafe { std::ptr::read_unaligned(ptr.sub(4) as *const u32) as usize };

        unsafe {
            let slice = std::slice::from_raw_parts(ptr, len);
            Some(std::str::from_utf8_unchecked(slice))
        }
    }

    #[inline]
    unsafe fn get_unchecked(&self, index: usize) -> &'static str {
        let chunk_idx = index >> CHUNK_SHIFT;
        let slot_idx = index & CHUNK_MASK;

        let chunk_ptr = self.load_chunk(chunk_idx);
        let chunk = unsafe { &*chunk_ptr };
        let ptr = chunk.ptrs[slot_idx].load(Ordering::Acquire);

        let len = unsafe { std::ptr::read_unaligned(ptr.sub(4) as *const u32) as usize };

        unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(ptr, len)) }
    }

    fn push(&self, value: &'static str) -> usize {
        let index = self.len.load(Ordering::Relaxed);
        let chunk_idx = index >> CHUNK_SHIFT;
        let slot_idx = index & CHUNK_MASK;

        let mut chunk_ptr = self.chunks[chunk_idx].load(Ordering::Acquire);
        if chunk_ptr.is_null() {
            let new_chunk = Box::into_raw(StringChunk::new_boxed());
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

        chunk.ptrs[slot_idx].store(value.as_ptr() as *mut u8, Ordering::Release);

        self.len.store(index + 1, Ordering::Release);

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
    entries: HashTable<(u64, Symbol)>,
    storage: Storage,
}

struct Shard {
    inner: RwLock<ShardInner>,
    strings: LockFreeStringArray,
}

pub struct SymbolInterner {
    interner_id: usize,
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
                    storage: Storage {
                        buckets: Vec::new(),
                    },
                }),
                strings: LockFreeStringArray::new(interner_id),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Self {
            interner_id,
            shards,
            hash_builder: rustc_hash::FxBuildHasher::default(),
            shard_mask: (count - 1) as u64,
        }
    }

    pub fn get_or_intern(&self, value: &str) -> Symbol {
        let hash = self.hash_builder.hash_one(value);
        let cache_index = (hash & CACHE_MASK) as usize;

        let mut hit = None;
        LOCAL_CACHE.with(|cache| {
            let cache = cache.borrow();
            if let Some((cached_id, cached_hash, ptr, len, symbol)) = cache.entries[cache_index] {
                if cached_id == self.interner_id && cached_hash == hash {
                    let cached_value = unsafe {
                        let slice = std::slice::from_raw_parts(ptr, len);
                        std::str::from_utf8_unchecked(slice)
                    };

                    if cached_value == value {
                        hit = Some(symbol);
                    }
                }
            }
        });

        if let Some(symbol) = hit {
            return symbol;
        }

        let shard_id = (hash & self.shard_mask) as usize;
        let shard = &self.shards[shard_id];
        let mut resolved = None;

        {
            let inner = shard.inner.read();
            let mut found_text = None;
            if let Some(&(_, symbol)) = inner.entries.find(hash, |&(_, symbol)| {
                if let Some(text) = shard.strings.get(symbol.index()) {
                    if text == value {
                        found_text = Some(text);
                        return true;
                    }
                }
                false
            }) {
                resolved = Some((found_text.unwrap(), symbol));
            }
        }

        let (text, symbol) = if let Some(res) = resolved {
            res
        } else {
            let mut inner = shard.inner.write();

            let ShardInner { entries, storage } = &mut *inner;

            match entries.entry(
                hash,
                |&(_, symbol)| shard.strings.get(symbol.index()) == Some(value),
                |&(cached_hash, _)| cached_hash,
            ) {
                Entry::Occupied(entry) => {
                    let symbol = entry.get().1;
                    (shard.strings.get(symbol.index()).unwrap(), symbol)
                }
                Entry::Vacant(entry) => {
                    let text = storage.alloc(value);
                    let index = shard.strings.push(text);

                    let symbol = Symbol::new(shard_id as u8, index);
                    entry.insert((hash, symbol));
                    (text, symbol)
                }
            }
        };

        LOCAL_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            cache.entries[cache_index] =
                Some((self.interner_id, hash, text.as_ptr(), text.len(), symbol));
        });

        symbol
    }

    pub fn get(&self, value: &str) -> Option<Symbol> {
        let hash = self.hash_builder.hash_one(value);
        let cache_index = (hash & CACHE_MASK) as usize;

        let mut hit = None;
        LOCAL_CACHE.with(|cache| {
            let cache = cache.borrow();
            if let Some((cached_id, cached_hash, ptr, len, symbol)) = cache.entries[cache_index] {
                if cached_id == self.interner_id && cached_hash == hash {
                    let cached_value = unsafe {
                        let slice = std::slice::from_raw_parts(ptr, len);
                        std::str::from_utf8_unchecked(slice)
                    };

                    if cached_value == value {
                        hit = Some(symbol);
                    }
                }
            }
        });

        if let Some(symbol) = hit {
            return Some(symbol);
        }

        let shard_id = (hash & self.shard_mask) as usize;
        let shard = &self.shards[shard_id];

        let inner = shard.inner.read();

        let symbol = inner
            .entries
            .find(hash, |&(_, symbol)| {
                shard.strings.get(symbol.index()) == Some(value)
            })
            .map(|&(_, symbol)| symbol)?;

        let text = shard.strings.get(symbol.index())?;

        LOCAL_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            cache.entries[cache_index] =
                Some((self.interner_id, hash, text.as_ptr(), text.len(), symbol));
        });

        Some(symbol)
    }

    pub fn resolve(&self, symbol: Symbol) -> Option<&str> {
        let shard_id = symbol.shard_id();

        if shard_id >= self.shards.len() {
            unsafe { std::hint::unreachable_unchecked() }
        }

        self.shards[shard_id].strings.get(symbol.index())
    }

    pub unsafe fn resolve_unchecked(&self, symbol: Symbol) -> &str {
        unsafe {
            let shard = self.shards.get_unchecked(symbol.shard_id());
            shard.strings.get_unchecked(symbol.index())
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
