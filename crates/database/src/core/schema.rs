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
use crate::SymbolInterner;
use crate::core::symbol_interner::Symbol;
use crate::core::value::ValueType;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Action {
    NoAction = 0,
    Cascade = 1,
    SetNull = 2,
    SetDefault = 3,
    Restrict = 4,
}

impl From<u8> for Action {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::NoAction,
            1 => Self::Cascade,
            2 => Self::SetNull,
            3 => Self::SetDefault,
            4 => Self::Restrict,
            _ => unreachable!(),
        }
    }
}

impl From<Action> for u8 {
    fn from(value: Action) -> Self {
        match value {
            Action::NoAction => 0,
            Action::Cascade => 1,
            Action::SetNull => 2,
            Action::SetDefault => 3,
            Action::Restrict => 4,
        }
    }
}

impl From<u16> for Action {
    fn from(value: u16) -> Self {
        Self::from(value as u8)
    }
}

impl From<Action> for u16 {
    fn from(value: Action) -> Self {
        match value {
            Action::NoAction => 0,
            Action::Cascade => 1,
            Action::SetNull => 2,
            Action::SetDefault => 3,
            Action::Restrict => 4,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ForeignKey {
    pub(crate) constraint_name: Symbol,
    pub(crate) namespace: Symbol,
    pub(crate) table: u16,
    pub(crate) column: u16,
    pub(crate) on_update: Action,
    pub(crate) on_delete: Action,
}

impl ForeignKey {
    #[inline]
    pub fn constraint_name(&self) -> Symbol {
        self.constraint_name
    }

    #[inline]
    pub fn namespace(&self) -> Symbol {
        self.namespace
    }

    #[inline]
    pub fn table_index(&self) -> usize {
        self.table as usize
    }

    #[inline]
    pub fn column_index(&self) -> usize {
        self.column as usize
    }

    #[inline]
    pub fn on_update(&self) -> Action {
        self.on_update
    }

    #[inline]
    pub fn on_delete(&self) -> Action {
        self.on_delete
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(transparent)]
pub struct ColumnFlags(u8);

impl ColumnFlags {
    pub const NULLABLE: Self = Self(1);
    pub const PRIMARY_KEY: Self = Self(1 << 1);
    pub const UNIQUE: Self = Self(1 << 2);

    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[inline]
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    #[inline]
    pub const fn set(&mut self, other: Self, enabled: bool) {
        if enabled {
            self.0 |= other.0;
        } else {
            self.0 &= !other.0;
        }
    }

    #[inline]
    pub(crate) const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PoolRange {
    pub(crate) start: u32,
    pub(crate) len: u16,
}

impl PoolRange {
    #[inline]
    pub(crate) fn as_range(self) -> std::ops::Range<usize> {
        self.start as usize..self.start as usize + self.len as usize
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Column {
    pub(crate) name: Symbol,
    pub(crate) data_type: ValueType,
    pub(crate) default_value: Option<Symbol>,
    pub(crate) flags: ColumnFlags,
    pub(crate) constraints: PoolRange,
    pub(crate) foreign_keys: PoolRange,
}

impl Column {
    #[inline]
    pub fn name_symbol(&self) -> Symbol {
        self.name
    }

    #[inline]
    pub fn data_type(&self) -> ValueType {
        self.data_type
    }

    #[inline]
    pub fn default_value(&self) -> Option<Symbol> {
        self.default_value
    }

    #[inline]
    pub fn is_nullable(&self) -> bool {
        self.flags.contains(ColumnFlags::NULLABLE)
    }

    #[inline]
    pub fn is_primary_key(&self) -> bool {
        self.flags.contains(ColumnFlags::PRIMARY_KEY)
    }

    #[inline]
    pub fn is_unique(&self) -> bool {
        self.flags.contains(ColumnFlags::UNIQUE)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TableMeta {
    pub(crate) name: Symbol,
    pub(crate) columns_start: u32,
    pub(crate) columns_len: u16,
    pub(crate) primary_key: Option<u16>,
    pub(crate) fk_start: u32,
    pub(crate) fk_len: u16,
}

pub struct DatabaseSchema {
    pub(crate) tables: Box<[TableMeta]>,
    pub(crate) by_name: Box<[u16]>,
    pub(crate) columns: Box<[Column]>,
    pub(crate) column_names: Box<[Symbol]>,
    pub(crate) constraint_pool: Box<[Symbol]>,
    pub(crate) foreign_key_pool: Box<[ForeignKey]>,
    pub(crate) hash: [u8; 32],
    pub(crate) interner: SymbolInterner,
}

#[derive(Clone, Copy)]
pub struct ColumnRef<'a> {
    pub(crate) schema: &'a DatabaseSchema,
    pub(crate) index: usize,
}

impl<'a> ColumnRef<'a> {
    #[inline(always)]
    fn meta(&self) -> &'a Column {
        &self.schema.columns[self.index]
    }

    #[inline]
    pub fn name(&self) -> &'a str {
        self.schema
            .interner
            .resolve(self.meta().name)
            .expect("column symbol not from the schema")
    }

    #[inline]
    pub fn name_symbol(&self) -> Symbol {
        self.meta().name
    }

    #[inline]
    pub fn data_type(&self) -> &ValueType {
        &self.meta().data_type
    }

    #[inline]
    pub fn default_value(&self) -> Option<Symbol> {
        self.meta().default_value
    }

    #[inline]
    pub fn is_nullable(&self) -> bool {
        self.meta().is_nullable()
    }

    #[inline]
    pub fn is_primary_key(&self) -> bool {
        self.meta().is_primary_key()
    }

    #[inline]
    pub fn is_unique(&self) -> bool {
        self.meta().is_unique()
    }

    #[inline]
    pub fn constraints(&self) -> &'a [Symbol] {
        &self.schema.constraint_pool[self.meta().constraints.as_range()]
    }

    #[inline]
    pub fn foreign_keys(&self) -> &'a [ForeignKey] {
        &self.schema.foreign_key_pool[self.meta().foreign_keys.as_range()]
    }
}

#[derive(Clone, Copy)]
pub struct TableRef<'a> {
    pub(crate) schema: &'a DatabaseSchema,
    pub(crate) index: u16,
}

impl<'a> TableRef<'a> {
    #[inline]
    pub(crate) fn meta(&self) -> &'a TableMeta {
        &self.schema.tables[self.index as usize]
    }

    #[inline]
    fn columns_range(&self) -> std::ops::Range<usize> {
        let m = self.meta();
        m.columns_start as usize..m.columns_start as usize + m.columns_len as usize
    }

    pub fn name(&self) -> &'a str {
        self.schema
            .interner
            .resolve(self.meta().name)
            .expect("table symbol not from the schema")
    }

    #[inline]
    pub fn name_symbol(&self) -> Symbol {
        self.meta().name
    }

    #[inline]
    pub fn topo_index(&self) -> usize {
        self.index as usize
    }

    #[inline]
    pub fn columns(&self) -> impl ExactSizeIterator<Item = ColumnRef<'a>> {
        let schema = self.schema;
        self.columns_range()
            .map(move |index| ColumnRef { schema, index })
    }

