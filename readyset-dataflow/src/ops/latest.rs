use std::collections::HashMap;
use std::convert::TryInto;

use dataflow_state::PointKey;
use readyset_errors::{internal_err, ReadySetResult};
use serde::{Deserialize, Serialize};
use vec1::vec1;

use crate::prelude::*;
use crate::processing::{ColumnSource, LookupIndex};

/// Latest provides an operator that will maintain the last record for every group.
///
/// Whenever a new record arrives for a group, the latest operator will negative the previous
/// latest for that group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Latest {
    us: Option<IndexPair>,
    src: IndexPair,
    key: usize,
}

impl Latest {
    /// Construct a new latest operator.
    ///
    /// `src` should be the ancestor the operation is performed over, and `keys` should be a list
    /// of fields used to group records by. The latest record *within each group* will be
    /// maintained.
    pub fn new(src: NodeIndex, key: usize) -> Latest {
        Latest {
            us: None,
            src: src.into(),
            key,
        }
    }
}

impl Ingredient for Latest {
    fn take(&mut self) -> NodeOperator {
        Clone::clone(self).into()
    }

    fn ancestors(&self) -> Vec<NodeIndex> {
        vec![self.src.as_global()]
    }

    fn on_connected(&mut self, _: &Graph) {}

    impl_replace_sibling!(src);

    fn on_commit(&mut self, us: NodeIndex, remap: &HashMap<NodeIndex, IndexPair>) {
        self.src.remap(remap);
        self.us = Some(remap[&us]);
    }

    fn on_input(
        &mut self,
        from: LocalNodeIndex,
        rs: Records,
        replay: &ReplayContext,
        _: &DomainNodes,
        state: &StateMap,
    ) -> ReadySetResult<ProcessingResult> {
        debug_assert_eq!(from, *self.src);

        // find the current value for each group
        let us = self.us.unwrap();
        let db = state
            .get(*us)
            .ok_or_else(|| internal_err!("latest must have its own state materialized"))?;

        let mut misses = Vec::new();
        let mut lookups = Vec::new();
        let mut out = Vec::with_capacity(rs.len());
        {
            let currents = rs.into_iter().filter_map(|r| {
                // We don't allow standalone negatives as input to a latest. This is because it
                // would be very computationally expensive (and currently impossible) to find what
                // the *previous* latest was if the current latest was revoked.
                if !r.is_positive() {
                    return None;
                }

                match db.lookup(&[self.key], &PointKey::Single(r[self.key].clone())) {
                    LookupResult::Some(rs) => {
                        if replay.is_partial() {
                            lookups.push(Lookup {
                                on: *us,
                                cols: vec![self.key],
                                key: vec1![r[self.key].clone()].into(),
                            });
                        }

                        debug_assert!(rs.len() <= 1, "a group had more than 1 result");
                        Some((r, rs))
                    }
                    LookupResult::Missing => {
                        // we don't actively materialize holes unless requested by a read. this
                        // can't be a read, because reads cause replay, which fill holes with an
                        // empty set before processing!
                        misses.push(
                            Miss::builder()
                                .on(*us)
                                .lookup_idx(vec![self.key])
                                .lookup_key(vec![self.key])
                                .replay(replay)
                                .record(r.into_row())
                                .build(),
                        );
                        None
                    }
                }
            });

            // buffer emitted records
            for (r, current_row) in currents {
                if let Some(row) = current_row.into_iter().next() {
                    out.push(Record::Negative(row.into_owned()));
                }

                // if there was a previous latest for this key, revoke old record
                out.push(r);
            }
        }

        // TODO: check that there aren't any standalone negatives

        Ok(ProcessingResult {
            results: out.into(),
            lookups,
            misses,
        })
    }

    fn suggest_indexes(&self, this: NodeIndex) -> HashMap<NodeIndex, LookupIndex> {
        // index all key columns
        HashMap::from([(this, LookupIndex::Strict(Index::hash_map(vec![self.key])))])
    }

    fn column_source(&self, cols: &[usize]) -> ColumnSource {
        ColumnSource::exact_copy(self.src.as_global(), cols.try_into().unwrap())
    }

