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

CREATE SCHEMA IF NOT EXISTS boomerang;

CREATE
OR REPLACE FUNCTION boomerang.database_schema(
    p_namespace   text   DEFAULT 'public',
    p_skip_tables text[] DEFAULT '{}'
)
    RETURNS TABLE (
                      table_name    text,
                      attnum        smallint,
                      column_name   text,
                      data_type     text,
                      is_custom     boolean,
                      nullable      boolean,
                      is_primary    boolean,
                      is_unique     boolean,
                      default_expr  text,
                      fk_constraints text[],
                      fk_namespaces  text[],
                      fk_tables      text[],
                      fk_columns     text[],
                      fk_on_update   int2[],
                      fk_on_delete   int2[],
                      check_defs    text[],
                      enum_labels   text[],
                      domain_checks text[]
                  )
    LANGUAGE sql STABLE PARALLEL SAFE
AS $$
WITH tables AS MATERIALIZED (
    SELECT c.oid, c.relname
    FROM   pg_class c
               JOIN   pg_namespace n ON n.oid = c.relnamespace
    WHERE  c.relkind IN ('r', 'p')
      AND  n.nspname = p_namespace
      AND  c.relname <> ALL (COALESCE(p_skip_tables, '{}'))
),

     pk_uniq AS (
         SELECT i.indrelid                AS table_oid,
                i.indkey[s.i]             AS attnum,
                bool_or(i.indisprimary)   AS is_pk,
                bool_or(i.indisunique
                    AND i.indnkeyatts = 1
                    AND i.indpred IS NULL) AS is_uniq
         FROM   pg_index i
                    JOIN   generate_series(0, i.indnkeyatts - 1) AS s(i) ON TRUE
         WHERE  i.indrelid IN (SELECT oid FROM tables)
           AND  i.indexprs IS NULL
         GROUP  BY 1, 2
     ),

     fk AS (
         SELECT con.conrelid    AS table_oid,
                con.conkey[s.i] AS attnum,
                array_agg(con.conname::text ORDER BY con.oid, s.i) AS names,
                array_agg(rn.nspname::text  ORDER BY con.oid, s.i) AS namespaces,
                array_agg(rc.relname::text  ORDER BY con.oid, s.i) AS tables_,
                array_agg(ra.attname::text  ORDER BY con.oid, s.i) AS columns_,
                array_agg(CASE con.confupdtype
                              WHEN 'a' THEN 0 WHEN 'c' THEN 1 WHEN 'n' THEN 2
                              WHEN 'd' THEN 3 WHEN 'r' THEN 4 ELSE 0
                              END::int2 ORDER BY con.oid, s.i) AS on_update,
                array_agg(CASE con.confdeltype
                              WHEN 'a' THEN 0 WHEN 'c' THEN 1 WHEN 'n' THEN 2
                              WHEN 'd' THEN 3 WHEN 'r' THEN 4 ELSE 0
                              END::int2 ORDER BY con.oid, s.i) AS on_delete
         FROM   pg_constraint con
                    JOIN   generate_subscripts(con.conkey, 1) AS s(i) ON TRUE
                    JOIN   pg_class     rc ON rc.oid = con.confrelid
                    JOIN   pg_namespace rn ON rn.oid = rc.relnamespace
                    JOIN   pg_attribute ra ON ra.attrelid = con.confrelid
             AND ra.attnum   = con.confkey[s.i]
         WHERE  con.contype = 'f'
           AND  con.conrelid IN (SELECT oid FROM tables)
         GROUP  BY 1, 2
     ),

     col_checks AS (
         SELECT con.conrelid AS table_oid,
                con.conkey[1] AS attnum,
                array_agg(pg_get_constraintdef(con.oid) ORDER BY con.conname) AS defs
         FROM   pg_constraint con
         WHERE  con.contype = 'c'
           AND  cardinality(con.conkey) = 1
           AND  con.conrelid IN (SELECT oid FROM tables)
         GROUP  BY 1, 2
     ),

     enum_labels AS (
         SELECT e.enumtypid AS typ_oid,
                array_agg(e.enumlabel::text ORDER BY e.enumsortorder) AS labels
         FROM   pg_enum e
         GROUP  BY 1
     ),

     domain_checks AS (
         SELECT c.contypid AS typ_oid,
                array_agg(pg_get_constraintdef(c.oid) ORDER BY c.conname) AS defs
         FROM   pg_constraint c
         WHERE  c.contypid <> 0
         GROUP  BY 1
     )

SELECT t.relname::text, a.attnum,
       a.attname::text, pg_catalog.format_type(a.atttypid, a.atttypmod),
       tyn.nspname NOT IN ('pg_catalog', 'information_schema') AS is_custom,
       NOT a.attnotnull,
       COALESCE(pu.is_pk, FALSE),
       COALESCE(pu.is_uniq, FALSE),
       pg_get_expr(ad.adbin, ad.adrelid),
       f.names,
       f.namespaces,
       f.tables_,
       f.columns_,
       f.on_update,
       f.on_delete,
       cc.defs,
       el.labels,
       dc.defs
FROM tables t
         JOIN pg_attribute a ON a.attrelid = t.oid
    AND a.attnum > 0
    AND NOT a.attisdropped
         JOIN pg_type ty ON ty.oid = a.atttypid
         JOIN pg_namespace tyn ON tyn.oid = ty.typnamespace
         LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum
         LEFT JOIN pk_uniq pu ON pu.table_oid = t.oid AND pu.attnum = a.attnum
         LEFT JOIN fk f ON f.table_oid = t.oid AND f.attnum = a.attnum
         LEFT JOIN col_checks cc ON cc.table_oid = t.oid AND cc.attnum = a.attnum
         LEFT JOIN enum_labels el ON el.typ_oid = CASE
                                                      WHEN ty.typelem <> 0
                                                          THEN ty.typelem
                                                      ELSE ty.oid END
    AND ty.typelem = 0
         LEFT JOIN domain_checks dc ON dc.typ_oid = a.atttypid
ORDER BY t.relname, a.attnum;
$$;