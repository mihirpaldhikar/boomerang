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
use crate::StringInterner;
use crate::core::schema_builder::DatabaseSchemaBuilder;
use crate::core::string_interner::Symbol;
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

impl From<i16> for Action {
    fn from(value: i16) -> Self {
        Self::from(value as u8)
    }
}

impl From<Action> for i16 {
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
    pub(crate) interner: StringInterner,
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
    pub fn new(total_tables: usize, total_columns: usize) -> DatabaseSchemaBuilder {
        DatabaseSchemaBuilder::with_capacity(total_tables, total_columns)
    }

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

impl DatabaseSchema {
    pub fn encode(&self) -> Vec<u8> {
        let interner_bytes = self.interner.bytes();

        let mut estimated_cap = 32 // hash
            + 4 + interner_bytes.len() // interner
            + 4 + self.tables.len() * 18 // tables
            + 4 + self.by_name.len() * 2 // by_name
            + 4 + self.column_names.len() * 4 // column_names
            + 4 + self.constraint_pool.len() * 4 // constraint_pool
            + 4 + self.foreign_key_pool.len() * 14; // foreign_keys

        estimated_cap += 4 + self.columns.len() * 32;

        let mut buf = Vec::with_capacity(estimated_cap);

        buf.extend_from_slice(&self.hash);

        buf.extend_from_slice(&(interner_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(&interner_bytes);

        macro_rules! write_u32 {
            ($val:expr) => {
                buf.extend_from_slice(&($val as u32).to_le_bytes())
            };
        }
        macro_rules! write_u16 {
            ($val:expr) => {
                buf.extend_from_slice(&($val as u16).to_le_bytes())
            };
        }
        macro_rules! write_u8 {
            ($val:expr) => {
                buf.push($val as u8)
            };
        }
        macro_rules! write_sym {
            ($sym:expr) => {
                write_u32!($sym.into_raw())
            };
        }
        macro_rules! write_sym_opt {
            ($sym:expr) => {
                match $sym {
                    Some(s) => write_u32!(s.into_raw()),
                    None => write_u32!(0), // 0 is invalid for NonZeroU32, safe marker
                }
            };
        }

        write_u32!(self.tables.len());
        for t in self.tables.iter() {
            write_sym!(t.name);
            write_u32!(t.columns_start);
            write_u16!(t.columns_len);
            write_u16!(t.primary_key.unwrap_or(u16::MAX));
            write_u32!(t.fk_start);
            write_u16!(t.fk_len);
        }

        write_u32!(self.by_name.len());
        for &idx in self.by_name.iter() {
            write_u16!(idx);
        }

        write_u32!(self.columns.len());
        for c in self.columns.iter() {
            write_sym!(c.name);
            write_u8!(c.data_type.to_u8());

            if let ValueType::Custom(sym) = c.data_type {
                write_sym!(sym);
            }

            write_sym_opt!(c.default_value);
            write_u8!(c.flags.0);
            write_u32!(c.constraints.start);
            write_u16!(c.constraints.len);
            write_u32!(c.foreign_keys.start);
            write_u16!(c.foreign_keys.len);
        }

        write_u32!(self.column_names.len());
        for &sym in self.column_names.iter() {
            write_sym!(sym);
        }

        write_u32!(self.constraint_pool.len());
        for &sym in self.constraint_pool.iter() {
            write_sym!(sym);
        }

        write_u32!(self.foreign_key_pool.len());
        for fk in self.foreign_key_pool.iter() {
            write_sym!(fk.constraint_name);
            write_sym!(fk.namespace);
            write_u16!(fk.table);
            write_u16!(fk.column);
            write_u8!(fk.on_update);
            write_u8!(fk.on_delete);
        }

        buf
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut ptr = 0;
        let buf = bytes;

        macro_rules! read_bytes {
            ($len:expr) => {{
                if ptr + $len > buf.len() {
                    return None;
                }
                let slice = &buf[ptr..ptr + $len];
                ptr += $len;
                slice
            }};
        }
        macro_rules! read_u32 {
            () => {
                u32::from_le_bytes(read_bytes!(4).try_into().unwrap())
            };
        }
        macro_rules! read_u16 {
            () => {
                u16::from_le_bytes(read_bytes!(2).try_into().unwrap())
            };
        }
        macro_rules! read_u8 {
            () => {
                read_bytes!(1)[0]
            };
        }

        macro_rules! read_sym {
            () => {
                Symbol::from_raw(read_u32!())?
            };
        }
        macro_rules! read_sym_opt {
            () => {{
                let raw = read_u32!();
                if raw == 0 {
                    None
                } else {
                    Symbol::from_raw(raw)
                }
            }};
        }

        let mut hash = [0u8; 32];
        hash.copy_from_slice(read_bytes!(32));

        let interner_len = read_u32!() as usize;
        let interner_bytes = read_bytes!(interner_len);
        let interner = StringInterner::from_bytes(interner_bytes);

        let tables_len = read_u32!();
        let mut tables = Vec::with_capacity(tables_len as usize);
        for _ in 0..tables_len {
            tables.push(TableMeta {
                name: read_sym!(),
                columns_start: read_u32!(),
                columns_len: read_u16!(),
                primary_key: {
                    let v = read_u16!();
                    if v == u16::MAX { None } else { Some(v) }
                },
                fk_start: read_u32!(),
                fk_len: read_u16!(),
            });
        }

        let by_name_len = read_u32!();
        let mut by_name = Vec::with_capacity(by_name_len as usize);
        for _ in 0..by_name_len {
            by_name.push(read_u16!());
        }

        let columns_len = read_u32!();
        let mut columns = Vec::with_capacity(columns_len as usize);
        for _ in 0..columns_len {
            let name = read_sym!();
            let dt_tag = read_u8!();

            let data_type = match dt_tag {
                0 => ValueType::Null,
                1 => ValueType::Bool,
                2 => ValueType::Int,
                3 => ValueType::Float,
                4 => ValueType::Decimal,
                5 => ValueType::Text,
                6 => ValueType::Blob,
                7 => ValueType::Json,
                8 => ValueType::Uuid,
                9 => ValueType::Timestamp,
                10 => ValueType::TimestampTz,
                11 => ValueType::Date,
                12 => ValueType::Time,
                13 => ValueType::TimeTz,
                14 => ValueType::Custom(read_sym!()),
                15 => ValueType::BoolArray,
                16 => ValueType::IntArray,
                17 => ValueType::FloatArray,
                18 => ValueType::DecimalArray,
                19 => ValueType::TextArray,
                _ => return None,
            };

            columns.push(Column {
                name,
                data_type,
                default_value: read_sym_opt!(),
                flags: ColumnFlags::from_bits(read_u8!()),
                constraints: PoolRange {
                    start: read_u32!(),
                    len: read_u16!(),
                },
                foreign_keys: PoolRange {
                    start: read_u32!(),
                    len: read_u16!(),
                },
            });
        }

        let col_names_len = read_u32!();
        let mut column_names = Vec::with_capacity(col_names_len as usize);
        for _ in 0..col_names_len {
            column_names.push(read_sym!());
        }

        let cpool_len = read_u32!();
        let mut constraint_pool = Vec::with_capacity(cpool_len as usize);
        for _ in 0..cpool_len {
            constraint_pool.push(read_sym!());
        }

        let fk_len = read_u32!();
        let mut foreign_key_pool = Vec::with_capacity(fk_len as usize);
        for _ in 0..fk_len {
            foreign_key_pool.push(ForeignKey {
                constraint_name: read_sym!(),
                namespace: read_sym!(),
                table: read_u16!(),
                column: read_u16!(),
                on_update: Action::from(read_u8!()),
                on_delete: Action::from(read_u8!()),
            });
        }

        Some(DatabaseSchema {
            interner,
            tables: tables.into_boxed_slice(),
            by_name: by_name.into_boxed_slice(),
            columns: columns.into_boxed_slice(),
            column_names: column_names.into_boxed_slice(),
            constraint_pool: constraint_pool.into_boxed_slice(),
            foreign_key_pool: foreign_key_pool.into_boxed_slice(),
            hash,
        })
    }
}

impl DatabaseSchema {
    pub unsafe fn decode_unchecked(bytes: &[u8]) -> Self {
        unsafe {
            let mut ptr = bytes.as_ptr();

            macro_rules! read_u32 {
                () => {{
                    let val = std::ptr::read_unaligned(ptr.cast::<u32>());
                    ptr = ptr.add(4);
                    u32::from_le(val)
                }};
            }
            macro_rules! read_u16 {
                () => {{
                    let val = std::ptr::read_unaligned(ptr.cast::<u16>());
                    ptr = ptr.add(2);
                    u16::from_le(val)
                }};
            }
            macro_rules! read_u8 {
                () => {{
                    let val = *ptr;
                    ptr = ptr.add(1);
                    val
                }};
            }
            macro_rules! read_sym {
                () => {
                    Symbol::from_raw(read_u32!()).unwrap_unchecked()
                };
            }
            macro_rules! read_sym_opt {
                () => {{
                    let raw = read_u32!();
                    if raw == 0 {
                        None
                    } else {
                        Some(Symbol::from_raw(raw).unwrap_unchecked())
                    }
                }};
            }

            macro_rules! read_vec {
                ($ty:ty, $len:expr, $block:expr) => {{
                    let len = $len as usize;
                    let mut vec: Vec<$ty> = Vec::with_capacity(len);
                    let vec_ptr = vec.as_mut_ptr();
                    for i in 0..len {
                        std::ptr::write(vec_ptr.add(i), $block);
                    }
                    vec.set_len(len);
                    vec.into_boxed_slice()
                }};
            }

            let mut hash = [0u8; 32];
            std::ptr::copy_nonoverlapping(ptr, hash.as_mut_ptr(), 32);
            ptr = ptr.add(32);

            let interner_len = read_u32!() as usize;
            let interner_bytes = std::slice::from_raw_parts(ptr, interner_len);
            ptr = ptr.add(interner_len);

            let interner = StringInterner::from_bytes_unchecked(interner_bytes);

            let tables = read_vec!(
                TableMeta,
                read_u32!(),
                TableMeta {
                    name: read_sym!(),
                    columns_start: read_u32!(),
                    columns_len: read_u16!(),
                    primary_key: {
                        let v = read_u16!();
                        if v == u16::MAX { None } else { Some(v) }
                    },
                    fk_start: read_u32!(),
                    fk_len: read_u16!(),
                }
            );

            let by_name = read_vec!(u16, read_u32!(), read_u16!());

            let columns = read_vec!(
                Column,
                read_u32!(),
                Column {
                    name: read_sym!(),
                    data_type: match read_u8!() {
                        0 => ValueType::Null,
                        1 => ValueType::Bool,
                        2 => ValueType::Int,
                        3 => ValueType::Float,
                        4 => ValueType::Decimal,
                        5 => ValueType::Text,
                        6 => ValueType::Blob,
                        7 => ValueType::Json,
                        8 => ValueType::Uuid,
                        9 => ValueType::Timestamp,
                        10 => ValueType::TimestampTz,
                        11 => ValueType::Date,
                        12 => ValueType::Time,
                        13 => ValueType::TimeTz,
                        14 => ValueType::Custom(read_sym!()),
                        15 => ValueType::BoolArray,
                        16 => ValueType::IntArray,
                        17 => ValueType::FloatArray,
                        18 => ValueType::DecimalArray,
                        19 => ValueType::TextArray,
                        _ => std::hint::unreachable_unchecked(),
                    },
                    default_value: read_sym_opt!(),
                    flags: ColumnFlags::from_bits(read_u8!()),
                    constraints: PoolRange {
                        start: read_u32!(),
                        len: read_u16!()
                    },
                    foreign_keys: PoolRange {
                        start: read_u32!(),
                        len: read_u16!()
                    },
                }
            );

            let column_names = read_vec!(Symbol, read_u32!(), read_sym!());
            let constraint_pool = read_vec!(Symbol, read_u32!(), read_sym!());

            let foreign_key_pool = read_vec!(
                ForeignKey,
                read_u32!(),
                ForeignKey {
                    constraint_name: read_sym!(),
                    namespace: read_sym!(),
                    table: read_u16!(),
                    column: read_u16!(),
                    on_update: Action::from(read_u8!()),
                    on_delete: Action::from(read_u8!()),
                }
            );

            DatabaseSchema {
                interner,
                tables,
                by_name,
                columns,
                column_names,
                constraint_pool,
                foreign_key_pool,
                hash,
            }
        }
    }
}
