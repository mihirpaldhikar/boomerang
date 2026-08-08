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
use crate::core::schema::{
    Action, Column, ColumnFlags, DatabaseSchema, ForeignKey, PoolRange, TableMeta,
};
use crate::core::value::ValueType;
use crate::{Symbol, SymbolInterner};
use std::collections::VecDeque;

pub struct RawForeignKey<'a> {
    pub constraint_name: &'a str,
    pub namespace: &'a str,
    pub referenced_table: &'a str,
    pub referenced_column: &'a str,
    pub on_update: Action,
    pub on_delete: Action,
}

struct UnresolvedFk {
    pool_index: usize,
    source_table_idx: usize,
    ref_table_sym: Symbol,
    ref_column_sym: Symbol,
}

#[derive(Default)]
pub struct DatabaseSchemaBuilder {
    interner: SymbolInterner,
    tables: Vec<TableMeta>,
    columns: Vec<Column>,
    column_names: Vec<Symbol>,
    constraint_pool: Vec<Symbol>,
    foreign_key_pool: Vec<ForeignKey>,

    // Current Table grouping state
    current_table: Option<Symbol>,
    current_table_cols_start: u32,
    current_table_fk_start: u32,
    current_table_pk: Option<u16>,

    // Deferred resolution
    unresolved_fks: Vec<UnresolvedFk>,
}

impl DatabaseSchemaBuilder {
    pub(crate) fn with_capacity(tables_cap: usize, columns_cap: usize) -> Self {
        Self {
            interner: SymbolInterner::with_capacity(columns_cap + tables_cap),
            tables: Vec::with_capacity(tables_cap),
            columns: Vec::with_capacity(columns_cap),
            column_names: Vec::with_capacity(columns_cap),
            constraint_pool: Vec::with_capacity(columns_cap * 2),
            foreign_key_pool: Vec::with_capacity(tables_cap * 2),
            current_table: None,
            current_table_cols_start: 0,
            current_table_fk_start: 0,
            current_table_pk: None,
            unresolved_fks: Vec::with_capacity(tables_cap * 2),
        }
    }

