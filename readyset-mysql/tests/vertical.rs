#![feature(trace_macros)]

//! This test suite implements the [Vertical Testing Design Doc][doc].
//!
//! [doc]: https://docs.google.com/document/d/1rTDzd4Z5jSUDqGmIu2C7R06f2HkNWxEll33-rF4WC-c
//!
//! Note that this test suite is ignored by default, and conditionally de-ignored with the
//! `vertical_tests` feature to prevent it running in normal builds (since it's slow and may find
//! new bugs); to run it locally run:
//!
//! ```notrust
//! cargo test -p noria-mysql --features vertical_tests --test vertical
//! ```
//!
//! This test suite will connect to a local mysql database, which can be set up with all the correct
//! configuration using the `docker-compose.yml` and `docker-compose.override.example.yml` in the
//! root of the repository. To run that mysql database, run:
//!
//! ```notrust
//! $ cp docker-compose.override.example.yml docker-compose.yml
//! $ docker-compose up -d mysql
//! ```
//!
//! Note that this test suite requires the *exact* configuration specified in that docker-compose
//! configuration, including the port, username, and password.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};
use std::ops::Range;
use std::{cmp, env, iter, mem};

use itertools::Itertools;
use mysql_async::prelude::Queryable;
use mysql_common::value::Value;
use paste::paste;
use proptest::prelude::*;
use proptest::sample::select;
use proptest::test_runner::TestCaseResult;
use readyset_client_test_helpers::mysql_helpers::MySQLAdapter;
use readyset_client_test_helpers::TestBuilder;
use readyset_data::DfValue;
use test_strategy::proptest;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Operation {
    Query {
        key: Vec<DfValue>,
    },
    Insert {
        table: String,
        row: Vec<DfValue>,
    },
    Update {
        table: String,
        old_row: Vec<DfValue>,
        new_row: Vec<DfValue>,
    },
    Delete {
        table: String,
        row: Vec<DfValue>,
    },
    /* TODO: coming soon
    Evict {
        /// *seed* for the node index to evict from.
        ///
        /// Note that we don't know how many nodes a query will have until after we install it in
        /// noria, so the actual node index will be this modulo the number of non-base-table nodes
        node_seed: usize,
        key: Vec<DfValue>,
    },
    */
}

#[derive(Debug, Clone)]
pub enum ColumnStrategy {
    Value(BoxedStrategy<DfValue>),
    ForeignKey {
        table: &'static str,
        foreign_column: usize,
    },
}

#[derive(Debug, Clone)]
pub struct RowStrategy(Vec<ColumnStrategy>);

impl RowStrategy {
    fn no_foreign_keys(self) -> Option<Vec<BoxedStrategy<DfValue>>> {
        self.0
            .into_iter()
            .map(|cs| match cs {
                ColumnStrategy::Value(strat) => Some(strat),
                ColumnStrategy::ForeignKey { .. } => None,
            })
            .collect()
    }

    fn has_foreign_keys(&self) -> bool {
        self.0
            .iter()
            .any(|cs| matches!(cs, ColumnStrategy::ForeignKey { .. }))
    }

    fn fill_foreign_keys<F, S>(self, foreign_key_strategy: F) -> Option<Vec<BoxedStrategy<DfValue>>>
    where
        F: Fn(&'static str, usize) -> Option<S>,
        S: Strategy<Value = DfValue> + Sized + 'static,
    {
        self.0
            .into_iter()
            .map(move |cs| match cs {
                ColumnStrategy::Value(strat) => Some(strat),
                ColumnStrategy::ForeignKey {
                    table,
                    foreign_column,
                } => foreign_key_strategy(table, foreign_column).map(|s| s.boxed()),
            })
            .collect()
    }
}

pub struct OperationParameters<'a> {
    already_generated: &'a [Operation],