    #[inline]
    pub fn column_names(&self) -> &'a [Symbol] {
        &self.schema.column_names[self.columns_range()]
    }

    #[inline]
    pub fn position(&self, name: Symbol) -> Option<usize> {
        self.column_names().iter().position(|&c| c == name)
    }

    pub fn column(&self, name: &str) -> Option<ColumnRef<'a>> {
        let symbol = self.schema.interner.get(name)?;
        self.column_by_symbol(symbol)
    }

    #[inline]
    pub fn column_by_symbol(&self, name: Symbol) -> Option<ColumnRef<'a>> {
        let local_index = self.position(name)?;
        Some(ColumnRef {
            schema: self.schema,
            index: self.columns_range().start + local_index,
        })
    }

    pub fn primary_key(&self) -> Option<ColumnRef<'a>> {
        self.meta().primary_key.map(|i| ColumnRef {
            schema: self.schema,
            index: self.columns_range().start + i as usize,
        })
    }

    #[inline]
    pub fn foreign_keys(&self) -> &'a [ForeignKey] {
        let m = self.meta();
        &self.schema.foreign_key_pool[m.fk_start as usize..m.fk_start as usize + m.fk_len as usize]
    }

    #[inline]
    pub fn referenced_table(&self, fk: &ForeignKey) -> TableRef<'a> {
        TableRef {
            schema: self.schema,
            index: fk.table,
        }
    }
}

impl DatabaseSchema {
    #[inline(always)]
    pub fn resolve_symbol(&self, symbol: Symbol) -> Option<&str> {
        self.interner.resolve(symbol)
    }
    #[inline]
    pub fn len(&self) -> usize {
        self.tables.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    pub fn tables(&self) -> impl ExactSizeIterator<Item = TableRef<'_>> + DoubleEndedIterator {
        (0..self.tables.len() as u16).map(|index| TableRef {
            schema: self,
            index,
        })
    }

    #[inline]
    pub fn table_at(&self, index: usize) -> Option<TableRef<'_>> {
        (index < self.tables.len()).then(|| TableRef {
            schema: self,
            index: index as u16,
        })
    }

    pub fn table(&self, name: &str) -> Option<TableRef<'_>> {
        let symbol = self.interner.get(name)?;
        self.table_by_symbol(symbol)
    }

    pub fn table_by_symbol(&self, symbol: Symbol) -> Option<TableRef<'_>> {
        let slot = self
            .by_name
            .binary_search_by_key(&symbol, |&i| self.tables[i as usize].name)
            .ok()?;
        self.table_at(self.by_name[slot] as usize)
    }

    pub fn independent_tables(&self) -> impl Iterator<Item = TableRef<'_>> {
        self.tables().filter(|t| {
            t.foreign_keys()
                .iter()
                .all(|fk| fk.table_index() == t.index as usize)
        })
    }

    pub fn leaf_tables(&self) -> Vec<TableRef<'_>> {
        let mut referenced = vec![false; self.tables.len()];
        for (i, t) in self.tables.iter().enumerate() {
            let fks = &self.foreign_key_pool
                [t.fk_start as usize..t.fk_start as usize + t.fk_len as usize];
            for fk in fks {
                if fk.table_index() != i {
                    referenced[fk.table_index()] = true;
                }
            }
        }
        referenced
            .into_iter()
            .enumerate()
            .filter_map(|(i, r)| (!r).then(|| self.table_at(i))?)
            .collect()
    }

    #[inline]
    pub fn hash(&self) -> &[u8; 32] {
        &self.hash
    }
}