    pub fn add_row<'a>(
        &mut self,
        table_name: &'a str,
        column_name: &'a str,
        data_type: ValueType,
        nullable: bool,
        is_primary: bool,
        is_unique: bool,
        default_expr: Option<&'a str>,
        foreign_keys: impl IntoIterator<Item = RawForeignKey<'a>>,
        check_defs: impl IntoIterator<Item = &'a str>,
        domain_checks: impl IntoIterator<Item = &'a str>,
    ) {
        let t_sym = self.interner.get_or_intern(table_name);

        if Some(t_sym) != self.current_table {
            self.finish_table();
            self.current_table = Some(t_sym);
            self.current_table_cols_start = self.columns.len() as u32;
            self.current_table_fk_start = self.foreign_key_pool.len() as u32;
            self.current_table_pk = None;
        }

        let col_sym = self.interner.get_or_intern(column_name);
        let col_idx_in_table = (self.columns.len() - self.current_table_cols_start as usize) as u16;

        let mut flags = ColumnFlags::default();
        flags.set(ColumnFlags::NULLABLE, nullable);
        flags.set(ColumnFlags::PRIMARY_KEY, is_primary);
        flags.set(ColumnFlags::UNIQUE, is_unique);

        if is_primary {
            self.current_table_pk = Some(col_idx_in_table);
        }

        let constraints_start = self.constraint_pool.len() as u32;
        for chk in check_defs.into_iter().chain(domain_checks) {
            self.constraint_pool.push(self.interner.get_or_intern(chk));
        }
        let constraints_len = (self.constraint_pool.len() as u32 - constraints_start) as u16;

        let fks_start = self.foreign_key_pool.len() as u32;
        for fk in foreign_keys {
            let constraint_sym = self.interner.get_or_intern(fk.constraint_name);
            let ns_sym = self.interner.get_or_intern(fk.namespace);
            let ref_table_sym = self.interner.get_or_intern(fk.referenced_table);
            let ref_column_sym = self.interner.get_or_intern(fk.referenced_column);

            let pool_index = self.foreign_key_pool.len();
            let source_table_idx = self.tables.len();

            self.foreign_key_pool.push(ForeignKey {
                constraint_name: constraint_sym,
                namespace: ns_sym,
                table: 0,
                column: 0,
                on_update: fk.on_update,
                on_delete: fk.on_delete,
            });

            self.unresolved_fks.push(UnresolvedFk {
                pool_index,
                source_table_idx,
                ref_table_sym,
                ref_column_sym,
            });
        }
        let fks_len = (self.foreign_key_pool.len() as u32 - fks_start) as u16;

        self.columns.push(Column {
            name: col_sym,
            data_type,
            default_value: default_expr.map(|d| self.interner.get_or_intern(d)),
            flags,
            constraints: PoolRange {
                start: constraints_start,
                len: constraints_len,
            },
            foreign_keys: PoolRange {
                start: fks_start,
                len: fks_len,
            },
        });

        self.column_names.push(col_sym);
    }

    #[inline]
    fn finish_table(&mut self) {
        if let Some(name) = self.current_table {
            let columns_len = (self.columns.len() as u32 - self.current_table_cols_start) as u16;
            let fk_len = (self.foreign_key_pool.len() as u32 - self.current_table_fk_start) as u16;

            self.tables.push(TableMeta {
                name,
                columns_start: self.current_table_cols_start,
                columns_len,
                primary_key: self.current_table_pk,
                fk_start: self.current_table_fk_start,
                fk_len,
            });
        }
    }

    pub fn build(mut self, hash: [u8; 32]) -> DatabaseSchema {
        self.finish_table();
        let num_tables = self.tables.len();

        let mut old_by_name: Vec<u16> = (0..num_tables as u16).collect();
        old_by_name.sort_unstable_by_key(|&i| self.tables[i as usize].name);

        let mut in_degree = vec![0; num_tables];
        let mut graph = vec![Vec::new(); num_tables];

        let mut resolved_fks_info = Vec::with_capacity(self.unresolved_fks.len());

        for unresolved in self.unresolved_fks {
            let table_slot = old_by_name
                .binary_search_by_key(&unresolved.ref_table_sym, |&i| self.tables[i as usize].name);

            if let Ok(slot) = table_slot {
                let ref_table_old_idx = old_by_name[slot] as usize;
                let table_meta = &self.tables[ref_table_old_idx];

                let cols_start = table_meta.columns_start as usize;
                let cols_end = cols_start + table_meta.columns_len as usize;
                let col_names_slice = &self.column_names[cols_start..cols_end];

                if let Some(col_idx_in_table) = col_names_slice
                    .iter()
                    .position(|&c| c == unresolved.ref_column_sym)
                {
                    resolved_fks_info.push((
                        unresolved.pool_index,
                        unresolved.source_table_idx,
                        ref_table_old_idx,
                        col_idx_in_table as u16,
                    ));

                    if ref_table_old_idx != unresolved.source_table_idx {
                        graph[ref_table_old_idx].push(unresolved.source_table_idx);
                        in_degree[unresolved.source_table_idx] += 1;
                    }
                }
            }
        }

        let mut sorted_old_indices = Vec::with_capacity(num_tables);
        let mut queue = VecDeque::new();

        for i in 0..num_tables {
            if in_degree[i] == 0 {
                queue.push_back(i);
            }
        }

        while let Some(u) = queue.pop_front() {
            sorted_old_indices.push(u);
            for &v in &graph[u] {
                in_degree[v] -= 1;
                if in_degree[v] == 0 {
                    queue.push_back(v);
                }
            }
        }

        if sorted_old_indices.len() < num_tables {
            let mut visited = vec![false; num_tables];
            for &idx in &sorted_old_indices {
                visited[idx] = true;
            }
            for i in 0..num_tables {
                if !visited[i] {
                    sorted_old_indices.push(i);
                }
            }
        }

        let mut old_to_new_table = vec![0u16; num_tables];
        for (new_idx, &old_idx) in sorted_old_indices.iter().enumerate() {
            old_to_new_table[old_idx] = new_idx as u16;
        }

        let mut new_tables = Vec::with_capacity(num_tables);
        for &old_idx in &sorted_old_indices {
            new_tables.push(self.tables[old_idx].clone());
        }
        self.tables = new_tables;

        let mut final_by_name: Vec<u16> = (0..num_tables as u16).collect();
        final_by_name.sort_unstable_by_key(|&i| self.tables[i as usize].name);

        let mut valid_fks = Vec::with_capacity(resolved_fks_info.len());
        let mut old_to_new_fk = vec![None; self.foreign_key_pool.len()];

        for (pool_index, _src, ref_table_old_idx, col_idx_in_table) in resolved_fks_info {
            let old_fk = &self.foreign_key_pool[pool_index];

            valid_fks.push(ForeignKey {
                constraint_name: old_fk.constraint_name,
                namespace: old_fk.namespace,
                table: old_to_new_table[ref_table_old_idx],
                column: col_idx_in_table,
                on_update: old_fk.on_update,
                on_delete: old_fk.on_delete,
            });

            old_to_new_fk[pool_index] = Some((valid_fks.len() - 1) as u32);
        }

        for col in &mut self.columns {
            let old_start = col.foreign_keys.start as usize;
            let old_end = old_start + col.foreign_keys.len as usize;

            let mut new_start = u32::MAX;
            let mut new_len = 0;

            for i in old_start..old_end {
                if let Some(new_idx) = old_to_new_fk[i] {
                    if new_start == u32::MAX {
                        new_start = new_idx;
                    }
                    new_len += 1;
                }
            }

            col.foreign_keys = PoolRange {
                start: if new_len == 0 { 0 } else { new_start },
                len: new_len,
            };
        }

        for table in &mut self.tables {
            let old_start = table.fk_start as usize;
            let old_end = old_start + table.fk_len as usize;

            let mut new_start = u32::MAX;
            let mut new_len = 0;

            for i in old_start..old_end {
                if let Some(new_idx) = old_to_new_fk[i] {
                    if new_start == u32::MAX {
                        new_start = new_idx;
                    }
                    new_len += 1;
                }
            }

            table.fk_start = if new_len == 0 { 0 } else { new_start };
            table.fk_len = new_len;
        }

        DatabaseSchema {
            interner: self.interner,
            tables: self.tables.into_boxed_slice(),
            by_name: final_by_name.into_boxed_slice(),
            columns: self.columns.into_boxed_slice(),
            column_names: self.column_names.into_boxed_slice(),
            constraint_pool: self.constraint_pool.into_boxed_slice(),
            foreign_key_pool: valid_fks.into_boxed_slice(),
            hash,
        }
    }
}