    /// table name, index in table
    key_columns: Vec<(&'a str, usize)>,

    /// Added on to the end of every key
    extra_key_strategies: Vec<BoxedStrategy<DfValue>>,

    row_strategies: HashMap<&'static str, RowStrategy>,
}

impl<'a> OperationParameters<'a> {
    /// Return an iterator over all the query lookup keys that match rows previously inserted into
    /// the table
    fn existing_keys(&'a self) -> impl Iterator<Item = Vec<DfValue>> + 'a {
        let mut rows: HashMap<&'a str, Vec<&'a Vec<DfValue>>> = HashMap::new();
        for op in self.already_generated {
            match op {
                Operation::Insert { table, row } => {
                    rows.entry(table).or_default().push(row);
                }
                Operation::Update {
                    table,
                    old_row,
                    new_row,
                } => {
                    let rows = rows.entry(table).or_default();
                    rows.retain(|r| *r != old_row);
                    rows.push(new_row);
                }
                Operation::Delete { table, row } => {
                    rows.entry(table).or_default().retain(|r| *r != row);
                }
                Operation::Query { .. } => {}
            }
        }

        rows.into_iter()
            .map(|(tbl, rows)| rows.into_iter().map(|r| (tbl, r)).collect::<Vec<_>>())
            .multi_cartesian_product()
            .filter_map(move |vals| {
                self.key_columns
                    .iter()
                    .map(|(tbl, idx)| Some(vals.iter().find(|(t, _)| *tbl == *t)?.1[*idx].clone()))
                    .collect::<Option<Vec<_>>>()
            })
    }

    /// If there are any rows previously inserted into the table, return a proptest [`Strategy`] for
    /// generating one of those keys, otherwise return None
    fn existing_key_strategy(&'a self) -> Option<impl Strategy<Value = Vec<DfValue>> + 'static> {
        let keys = self.existing_keys().collect::<Vec<_>>();
        if keys.is_empty() {
            return None;
        }

        let extra_key_strategies = self.extra_key_strategies.clone();
        Some(select(keys).prop_flat_map(move |key| {
            key.into_iter()
                .map(|val| Just(val).boxed())
                .chain(extra_key_strategies.clone())
                .collect::<Vec<_>>()
        }))
    }

    fn existing_rows(&'a self) -> impl Iterator<Item = (String, Vec<DfValue>)> + 'a {
        self.already_generated.iter().filter_map(|op| match op {
            Operation::Insert { table, row } => Some((table.clone(), row.clone())),
            _ => None,
        })
    }

    /// Return a proptest [`Strategy`] for generating new keys for the query
    fn key_strategy(&'a self) -> impl Strategy<Value = Vec<DfValue>> + 'static {
        self.key_columns
            .clone()
            .into_iter()
            .map(move |(t, idx)| {
                self.row_strategies[t]
                    .clone()
                    .no_foreign_keys()
                    .expect("foreign key can't be a key_column")
                    .prop_map(move |mut r| r.remove(idx))
                    .boxed()
            })
            .chain(self.extra_key_strategies.clone())
            .collect::<Vec<_>>()
    }
}

impl Operation {
    /// Return a proptest [`Strategy`] for generating the *first* [`Operation`] in the sequence (eg
    /// not dependent on previous operations)
    fn first_arbitrary(
        key_columns: Vec<(&str, usize)>,
        extra_key_strategies: Vec<BoxedStrategy<DfValue>>,
        row_strategies: HashMap<&'static str, RowStrategy>,
    ) -> impl Strategy<Value = Self> {
        Self::arbitrary(OperationParameters {
            already_generated: &[],
            key_columns,
            extra_key_strategies,
            row_strategies,
        })
    }

    /// Return a proptest [`Strategy`] for generating all but the first [`Operation`], based on the
    /// previously generated operations
    fn arbitrary(params: OperationParameters) -> impl Strategy<Value = Self> + 'static {
        use Operation::*;

        let no_fk_strategies = params
            .row_strategies
            .iter()
            .filter_map(|(k, v)| Some((*k, v.clone().no_foreign_keys()?)))
            .collect::<Vec<_>>();
        let non_key_ops = prop_oneof![
            params.key_strategy().prop_map(|key| Query { key }),
            select(no_fk_strategies).prop_flat_map(|(table, row_strat)| row_strat.prop_map(
                move |row| Insert {
                    table: table.to_string(),
                    row,
                }
            ))
        ];

        if let Some(existing_key_strategy) = params.existing_key_strategy() {
            let key_ops = prop_oneof![
                non_key_ops,
                existing_key_strategy.prop_map(|key| Query { key })
            ];

            let rows = params.existing_rows().collect::<Vec<_>>();
            if rows.is_empty() {
                key_ops.boxed()
            } else {
                let fk_rows = rows.clone();
                let fill_foreign_keys = move |row_strategy: RowStrategy| {
                    row_strategy.fill_foreign_keys(|table, col| {
                        let vals = fk_rows
                            .iter()
                            .filter(|(t, _)| table == t)
                            .map(|(_, r)| r[col].clone())
                            .collect::<Vec<_>>();
                        if vals.is_empty() {
                            None
                        } else {
                            Some(select(vals))
                        }
                    })
                };

                let mk_fk_insert = {
                    let fk_tables = params
                        .row_strategies
                        .iter()
                        .filter(|(_, v)| v.has_foreign_keys())
                        .map(|(t, v)| (*t, v.clone()))
                        .collect::<Vec<_>>();
                    if fk_tables.is_empty() {
                        None
                    } else {
                        let fill_foreign_keys = fill_foreign_keys.clone();
                        Some(
                            select(fk_tables)
                                .prop_filter_map(
                                    "No previously-generated rows",
                                    move |(table, row_strat)| {
                                        Some(fill_foreign_keys(row_strat)?.prop_map(move |row| {
                                            Insert {
                                                table: table.to_owned(),
                                                row,
                                            }
                                        }))
                                    },
                                )
                                .prop_flat_map(|s| s),
                        )
                    }
                };

                let row_strategies = params.row_strategies.clone();
                let mk_update = select(rows.clone()).prop_flat_map(move |(table, old_row)| {
                    (fill_foreign_keys(row_strategies[table.as_str()].clone())
                        .unwrap()
                        .into_iter()
                        .zip(old_row.clone())
                        .map(|(new_val, old_val)| prop_oneof![Just(old_val), new_val])
                        .collect::<Vec<_>>())
                    .prop_filter_map("No-op update", move |new_row| {
                        (old_row != new_row).then(|| Update {
                            table: table.clone(),
                            old_row: old_row.clone(),
                            new_row,
                        })
                    })
                });
                let mk_delete = select(rows).prop_map(move |(table, row)| Delete { table, row });
                if let Some(mk_fk_insert) = mk_fk_insert {
                    prop_oneof![key_ops, mk_fk_insert, mk_update, mk_delete].boxed()
                } else {
                    prop_oneof![key_ops, mk_update, mk_delete].boxed()
                }
            }
        } else {
            non_key_ops.boxed()
        }
    }
}

#[derive(Debug)]
struct Table {
    name: &'static str,
    create_statement: &'static str,
    primary_key: usize,
    columns: Vec<&'static str>,
}

impl Table {
    fn primary_key_column(&self) -> &'static str {
        self.columns[self.primary_key]
    }
}

/// The result of running a single [`Operation`] against either mysql or noria.
#[derive(Debug)]
pub enum OperationResult {
    Err(mysql_async::Error),
    Rows(Vec<mysql_async::Row>),
    NoResults,
}

impl OperationResult {
    /// Returns `true` if the operation result is [`Err`].
    ///
    /// [`Err`]: OperationResult::Err
    pub fn is_err(&self) -> bool {
        matches!(self, Self::Err(..))
    }

    pub fn err(&self) -> Option<&mysql_async::Error> {
        match self {
            Self::Err(v) => Some(v),
            _ => None,
        }
    }
}

fn compare_rows(r1: &mysql_async::Row, r2: &mysql::Row) -> Ordering {
    let l = cmp::min(r1.len(), r2.len());
    for i in 0..l {
        match r1.as_ref(i).unwrap().partial_cmp(r2.as_ref(i).unwrap()) {
            Some(Ordering::Equal) => {}
            Some(non_equal) => return non_equal,
            None => {}
        }
    }
    r1.len().cmp(&r2.len())
}

impl PartialEq for OperationResult {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            // TODO: is it worth trying to check error equality here? probably not
            (Self::Err(_), Self::Err(_)) => true,
            (Self::Rows(rs1), Self::Rows(rs2)) => {
                rs1.len() == rs2.len() && {
                    let mut rs1 = rs1.iter().collect::<Vec<_>>();
                    rs1.sort_by(|r1, r2| compare_rows(r1, r2));
                    let mut rs2 = rs2.iter().collect::<Vec<_>>();
                    rs2.sort_by(|r1, r2| compare_rows(r1, r2));
                    rs1.iter().zip(rs2).all(|(r1, r2)| {
                        r1.len() == r2.len()
                            && (0..r1.len()).all(|i| r1.as_ref(i).unwrap() == r2.as_ref(i).unwrap())
                    })
                }
            }
            (Self::NoResults, Self::NoResults) => true,
            _ => false,
        }
    }
}

impl From<mysql_async::Result<Vec<mysql::Row>>> for OperationResult {
    fn from(res: mysql_async::Result<Vec<mysql::Row>>) -> Self {
        match res {
            Ok(rows) => Self::Rows(rows),
            Err(e) => Self::Err(e),
        }
    }
}

impl From<mysql_async::Result<()>> for OperationResult {
    fn from(res: mysql_async::Result<()>) -> Self {
        match res {
            Ok(()) => Self::NoResults,
            Err(e) => Self::Err(e),
        }
    }
}

impl Operation {
    async fn run(
        &self,
        conn: &mut mysql_async::Conn,
        query: &str,
        tables: &HashMap<&str, Table>,
    ) -> Result<OperationResult, TestCaseError> {
        fn to_values(dts: &[DfValue]) -> Result<Vec<Value>, TestCaseError> {
            dts.iter()
                .map(|dt| {
                    Value::try_from(dt.clone()).map_err(|e| {
                        TestCaseError::reject(format!(
                            "DfValue conversion to mysql Value failed: {}",
                            e
                        ))
                    })
                })
                .collect()
        }

        match self {
            Operation::Query { key } => Ok(conn
                .exec::<mysql_async::Row, _, _>(query, mysql::Params::Positional(to_values(key)?))
                .await
                .into()),
            Operation::Insert { table, row } => Ok(conn
                .exec_drop(
                    format!(
                        "INSERT INTO {} VALUES ({})",
                        table,
                        (0..row.len()).map(|_| "?").join(",")
                    ),
                    to_values(row)?,
                )
                .await
                .into()),
            Operation::Update {
                table: table_name,
                old_row,
                new_row,
            } => {
                let table = &tables[table_name.as_str()];
                let updates = table
                    .columns
                    .iter()
                    .zip(old_row)
                    .zip(new_row)
                    .filter_map(|((col_name, old_val), new_val)| {
                        (old_val != new_val).then_some((col_name, new_val))
                    })
                    .collect::<Vec<_>>();
                let set_clause = updates
                    .iter()
                    .map(|(col_name, _)| format!("{} = ?", col_name))
                    .join(",");
                let mut params = updates
                    .into_iter()
                    .map(|(_, val)| Value::try_from(val.clone()).unwrap())
                    .collect::<Vec<_>>();
                params.push(old_row[table.primary_key].clone().try_into().unwrap());
                Ok(conn
                    .exec_drop(
                        format!(
                            "UPDATE {} SET {} WHERE {} = ?",
                            table.name,
                            set_clause,
                            table.primary_key_column()
                        ),
                        params,
                    )
                    .await
                    .into())
            }
            Operation::Delete {
                table: table_name,
                row,
            } => {
                let table = &tables[table_name.as_str()];
                Ok(conn
                    .exec_drop(
                        format!(
                            "DELETE FROM {} WHERE {} = ?",
                            table.name,
                            table.primary_key_column()
                        ),
                        (Value::try_from(row[table.primary_key].clone()).unwrap(),),
                    )
                    .await
                    .into())
            }
        }
    }
}

#[derive(Default)]
struct TestOptions {
    /// Added on to the end of every key
    extra_key_strategies: Vec<BoxedStrategy<DfValue>>,
}

struct OperationsParams {
    size_range: Range<usize>,
    key_columns: Vec<(&'static str, usize)>,
    row_strategies: HashMap<&'static str, RowStrategy>,
    options: TestOptions,
}

#[derive(Default, Debug, Clone)]
struct Operations(Vec<Operation>);

impl Operations {
    fn arbitrary(mut params: OperationsParams) -> impl Strategy<Value = Self> {
        let key_columns = params.key_columns;
        let row_strategies = mem::take(&mut params.row_strategies);
        let extra_key_strategies = mem::take(&mut params.options.extra_key_strategies);
        params.size_range.prop_flat_map(move |len| {
            if len == 0 {
                return Just(Default::default()).boxed();
            }

            let mut res = Operation::first_arbitrary(
                key_columns.clone(),
                extra_key_strategies.clone(),
                row_strategies.clone(),
            )
            .prop_map(|op| vec![op])
            .boxed();
            for _ in 0..len {
                let row_strategies = row_strategies.clone();
                let extra_key_strategies = extra_key_strategies.clone();
                let key_columns = key_columns.clone();
                res = res
                    .prop_flat_map(move |ops| {
                        let op_params = OperationParameters {
                            already_generated: &ops,
                            key_columns: key_columns.clone(),
                            extra_key_strategies: extra_key_strategies.clone(),
                            row_strategies: row_strategies.clone(),
                        };

                        Operation::arbitrary(op_params).prop_map(move |op| {
                            ops.clone().into_iter().chain(iter::once(op)).collect()
                        })
                    })
                    .boxed();
            }
            res.prop_map(Operations).boxed()
        })
    }
}

impl Operations {
    async fn run(self, query: &str, tables: &HashMap<&str, Table>) -> TestCaseResult {
        readyset_client_test_helpers::mysql_helpers::recreate_database("vertical").await;
        let mut mysql = mysql_async::Conn::new(
            mysql_async::OptsBuilder::default()
                .user(Some("root"))
                .pass(Some("noria"))
                .ip_or_hostname(env::var("MYSQL_HOST").unwrap_or_else(|_| "127.0.0.1".into()))
                .tcp_port(
                    env::var("MYSQL_TCP_PORT")
                        .unwrap_or_else(|_| "3306".into())
                        .parse()
                        .unwrap(),
                )
                .db_name(Some("vertical")),
        )
        .await
        .unwrap();
        let (opts, _handle) = TestBuilder::default().build::<MySQLAdapter>().await;
        let mut noria = mysql_async::Conn::new(opts).await.unwrap();

        for table in tables.values() {
            mysql.query_drop(table.create_statement).await.unwrap();
            noria.query_drop(table.create_statement).await.unwrap();
        }

        for op in self.0 {
            let mysql_res = op.run(&mut mysql, query, tables).await?;
            // skip tests where mysql returns an error for the operations
            prop_assume!(
                !mysql_res.is_err(),
                "MySQL returned an error: {}",
                mysql_res.err().unwrap()
            );

            let noria_res = op.run(&mut noria, query, tables).await?;
            assert_eq!(mysql_res, noria_res);
        }

        Ok(())
    }
}

macro_rules! vertical_tests {
    ($(#[$meta:meta])* $name:ident($($params: tt)*); $($rest: tt)*) => {
        vertical_tests!(@test $(#[$meta])* $name($($params)*));
        vertical_tests!($($rest)*);
    };

    // define the test itself
    (@test $(#[$meta:meta])* $name:ident($query: expr; $options: expr; $($tables: tt)*)) => {
        paste! {
        fn [<$name _generate_ops>]() -> impl Strategy<Value = Operations> {
            let size_range = 1..100; // TODO make configurable
            let row_strategies = vertical_tests!(@row_strategies $($tables)*);
            let key_columns = vertical_tests!(@key_columns $($tables)*);

            let params = OperationsParams {
                size_range,
                key_columns,
                row_strategies,
                options: $options,
            };

            Operations::arbitrary(params)
        }

        #[proptest]
        #[serial_test::serial]
        #[cfg_attr(not(feature = "vertical_tests"), ignore)]
        $(#[$meta])*
        fn $name(
            #[strategy([<$name _generate_ops>]())]
            operations: Operations
        ) {
            let tables = vertical_tests!(@tables $($tables)*);
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(operations.run($query, &tables))?;
        }
        }
    };
    (@test $(#[$meta:meta])* $name:ident($query: expr; $($tables: tt)*)) => {
        vertical_tests!(@test $(#[$meta])* $name($query;Default::default();$($tables)*));
    };

    // collect together all of the key columns from the tables into a single array
    (@key_columns $($table_name: expr => (
        $create_table: expr,
        schema: [$($schema:tt)*],
        primary_key: $pk_index: expr,
        key_columns: [$($kc: expr),* $(,)?]
        $(,)?
    )),* $(,)?) => {
        vec![$($(($table_name, $kc),)*)*]
    };

    // Build up the hashmap of Tables
    (@tables $($table_name: expr => (
        $create_table: expr,
        schema: [$($col_name: ident : $col_strat: expr),* $(,)?],
        primary_key: $pk_index: expr,
        key_columns: [$($kc: expr),* $(,)?]
        $(,)?
    )),* $(,)?) => {
        HashMap::from([
            $(($table_name, Table {
                name: $table_name,
                create_statement: $create_table,
                primary_key: $pk_index,
                columns: vec![$(stringify!($col_name),)*],
            }),)*
        ])
    };

    // Build up the hashmap of row_strategies
    (@row_strategies $($table_name: expr => (
        $create_table: expr,
        schema: [$($schema:tt)*],
        primary_key: $pk_index: expr,
        key_columns: [$($kc: expr),* $(,)?]
        $(,)?
    )),* $(,)?) => {
        HashMap::from([
            $(($table_name, vertical_tests!(@row_strategy $($schema)*)),)*
        ])
    };

    (@row_strategy $($col_name: ident : $col_type: tt $(($($type_args: tt)*))?),* $(,)?) => {{
        RowStrategy(vec![
            $(vertical_tests!(@column_strategy $col_type $(($($type_args)*))*), )*
        ])
    }};

    (@column_strategy foreign_key($table_name: expr, $foreign_column: expr)) => {
        ColumnStrategy::ForeignKey {
            table: $table_name,
            foreign_column: $foreign_column,
        }
    };

    (@column_strategy nullable($schema_type: ty)) => {
        ColumnStrategy::Value(any::<Option<$schema_type>>().prop_map(|ov| match ov {
            Some(v) => DfValue::from(v),
            None => DfValue::None,
        }).boxed())
    };

    (@column_strategy value($value: expr)) => {
        ColumnStrategy::Value(Just(DfValue::try_from($value).unwrap()).boxed())
    };

    (@column_strategy $schema_type: ty) => {
        ColumnStrategy::Value(any::<$schema_type>().prop_map_into::<DfValue>().boxed())
    };

    () => {};
}

vertical_tests! {
    simple_point_lookup(
        "SELECT id, name FROM users WHERE id = ?";
        "users" => (
            "CREATE TABLE users (id INT, name TEXT, PRIMARY KEY (id))",
            schema: [id: i32, name: String],
            primary_key: 0,
            key_columns: [0],
        )
    );

    aggregate_with_filter(
        "SELECT count(*) FROM users_groups WHERE group_id = ? AND joined_at IS NOT NULL";
        "users_groups" => (
            "CREATE TABLE users_groups (
                id int NOT NULL,
                user_id int NOT NULL,
                group_id int NOT NULL,
                joined_at int DEFAULT NULL,
                PRIMARY KEY (id)
            )",
            schema: [id: i32, user_id: i32, group_id: i32, joined_at: nullable(i32)],
            primary_key: 0,
            key_columns: [2],
        )
    );

    partial_inner_join(
        "SELECT posts.id, posts.title, users.name
         FROM posts
         JOIN users ON posts.author_id = users.id
         WHERE users.id = ?";
        "posts" => (
            "CREATE TABLE posts (id INT, title TEXT, author_id INT, PRIMARY KEY (id))",
            schema: [id: i32, title: String, author_id: foreign_key("users", 0)],
            primary_key: 0,
            key_columns: [],
        ),
        "users" => (
            "CREATE TABLE users (id INT, name TEXT, PRIMARY KEY (id))",
            schema: [id: i32, name: String],
            primary_key: 0,
            key_columns: [0],
        )
    );

    partial_union_inner_join(
        "SELECT posts.id, posts.title, users.name
         FROM posts
         JOIN users ON posts.author_id = users.id
         WHERE (users.name = \"a\" OR posts.title = \"a\")
           AND users.id = ?";
        "posts" => (
            "CREATE TABLE posts (id INT, title TEXT, author_id INT, PRIMARY KEY (id))",
            schema: [id: i32, title: value("a"), author_id: i32],
            primary_key: 0,
            key_columns: [],
        ),
        "users" => (
            "CREATE TABLE users (id INT, name TEXT, PRIMARY KEY (id))",
            schema: [id: i32, name: value("a")],
            primary_key: 0,
            key_columns: [0],
        )
    );

    partial_topk(
        "SELECT id, title FROM posts WHERE author_id = ? ORDER BY score DESC LIMIT 3";
        "posts" => (
            "CREATE TABLE posts (id INT, title TEXT, score INT, author_id INT, PRIMARY KEY (id))",
            schema: [id: i32, title: String, score: i32, author_id: foreign_key("users", 0)],
            primary_key: 0,
            key_columns: []
        ),
        "users" => (
            "CREATE TABLE users (id INT, name TEXT, PRIMARY KEY (id))",
            schema: [id: i32, name: String],
            primary_key: 0,
            key_columns: [0],
        )
    );

    simple_range(
        "SELECT id, name, score FROM posts WHERE score > ?";
        "posts" => (
            "CREATE TABLE posts (id INT, name TEXT, score INT, PRIMARY KEY (id))",
            schema: [id: i32, name: String, score: i32],
            primary_key: 0,
            key_columns: [2],
        )
    );

    paginate_grouped(
        "SELECT id FROM posts WHERE author_id = ? ORDER BY score DESC LIMIT 3 OFFSET ?";
        TestOptions {
            extra_key_strategies: vec![
                (0u32..=15u32).prop_map(|i| DfValue::from(i * 3)).boxed()
            ]
        };
        "posts" => (
            "CREATE TABLE posts (id INT, author_id INT, score INT, PRIMARY KEY (id))",
            schema: [id: i32, author_id: i32, score: i32],
            primary_key: 0,
            key_columns: [2],
        )
    );
}
