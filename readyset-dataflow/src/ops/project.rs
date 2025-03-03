use std::borrow::Cow;
use std::collections::HashMap;
use std::convert::TryInto;

use dataflow_expression::Expr;
use dataflow_state::PointKey;
use readyset_errors::ReadySetResult;
use readyset_tracing::error;
use serde::{Deserialize, Serialize};

use crate::prelude::*;
use crate::processing::{ColumnSource, IngredientLookupResult, LookupIndex, LookupMode};

/// Permutes or omits columns from its source node, or adds additional columns whose values are
/// given by expressions
///
/// Columns emitted by project are always in the following order:
///
/// 1. columns ([`emit`](Project::emit))
/// 2. [`expressions`](Project::expressions)
/// 3. literals ([`additional`](Project::additional))
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    us: Option<IndexPair>,
    emit: Option<Vec<usize>>,
    additional: Option<Vec<DfValue>>,
    expressions: Option<Vec<Expr>>,
    src: IndexPair,
    cols: usize,
}

impl Project {
    /// Construct a new project operator.
    pub fn new(
        src: NodeIndex,
        emit: &[usize],
        additional: Option<Vec<DfValue>>,
        expressions: Option<Vec<Expr>>,
    ) -> Project {
        Project {
            emit: Some(emit.into()),
            additional,
            expressions,
            src: src.into(),
            cols: 0,
            us: None,
        }
    }
}

impl Ingredient for Project {
    fn take(&mut self) -> NodeOperator {
        Clone::clone(self).into()
    }

    fn ancestors(&self) -> Vec<NodeIndex> {
        vec![self.src.as_global()]
    }

    fn can_query_through(&self) -> bool {
        self.expressions.is_none() && self.additional.is_none()
    }