    fn description(&self, detailed: bool) -> String {
        if !detailed {
            String::from("⧖")
        } else {
            format!("⧖ γ[{}]", self.key)
        }
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unreachable)]
mod tests {
    use super::*;
    use crate::ops;

    fn setup(key: usize, mat: bool) -> ops::test::MockGraph {
        let mut g = ops::test::MockGraph::new();
        let s = g.add_base("source", &["x", "y"]);
        g.set_op("latest", &["x", "y"], Latest::new(s.as_global(), key), mat);
        g
    }

    // TODO: test when last *isn't* latest!

    #[test]
    fn it_describes() {
        let c = setup(0, false);
        assert_eq!(c.node().description(true), "⧖ γ[0]");
    }

    #[test]
    fn it_forwards() {
        let mut c = setup(0, true);

        let u = vec![1.into(), 1.into()];

        // first record for a group should emit just a positive
        let rs = c.narrow_one_row(u, true);
        assert_eq!(rs.len(), 1);
        let mut rs = rs.into_iter();

        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], 1.into());
            }
            _ => unreachable!(),
        }

        let u = vec![2.into(), 2.into()];

        // first record for a second group should also emit just a positive
        let rs = c.narrow_one_row(u, true);
        assert_eq!(rs.len(), 1);
        let mut rs = rs.into_iter();

        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 2.into());
                assert_eq!(r[1], 2.into());
            }
            _ => unreachable!(),
        }

        let u = vec![1.into(), 2.into()];

        // new record for existing group should revoke the old latest, and emit the new
        let rs = c.narrow_one_row(u, true);
        assert_eq!(rs.len(), 2);
        let mut rs = rs.into_iter();

        match rs.next().unwrap() {
            Record::Negative(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], 1.into());
            }
            _ => unreachable!(),
        }
        match rs.next().unwrap() {
            Record::Positive(r) => {
                assert_eq!(r[0], 1.into());
                assert_eq!(r[1], 2.into());
            }
            _ => unreachable!(),
        }

        let u = vec![
            (vec![1.into(), 1.into()], false),
            (vec![1.into(), 2.into()], false),
            (vec![1.into(), 3.into()], true),
            (vec![2.into(), 2.into()], false),
            (vec![2.into(), 4.into()], true),
        ];

        // negatives and positives should still result in only one new current for each group
        let rs = c.narrow_one(u, true);
        assert_eq!(rs.len(), 4); // one - and one + for each group
                                 // group 1 lost 2 and gained 3
        assert!(rs.iter().any(|r| if let Record::Negative(ref r) = *r {
            r[0] == 1.into() && r[1] == 2.into()
        } else {
            false
        }));
        assert!(rs.iter().any(|r| if let Record::Positive(ref r) = *r {
            r[0] == 1.into() && r[1] == 3.into()
        } else {
            false
        }));
        // group 2 lost 2 and gained 4
        assert!(rs.iter().any(|r| if let Record::Negative(ref r) = *r {
            r[0] == 2.into() && r[1] == 2.into()
        } else {
            false
        }));
        assert!(rs.iter().any(|r| if let Record::Positive(ref r) = *r {
            r[0] == 2.into() && r[1] == 4.into()
        } else {
            false
        }));
    }

    #[test]
    fn it_suggests_indices() {
        let me = 1.into();
        let c = setup(1, false);
        let idx = c.node().suggest_indexes(me);

        // should only add index on own columns
        assert_eq!(idx.len(), 1);
        assert!(idx.contains_key(&me));

        // should only index on the group-by column
        assert_eq!(idx[&me], LookupIndex::Strict(Index::hash_map(vec![1])));
    }

    #[test]
    fn it_resolves() {
        let c = setup(1, false);
        assert_eq!(
            c.node().resolve(0),
            Some(vec![(c.narrow_base_id().as_global(), 0)])
        );
        assert_eq!(
            c.node().resolve(1),
            Some(vec![(c.narrow_base_id().as_global(), 1)])
        );
        assert_eq!(
            c.node().resolve(2),
            Some(vec![(c.narrow_base_id().as_global(), 2)])
        );
    }
}