    #[allow(clippy::type_complexity)]
    fn query_through<'a>(
        &self,
        columns: &[usize],
        key: &PointKey,
        nodes: &DomainNodes,
        states: &'a StateMap,
        mode: LookupMode,
    ) -> ReadySetResult<IngredientLookupResult<'a>> {
        let emit = self.emit.clone();
        let additional = self.additional.clone();
        let expressions = self.expressions.clone();

        let in_cols = if let Some(ref emit) = self.emit {
            let c = columns
                .iter()
                .map(|&outi| {
                    if outi >= emit.len() {
                        Err(ReadySetError::Internal(format!(
                            "query_through should never be queried for
                                                generated columns; columns: {:?}; self.emit: {:?}",
                            columns, emit
                        )))
                    } else {
                        Ok(emit[outi])
                    }
                })
                .collect();
            match c {
                Ok(c) => Cow::Owned(c),
                Err(e) => return Ok(IngredientLookupResult::err(e)),
            }
        } else {
            Cow::Borrowed(columns)
        };

        let res = self.lookup(*self.src, &in_cols, key, nodes, states, mode)?;
        Ok(match emit {
            Some(emit) => res.map(move |r| {
                let r = r?;
                let mut new_r = Vec::with_capacity(r.len());
                let mut expr: Vec<DfValue> = if let Some(ref e) = expressions {
                    e.iter()
                        .map(|expr| expr.eval(&r))
                        .collect::<ReadySetResult<Vec<DfValue>>>()?
                } else {
                    vec![]
                };

                new_r.extend(
                    r.iter()
                        .cloned()
                        .enumerate()
                        .filter(|(i, _)| emit.iter().any(|e| e == i))
                        .map(|(_, c)| c),
                );

                new_r.append(&mut expr);
                if let Some(ref a) = additional {
                    new_r.append(&mut a.clone());
                }

                Ok(Cow::from(new_r))
            }),
            None => res,
        })
    }

    fn on_connected(&mut self, g: &Graph) {
        self.cols = g[self.src.as_global()].columns().len();
    }

    impl_replace_sibling!(src);

    fn on_commit(&mut self, us: NodeIndex, remap: &HashMap<NodeIndex, IndexPair>) {
        self.src.remap(remap);
        self.us = Some(remap[&us]);

        // Eliminate emit specifications which require no permutation of
        // the inputs, so we don't needlessly perform extra work on each
        // update.
        self.emit = self.emit.take().and_then(|emit| {
            let complete =
                emit.len() == self.cols && self.additional.is_none() && self.expressions.is_none();
            let sequential = emit.iter().enumerate().all(|(i, &j)| i == j);
            if complete && sequential {
                None
            } else {
                Some(emit)
            }
        });
    }

    fn on_input(
        &mut self,
        from: LocalNodeIndex,
        mut rs: Records,
        _: &ReplayContext,
        _: &DomainNodes,
        _: &StateMap,
    ) -> ReadySetResult<ProcessingResult> {
        debug_assert_eq!(from, *self.src);
        if let Some(ref emit) = self.emit {
            for r in &mut *rs {
                let mut new_r = Vec::with_capacity(r.len());

                for &i in emit {
                    new_r.push(r[i].clone());
                }

                if let Some(ref e) = self.expressions {
                    new_r.extend(e.iter().map(|expr| match expr.eval(r) {
                        Ok(val) => val,
                        Err(e) => {
                            error!(error = %e, "Error evaluating project expression");
                            DfValue::None
                        }
                    }));
                }

                if let Some(ref a) = self.additional {
                    new_r.append(&mut a.clone());
                }

                **r = new_r;
            }
        }

        Ok(ProcessingResult {
            results: rs,
            ..Default::default()
        })
    }

    fn suggest_indexes(&self, _: NodeIndex) -> HashMap<NodeIndex, LookupIndex> {
        HashMap::new()
    }

    fn column_source(&self, cols: &[usize]) -> ColumnSource {
        let mapped_cols = cols
            .iter()
            .filter_map(|&x| {
                if self
                    .emit
                    .as_ref()
                    .map(|emit| x >= emit.len())
                    .unwrap_or(false)
                {
                    None
                } else {
                    Some(self.emit.as_ref().map_or(x, |emit| emit[x]))
                }
            })
            .collect::<Vec<_>>();
        if mapped_cols.len() != cols.len() {
            ColumnSource::RequiresFullReplay(vec1![self.src.as_global()])
        } else {
            ColumnSource::exact_copy(self.src.as_global(), mapped_cols.try_into().unwrap())
        }
    }

    fn description(&self, detailed: bool) -> String {
        if !detailed {
            return String::from("π");
        }

        let mut emit_cols = vec![];
        match self.emit.as_ref() {
            None => emit_cols.push("*".to_string()),
            Some(emit) => {
                emit_cols.extend(emit.iter().map(ToString::to_string));

                if let Some(ref arithmetic) = self.expressions {
                    emit_cols.extend(arithmetic.iter().map(ToString::to_string));
                }

                if let Some(ref add) = self.additional {
                    emit_cols.extend(add.iter().map(|e| format!("lit: {}", e)));
                }
            }
        };
        format!("π[{}]", emit_cols.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use dataflow_expression::utils::{make_int_column, make_literal};
    use dataflow_expression::BinaryOperator;
    use dataflow_state::MaterializedNodeState;
    use readyset_data::DfType;
    use Expr::Op;

    use super::*;
    use crate::ops;

    fn setup(materialized: bool, all: bool, add: bool) -> ops::test::MockGraph {
        let mut g = ops::test::MockGraph::new();
        let s = g.add_base("source", &["x", "y", "z"]);

        let permutation = if all { vec![0, 1, 2] } else { vec![2, 0] };
        let additional = if add {
            Some(vec![DfValue::from("hello"), DfValue::Int(42)])
        } else {
            None
        };
        g.set_op(
            "permute",
            &["x", "y", "z"],
            Project::new(s.as_global(), &permutation[..], additional, None),
            materialized,
        );
        g
    }

    fn setup_arithmetic(expression: Expr) -> ops::test::MockGraph {
        let mut g = ops::test::MockGraph::new();
        let s = g.add_base("source", &["x", "y", "z"]);

        let permutation = vec![0, 1];
        g.set_op(
            "permute",
            &["x", "y", "z"],
            Project::new(
                s.as_global(),
                &permutation[..],
                None,
                Some(vec![expression]),
            ),
            false,
        );
        g
    }

    fn setup_column_arithmetic(op: BinaryOperator) -> ops::test::MockGraph {
        let expression = Expr::Op {
            left: Box::new(make_int_column(0)),
            right: Box::new(make_int_column(1)),
            op,
            ty: DfType::Int,
        };

        setup_arithmetic(expression)
    }

    #[test]
    fn it_describes() {
        let p = setup(false, false, true);
        assert_eq!(p.node().description(true), "π[2, 0, lit: hello, lit: 42]");
    }

    #[test]
    fn it_describes_arithmetic() {
        let p = setup_column_arithmetic(BinaryOperator::Add);
        assert_eq!(p.node().description(true), "π[0, 1, (0 + 1)]");
    }

    #[test]
    fn it_describes_all() {
        let p = setup(false, true, false);
        assert_eq!(p.node().description(true), "π[*]");
    }

    #[test]
    fn it_describes_all_w_literals() {
        let p = setup(false, true, true);
        assert_eq!(
            p.node().description(true),
            "π[0, 1, 2, lit: hello, lit: 42]"
        );
    }

    #[test]
    fn it_forwards_some() {
        let mut p = setup(false, false, true);

        let rec = vec![
            "a".try_into().unwrap(),
            "b".try_into().unwrap(),
            "c".try_into().unwrap(),
        ];
        assert_eq!(
            p.narrow_one_row(rec, false),
            vec![vec![
                "c".try_into().unwrap(),
                "a".try_into().unwrap(),
                "hello".try_into().unwrap(),
                42.into()
            ]]
            .into()
        );
    }

    #[test]
    fn it_forwards_all() {
        let mut p = setup(false, true, false);

        let rec = vec![
            "a".try_into().unwrap(),
            "b".try_into().unwrap(),
            "c".try_into().unwrap(),
        ];
        assert_eq!(
            p.narrow_one_row(rec, false),
            vec![vec![
                "a".try_into().unwrap(),
                "b".try_into().unwrap(),
                "c".try_into().unwrap()
            ]]
            .into()
        );
    }

    #[test]
    fn it_forwards_all_w_literals() {
        let mut p = setup(false, true, true);

        let rec = vec![
            "a".try_into().unwrap(),
            "b".try_into().unwrap(),
            "c".try_into().unwrap(),
        ];
        assert_eq!(
            p.narrow_one_row(rec, false),
            vec![vec![
                "a".try_into().unwrap(),
                "b".try_into().unwrap(),
                "c".try_into().unwrap(),
                "hello".try_into().unwrap(),
                42.into(),
            ]]
            .into()
        );
    }

    #[test]
    fn it_forwards_addition_arithmetic() {
        let mut p = setup_column_arithmetic(BinaryOperator::Add);
        let rec = vec![10.into(), 20.into()];
        assert_eq!(
            p.narrow_one_row(rec, false),
            vec![vec![10.into(), 20.into(), 30.into()]].into()
        );
    }

    #[test]
    fn it_forwards_subtraction_arithmetic() {
        let mut p = setup_column_arithmetic(BinaryOperator::Subtract);
        let rec = vec![10.into(), 20.into()];
        assert_eq!(
            p.narrow_one_row(rec, false),
            vec![vec![10.into(), 20.into(), (-10).into()]].into()
        );
    }

    #[test]
    fn it_forwards_multiplication_arithmetic() {
        let mut p = setup_column_arithmetic(BinaryOperator::Multiply);
        let rec = vec![10.into(), 20.into()];
        assert_eq!(
            p.narrow_one_row(rec, false),
            vec![vec![10.into(), 20.into(), 200.into()]].into()
        );
    }

    #[test]
    fn it_forwards_division_arithmetic() {
        let mut p = setup_column_arithmetic(BinaryOperator::Divide);
        let rec = vec![10.into(), 2.into()];
        assert_eq!(
            p.narrow_one_row(rec, false),
            vec![vec![10.into(), 2.into(), 5.into()]].into()
        );
    }

    #[test]
    fn it_forwards_arithmetic_w_literals() {
        let number: DfValue = 40.into();
        let expression = Expr::Op {
            left: Box::new(make_int_column(0)),
            right: Box::new(make_literal(number)),
            op: BinaryOperator::Multiply,
            ty: DfType::Int,
        };

        let mut p = setup_arithmetic(expression);
        let rec = vec![10.into(), 0.into()];
        assert_eq!(
            p.narrow_one_row(rec, false),
            vec![vec![10.into(), 0.into(), 400.into()]].into()
        );
    }

    #[test]
    fn it_forwards_arithmetic_w_only_literals() {
        let a: DfValue = 80.into();
        let b: DfValue = 40.into();
        let expression = Expr::Op {
            left: Box::new(make_literal(a)),
            right: Box::new(make_literal(b)),
            op: BinaryOperator::Divide,
            ty: DfType::Int,
        };

        let mut p = setup_arithmetic(expression);
        let rec = vec![0.into(), 0.into()];
        assert_eq!(
            p.narrow_one_row(rec, false),
            vec![vec![0.into(), 0.into(), 2.into()]].into()
        );
    }

    fn setup_query_through(
        mut state: MaterializedNodeState,
        permutation: &[usize],
        additional: Option<Vec<DfValue>>,
        expressions: Option<Vec<Expr>>,
    ) -> (Project, StateMap) {
        let global = NodeIndex::new(0);
        let mut index: IndexPair = global.into();
        let local = LocalNodeIndex::make(0);
        index.set_local(local);

        let mut states = StateMap::default();
        let row: Record = vec![1.into(), 2.into(), 3.into()].into();
        state.add_key(Index::hash_map(vec![0]), None);
        state.add_key(Index::hash_map(vec![1]), None);
        state.process_records(&mut row.into(), None, None).unwrap();
        states.insert(local, state);

        let mut project = Project::new(global, permutation, additional, expressions);
        let mut remap = HashMap::new();
        remap.insert(global, index);
        project.on_commit(global, &remap);
        (project, states)
    }

    fn assert_query_through(
        project: Project,
        by_column: usize,
        key: DfValue,
        states: StateMap,
        expected: Vec<DfValue>,
    ) {
        let mut iter = project
            .query_through(
                &[by_column],
                &PointKey::Single(key),
                &DomainNodes::default(),
                &states,
                LookupMode::Strict,
            )
            .unwrap()
            .unwrap();
        assert_eq!(expected, iter.next().unwrap().unwrap().into_owned());
    }

    #[test]
    fn it_queries_through_all() {
        let state = MaterializedNodeState::Memory(MemoryState::default());
        let (p, states) = setup_query_through(state, &[0, 1, 2], None, None);
        let expected: Vec<DfValue> = vec![1.into(), 2.into(), 3.into()];
        assert_query_through(p, 0, 1.into(), states, expected);
    }

    #[test]
    fn it_queries_through_all_persistent() {
        let state = MaterializedNodeState::Persistent(PersistentState::new(
            String::from("it_queries_through_all_persistent"),
            Vec::<Box<[usize]>>::new(),
            &PersistenceParameters::default(),
        ));

        let (p, states) = setup_query_through(state, &[0, 1, 2], None, None);
        let expected: Vec<DfValue> = vec![1.into(), 2.into(), 3.into()];
        assert_query_through(p, 0, 1.into(), states, expected);
    }

    #[test]
    fn it_queries_through_some() {
        let state = MaterializedNodeState::Memory(MemoryState::default());
        let (p, states) = setup_query_through(state, &[1], None, None);
        let expected: Vec<DfValue> = vec![2.into()];
        assert_query_through(p, 0, 2.into(), states, expected);
    }

    #[test]
    fn it_queries_through_some_persistent() {
        let state = MaterializedNodeState::Persistent(PersistentState::new(
            String::from("it_queries_through_some_persistent"),
            Vec::<Box<[usize]>>::new(),
            &PersistenceParameters::default(),
        ));

        let (p, states) = setup_query_through(state, &[1], None, None);
        let expected: Vec<DfValue> = vec![2.into()];
        assert_query_through(p, 0, 2.into(), states, expected);
    }

    #[test]
    fn it_queries_through_w_literals() {
        let additional = Some(vec![DfValue::Int(42)]);
        let state = MaterializedNodeState::Memory(MemoryState::default());
        let (p, states) = setup_query_through(state, &[1], additional, None);
        let expected: Vec<DfValue> = vec![2.into(), 42.into()];
        assert_query_through(p, 0, 2.into(), states, expected);
    }

    #[test]
    fn it_queries_through_w_literals_persistent() {
        let additional = Some(vec![DfValue::Int(42)]);
        let state = MaterializedNodeState::Persistent(PersistentState::new(
            String::from("it_queries_through_w_literals"),
            Vec::<Box<[usize]>>::new(),
            &PersistenceParameters::default(),
        ));

        let (p, states) = setup_query_through(state, &[1], additional, None);
        let expected: Vec<DfValue> = vec![2.into(), 42.into()];
        assert_query_through(p, 0, 2.into(), states, expected);
    }

    #[test]
    fn it_queries_through_w_arithmetic_and_literals() {
        let additional = Some(vec![DfValue::Int(42)]);
        let expressions = Some(vec![Expr::Op {
            left: Box::new(make_int_column(0)),
            right: Box::new(make_int_column(1)),
            op: BinaryOperator::Add,
            ty: DfType::Int,
        }]);

        let state = MaterializedNodeState::Memory(MemoryState::default());
        let (p, states) = setup_query_through(state, &[1], additional, expressions);
        let expected: Vec<DfValue> = vec![2.into(), (1 + 2).into(), 42.into()];
        assert_query_through(p, 0, 2.into(), states, expected);
    }

    #[test]
    fn it_queries_through_w_arithmetic_and_literals_persistent() {
        let additional = Some(vec![DfValue::Int(42)]);
        let expressions = Some(vec![Expr::Op {
            left: Box::new(make_int_column(0)),
            right: Box::new(make_int_column(1)),
            op: BinaryOperator::Add,
            ty: DfType::Int,
        }]);

        let state = MaterializedNodeState::Persistent(PersistentState::new(
            String::from("it_queries_through_w_arithmetic_and_literals_persistent"),
            Vec::<Box<[usize]>>::new(),
            &PersistenceParameters::default(),
        ));

        let (p, states) = setup_query_through(state, &[1], additional, expressions);
        let expected: Vec<DfValue> = vec![2.into(), (1 + 2).into(), 42.into()];
        assert_query_through(p, 0, 2.into(), states, expected);
    }

    #[test]
    fn it_queries_nested_expressions() {
        let expression = Op {
            op: BinaryOperator::Multiply,
            left: Box::new(Op {
                left: Box::new(make_int_column(0)),
                right: Box::new(make_int_column(1)),
                op: BinaryOperator::Add,
                ty: DfType::Int,
            }),
            right: Box::new(make_literal(DfValue::Int(2))),
            ty: DfType::Int,
        };

        let state = MaterializedNodeState::Persistent(PersistentState::new(
            String::from("it_queries_nested_expressions"),
            Vec::<Box<[usize]>>::new(),
            &PersistenceParameters::default(),
        ));

        let (p, states) = setup_query_through(state, &[1], None, Some(vec![expression]));
        let expected: Vec<DfValue> = vec![2.into(), ((1 + 2) * 2).into()];
        assert_query_through(p, 0, 2.into(), states, expected);
    }

    #[test]
    fn it_suggests_indices() {
        let me = 1.into();
        let p = setup(false, false, true);
        let idx = p.node().suggest_indexes(me);
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn it_resolves() {
        let p = setup(false, false, true);
        assert_eq!(
            p.node().resolve(0),
            Some(vec![(p.narrow_base_id().as_global(), 2)])
        );
        assert_eq!(
            p.node().resolve(1),
            Some(vec![(p.narrow_base_id().as_global(), 0)])
        );
    }

    #[test]
    fn it_resolves_all() {
        let p = setup(false, true, true);
        assert_eq!(
            p.node().resolve(0),
            Some(vec![(p.narrow_base_id().as_global(), 0)])
        );
        assert_eq!(
            p.node().resolve(1),
            Some(vec![(p.narrow_base_id().as_global(), 1)])
        );
        assert_eq!(
            p.node().resolve(2),
            Some(vec![(p.narrow_base_id().as_global(), 2)])
        );
    }

    #[test]
    fn it_fails_to_resolve_literal() {
        let p = setup(false, false, true);
        assert!(p.node().resolve(2).is_none());
    }
}
