// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use std::fmt::Display;
use std::hash::Hash;
use std::sync::Arc;
use std::vec::IntoIter;

use crate::equivalence::add_offset_to_expr;
use crate::{LexOrdering, PhysicalExpr};
use arrow_schema::SortOptions;

/// An `OrderingEquivalenceClass` object keeps track of different alternative
/// orderings than can describe a schema. For example, consider the following table:
///
/// ```text
/// |a|b|c|d|
/// |1|4|3|1|
/// |2|3|3|2|
/// |3|1|2|2|
/// |3|2|1|3|
/// ```
///
/// Here, both `vec![a ASC, b ASC]` and `vec![c DESC, d ASC]` describe the table
/// ordering. In this case, we say that these orderings are equivalent.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Default)]
pub struct OrderingEquivalenceClass {
    orderings: Vec<LexOrdering>,
}

impl OrderingEquivalenceClass {
    /// Creates new empty ordering equivalence class.
    pub fn empty() -> Self {
        Default::default()
    }

    /// Clears (empties) this ordering equivalence class.
    pub fn clear(&mut self) {
        self.orderings.clear();
    }

    /// Creates new ordering equivalence class from the given orderings
    ///
    /// Any redundant entries are removed
    pub fn new(orderings: Vec<LexOrdering>) -> Self {
        let mut result = Self { orderings };
        result.remove_redundant_entries();
        result
    }

    /// Converts this OrderingEquivalenceClass to a vector of orderings.
    pub fn into_inner(self) -> Vec<LexOrdering> {
        self.orderings
    }

    /// Checks whether `ordering` is a member of this equivalence class.
    pub fn contains(&self, ordering: &LexOrdering) -> bool {
        self.orderings.contains(ordering)
    }

    /// Adds `ordering` to this equivalence class.
    #[allow(dead_code)]
    #[deprecated(
        since = "45.0.0",
        note = "use OrderingEquivalenceClass::add_new_ordering instead"
    )]
    fn push(&mut self, ordering: LexOrdering) {
        self.add_new_ordering(ordering)
    }

    /// Checks whether this ordering equivalence class is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns an iterator over the equivalent orderings in this class.
    ///
    /// Note this class also implements [`IntoIterator`] to return an iterator
    /// over owned [`LexOrdering`]s.
    pub fn iter(&self) -> impl Iterator<Item = &LexOrdering> {
        self.orderings.iter()
    }

    /// Returns how many equivalent orderings there are in this class.
    pub fn len(&self) -> usize {
        self.orderings.len()
    }

    /// Extend this ordering equivalence class with the `other` class.
    pub fn extend(&mut self, other: Self) {
        self.orderings.extend(other.orderings);
        // Make sure that there are no redundant orderings:
        self.remove_redundant_entries();
    }

    /// Adds new orderings into this ordering equivalence class
    pub fn add_new_orderings(
        &mut self,
        orderings: impl IntoIterator<Item = LexOrdering>,
    ) {
        self.orderings.extend(orderings);
        // Make sure that there are no redundant orderings:
        self.remove_redundant_entries();
    }

    /// Adds a single ordering to the existing ordering equivalence class.
    pub fn add_new_ordering(&mut self, ordering: LexOrdering) {
        self.add_new_orderings([ordering]);
    }

    /// Removes redundant orderings from this equivalence class.
    ///
    /// For instance, if we already have the ordering `[a ASC, b ASC, c DESC]`,
    /// then there is no need to keep ordering `[a ASC, b ASC]` in the state.
    fn remove_redundant_entries(&mut self) {
        let mut work = true;
        while work {
            work = false;
            let mut idx = 0;
            while idx < self.orderings.len() {
                let mut ordering_idx = idx + 1;
                let mut removal = self.orderings[idx].is_empty();
                while ordering_idx < self.orderings.len() {
                    work |= self.resolve_overlap(idx, ordering_idx);
                    if self.orderings[idx].is_empty() {
                        removal = true;
                        break;
                    }
                    work |= self.resolve_overlap(ordering_idx, idx);
                    if self.orderings[ordering_idx].is_empty() {
                        self.orderings.swap_remove(ordering_idx);
                    } else {
                        ordering_idx += 1;
                    }
                }
                if removal {
                    self.orderings.swap_remove(idx);
                } else {
                    idx += 1;
                }
            }
        }
    }

    /// Trims `orderings[idx]` if some suffix of it overlaps with a prefix of
    /// `orderings[pre_idx]`. Returns `true` if there is any overlap, `false` otherwise.
    ///
    /// For example, if `orderings[idx]` is `[a ASC, b ASC, c DESC]` and
    /// `orderings[pre_idx]` is `[b ASC, c DESC]`, then the function will trim
    /// `orderings[idx]` to `[a ASC]`.
    fn resolve_overlap(&mut self, idx: usize, pre_idx: usize) -> bool {
        let length = self.orderings[idx].len();
        let other_length = self.orderings[pre_idx].len();
        for overlap in 1..=length.min(other_length) {
            if self.orderings[idx][length - overlap..]
                == self.orderings[pre_idx][..overlap]
            {
                self.orderings[idx].truncate(length - overlap);
                return true;
            }
        }
        false
    }

    /// Returns the concatenation of all the orderings. This enables merge
    /// operations to preserve all equivalent orderings simultaneously.
    pub fn output_ordering(&self) -> Option<LexOrdering> {
        let output_ordering = self
            .orderings
            .iter()
            .flatten()
            .cloned()
            .collect::<LexOrdering>()
            .collapse();
        (!output_ordering.is_empty()).then_some(output_ordering)
    }

    // Append orderings in `other` to all existing orderings in this equivalence
    // class.
    pub fn join_suffix(mut self, other: &Self) -> Self {
        let n_ordering = self.orderings.len();
        // Replicate entries before cross product
        let n_cross = std::cmp::max(n_ordering, other.len() * n_ordering);
        self.orderings = self
            .orderings
            .iter()
            .cloned()
            .cycle()
            .take(n_cross)
            .collect();
        // Suffix orderings of other to the current orderings.
        for (outer_idx, ordering) in other.iter().enumerate() {
            for idx in 0..n_ordering {
                // Calculate cross product index
                let idx = outer_idx * n_ordering + idx;
                self.orderings[idx].extend(ordering.iter().cloned());
            }
        }
        self
    }

    /// Adds `offset` value to the index of each expression inside this
    /// ordering equivalence class.
    pub fn add_offset(&mut self, offset: usize) {
        for ordering in self.orderings.iter_mut() {
            ordering.transform(|sort_expr| {
                sort_expr.expr = add_offset_to_expr(Arc::clone(&sort_expr.expr), offset);
            })
        }
    }

    /// Gets sort options associated with this expression if it is a leading
    /// ordering expression. Otherwise, returns `None`.
    pub fn get_options(&self, expr: &Arc<dyn PhysicalExpr>) -> Option<SortOptions> {
        for ordering in self.iter() {
            let leading_ordering = &ordering[0];
            if leading_ordering.expr.eq(expr) {
                return Some(leading_ordering.options);
            }
        }
        None
    }
}

/// Convert the `OrderingEquivalenceClass` into an iterator of LexOrderings
impl IntoIterator for OrderingEquivalenceClass {
    type Item = LexOrdering;
    type IntoIter = IntoIter<LexOrdering>;

    fn into_iter(self) -> Self::IntoIter {
        self.orderings.into_iter()
    }
}

impl Display for OrderingEquivalenceClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[")?;
        let mut iter = self.orderings.iter();
        if let Some(ordering) = iter.next() {
            write!(f, "[{}]", ordering)?;
        }
        for ordering in iter {
            write!(f, ", [{}]", ordering)?;
        }
        write!(f, "]")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::equivalence::tests::{
        convert_to_orderings, convert_to_sort_exprs, create_test_schema,
    };
    use crate::equivalence::{
        EquivalenceClass, EquivalenceGroup, EquivalenceProperties,
        OrderingEquivalenceClass,
    };
    use crate::expressions::{col, BinaryExpr, Column};
    use crate::utils::tests::TestScalarUDF;
    use crate::{
        AcrossPartitions, ConstExpr, PhysicalExpr, PhysicalExprRef, PhysicalSortExpr,
        ScalarFunctionExpr,
    };

    use arrow::datatypes::{DataType, Field, Schema};
    use arrow_schema::SortOptions;
    use datafusion_common::Result;
    use datafusion_expr::{Operator, ScalarUDF};
    use datafusion_physical_expr_common::sort_expr::LexOrdering;

    #[test]
    fn test_ordering_satisfy() -> Result<()> {
        let input_schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int64, true),
            Field::new("b", DataType::Int64, true),
        ]));
        let crude = LexOrdering::new(vec![PhysicalSortExpr {
            expr: Arc::new(Column::new("a", 0)),
            options: SortOptions::default(),
        }]);
        let finer = LexOrdering::new(vec![
            PhysicalSortExpr {
                expr: Arc::new(Column::new("a", 0)),
                options: SortOptions::default(),
            },
            PhysicalSortExpr {
                expr: Arc::new(Column::new("b", 1)),
                options: SortOptions::default(),
            },
        ]);
        // finer ordering satisfies, crude ordering should return true
        let eq_properties_finer = EquivalenceProperties::new_with_orderings(
            Arc::clone(&input_schema),
            &[finer.clone()],
        );
        assert!(eq_properties_finer.ordering_satisfy(crude.as_ref()));

        // Crude ordering doesn't satisfy finer ordering. should return false
        let eq_properties_crude = EquivalenceProperties::new_with_orderings(
            Arc::clone(&input_schema),
            &[crude.clone()],
        );
        assert!(!eq_properties_crude.ordering_satisfy(finer.as_ref()));
        Ok(())
    }

    #[test]
    fn test_ordering_satisfy_with_equivalence2() -> Result<()> {
        let test_schema = create_test_schema()?;
        let col_a = &col("a", &test_schema)?;
        let col_b = &col("b", &test_schema)?;
        let col_c = &col("c", &test_schema)?;
        let col_d = &col("d", &test_schema)?;
        let col_e = &col("e", &test_schema)?;
        let col_f = &col("f", &test_schema)?;
        let test_fun = Arc::new(ScalarUDF::new_from_impl(TestScalarUDF::new()));

        let floor_a = Arc::new(ScalarFunctionExpr::try_new(
            Arc::clone(&test_fun),
            vec![Arc::clone(col_a)],
            &test_schema,
        )?) as PhysicalExprRef;
        let floor_f = Arc::new(ScalarFunctionExpr::try_new(
            Arc::clone(&test_fun),
            vec![Arc::clone(col_f)],
            &test_schema,
        )?) as PhysicalExprRef;
        let exp_a = Arc::new(ScalarFunctionExpr::try_new(
            Arc::clone(&test_fun),
            vec![Arc::clone(col_a)],
            &test_schema,
        )?) as PhysicalExprRef;

        let a_plus_b = Arc::new(BinaryExpr::new(
            Arc::clone(col_a),
            Operator::Plus,
            Arc::clone(col_b),
        )) as Arc<dyn PhysicalExpr>;
        let options = SortOptions {
            descending: false,
            nulls_first: false,
        };

        let test_cases = vec![
            // ------------ TEST CASE 1 ------------
            (
                // orderings
                vec![
                    // [a ASC, d ASC, b ASC]
                    vec![(col_a, options), (col_d, options), (col_b, options)],
                    // [c ASC]
                    vec![(col_c, options)],
                ],
                // equivalence classes
                vec![vec![col_a, col_f]],
                // constants
                vec![col_e],
                // requirement [a ASC, b ASC], requirement is not satisfied.
                vec![(col_a, options), (col_b, options)],
                // expected: requirement is not satisfied.
                false,
            ),
            // ------------ TEST CASE 2 ------------
            (
                // orderings
                vec![
                    // [a ASC, c ASC, b ASC]
                    vec![(col_a, options), (col_c, options), (col_b, options)],
                    // [d ASC]
                    vec![(col_d, options)],
                ],
                // equivalence classes
                vec![vec![col_a, col_f]],
                // constants
                vec![col_e],
                // requirement [floor(a) ASC],
                vec![(&floor_a, options)],
                // expected: requirement is satisfied.
                true,
            ),
            // ------------ TEST CASE 2.1 ------------
            (
                // orderings
                vec![
                    // [a ASC, c ASC, b ASC]
                    vec![(col_a, options), (col_c, options), (col_b, options)],
                    // [d ASC]
                    vec![(col_d, options)],
                ],
                // equivalence classes
                vec![vec![col_a, col_f]],
                // constants
                vec![col_e],
                // requirement [floor(f) ASC], (Please note that a=f)
                vec![(&floor_f, options)],
                // expected: requirement is satisfied.
                true,
            ),
            // ------------ TEST CASE 3 ------------
            (
                // orderings
                vec![
                    // [a ASC, c ASC, b ASC]
                    vec![(col_a, options), (col_c, options), (col_b, options)],
                    // [d ASC]
                    vec![(col_d, options)],
                ],
                // equivalence classes
                vec![vec![col_a, col_f]],
                // constants
                vec![col_e],
                // requirement [a ASC, c ASC, a+b ASC],
                vec![(col_a, options), (col_c, options), (&a_plus_b, options)],
                // expected: requirement is satisfied.
                true,
            ),
            // ------------ TEST CASE 4 ------------
            (
                // orderings
                vec![
                    // [a ASC, b ASC, c ASC, d ASC]
                    vec![
                        (col_a, options),
                        (col_b, options),
                        (col_c, options),
                        (col_d, options),
                    ],
                ],
                // equivalence classes
                vec![vec![col_a, col_f]],
                // constants
                vec![col_e],
                // requirement [floor(a) ASC, a+b ASC],
                vec![(&floor_a, options), (&a_plus_b, options)],
                // expected: requirement is satisfied.
                false,
            ),
            // ------------ TEST CASE 5 ------------
            (
                // orderings
                vec![
                    // [a ASC, b ASC, c ASC, d ASC]
                    vec![
                        (col_a, options),
                        (col_b, options),
                        (col_c, options),
                        (col_d, options),
                    ],
                ],
                // equivalence classes
                vec![vec![col_a, col_f]],
                // constants
                vec![col_e],
                // requirement [exp(a) ASC, a+b ASC],
                vec![(&exp_a, options), (&a_plus_b, options)],
                // expected: requirement is not satisfied.
                // TODO: If we know that exp function is 1-to-1 function.
                //  we could have deduced that above requirement is satisfied.
                false,
            ),
            // ------------ TEST CASE 6 ------------
            (
                // orderings
                vec![
                    // [a ASC, d ASC, b ASC]
                    vec![(col_a, options), (col_d, options), (col_b, options)],
                    // [c ASC]
                    vec![(col_c, options)],
                ],
                // equivalence classes
                vec![vec![col_a, col_f]],
                // constants
                vec![col_e],
                // requirement [a ASC, d ASC, floor(a) ASC],
                vec![(col_a, options), (col_d, options), (&floor_a, options)],
                // expected: requirement is satisfied.
                true,
            ),
            // ------------ TEST CASE 7 ------------
            (
                // orderings
                vec![
                    // [a ASC, c ASC, b ASC]
                    vec![(col_a, options), (col_c, options), (col_b, options)],
                    // [d ASC]
                    vec![(col_d, options)],
                ],
                // equivalence classes
                vec![vec![col_a, col_f]],
                // constants
                vec![col_e],
                // requirement [a ASC, floor(a) ASC, a + b ASC],
                vec![(col_a, options), (&floor_a, options), (&a_plus_b, options)],
                // expected: requirement is not satisfied.
                false,
            ),
            // ------------ TEST CASE 8 ------------
            (
                // orderings
                vec![
                    // [a ASC, b ASC, c ASC]
                    vec![(col_a, options), (col_b, options), (col_c, options)],
                    // [d ASC]
                    vec![(col_d, options)],
                ],
                // equivalence classes
                vec![vec![col_a, col_f]],
                // constants
                vec![col_e],
                // requirement [a ASC, c ASC, floor(a) ASC, a + b ASC],
                vec![
                    (col_a, options),
                    (col_c, options),
                    (&floor_a, options),
                    (&a_plus_b, options),
                ],
                // expected: requirement is not satisfied.
                false,
            ),
            // ------------ TEST CASE 9 ------------
            (
                // orderings
                vec![
                    // [a ASC, b ASC, c ASC, d ASC]
                    vec![
                        (col_a, options),
                        (col_b, options),
                        (col_c, options),
                        (col_d, options),
                    ],
                ],
                // equivalence classes
                vec![vec![col_a, col_f]],
                // constants
                vec![col_e],
                // requirement [a ASC, b ASC, c ASC, floor(a) ASC],
                vec![
                    (col_a, options),
                    (col_b, options),
                    (col_c, options),
                    (&floor_a, options),
                ],
                // expected: requirement is satisfied.
                true,
            ),
            // ------------ TEST CASE 10 ------------
            (
                // orderings
                vec![
                    // [d ASC, b ASC]
                    vec![(col_d, options), (col_b, options)],
                    // [c ASC, a ASC]
                    vec![(col_c, options), (col_a, options)],
                ],
                // equivalence classes
                vec![vec![col_a, col_f]],
                // constants
                vec![col_e],
                // requirement [c ASC, d ASC, a + b ASC],
                vec![(col_c, options), (col_d, options), (&a_plus_b, options)],
                // expected: requirement is satisfied.
                true,
            ),
        ];

        for (orderings, eq_group, constants, reqs, expected) in test_cases {
            let err_msg =
                format!("error in test orderings: {orderings:?}, eq_group: {eq_group:?}, constants: {constants:?}, reqs: {reqs:?}, expected: {expected:?}");
            let mut eq_properties = EquivalenceProperties::new(Arc::clone(&test_schema));
            let orderings = convert_to_orderings(&orderings);
            eq_properties.add_new_orderings(orderings);
            let eq_group = eq_group
                .into_iter()
                .map(|eq_class| {
                    let eq_classes = eq_class.into_iter().cloned().collect::<Vec<_>>();
                    EquivalenceClass::new(eq_classes)
                })
                .collect::<Vec<_>>();
            let eq_group = EquivalenceGroup::new(eq_group);
            eq_properties.add_equivalence_group(eq_group);

            let constants = constants.into_iter().map(|expr| {
                ConstExpr::from(expr)
                    .with_across_partitions(AcrossPartitions::Uniform(None))
            });
            eq_properties = eq_properties.with_constants(constants);

            let reqs = convert_to_sort_exprs(&reqs);
            assert_eq!(
                eq_properties.ordering_satisfy(reqs.as_ref()),
                expected,
                "{}",
                err_msg
            );
        }

        Ok(())
    }

    #[test]
    fn test_ordering_satisfy_different_lengths() -> Result<()> {
        let test_schema = create_test_schema()?;
        let col_a = &col("a", &test_schema)?;
        let col_b = &col("b", &test_schema)?;
        let col_c = &col("c", &test_schema)?;
        let col_d = &col("d", &test_schema)?;
        let col_e = &col("e", &test_schema)?;
        let col_f = &col("f", &test_schema)?;
        let options = SortOptions {
            descending: false,
            nulls_first: false,
        };
        // a=c (e.g they are aliases).
        let mut eq_properties = EquivalenceProperties::new(test_schema);
        eq_properties.add_equal_conditions(col_a, col_c)?;

        let orderings = vec![
            vec![(col_a, options)],
            vec![(col_e, options)],
            vec![(col_d, options), (col_f, options)],
        ];
        let orderings = convert_to_orderings(&orderings);

        // Column [a ASC], [e ASC], [d ASC, f ASC] are all valid orderings for the schema.
        eq_properties.add_new_orderings(orderings);

        // First entry in the tuple is required ordering, second entry is the expected flag
        // that indicates whether this required ordering is satisfied.
        // ([a ASC], true) indicate a ASC requirement is already satisfied by existing orderings.
        let test_cases = vec![
            // [c ASC, a ASC, e ASC], expected represents this requirement is satisfied
            (
                vec![(col_c, options), (col_a, options), (col_e, options)],
                true,
            ),
            (vec![(col_c, options), (col_b, options)], false),
            (vec![(col_c, options), (col_d, options)], true),
            (
                vec![(col_d, options), (col_f, options), (col_b, options)],
                false,
            ),
            (vec![(col_d, options), (col_f, options)], true),
        ];

        for (reqs, expected) in test_cases {
            let err_msg =
                format!("error in test reqs: {:?}, expected: {:?}", reqs, expected,);
            let reqs = convert_to_sort_exprs(&reqs);
            assert_eq!(
                eq_properties.ordering_satisfy(reqs.as_ref()),
                expected,
                "{}",
                err_msg
            );
        }

        Ok(())
    }

    #[test]
    fn test_remove_redundant_entries_oeq_class() -> Result<()> {
        let schema = create_test_schema()?;
        let col_a = &col("a", &schema)?;
        let col_b = &col("b", &schema)?;
        let col_c = &col("c", &schema)?;
        let col_d = &col("d", &schema)?;
        let col_e = &col("e", &schema)?;

        let option_asc = SortOptions {
            descending: false,
            nulls_first: false,
        };
        let option_desc = SortOptions {
            descending: true,
            nulls_first: true,
        };

        // First entry in the tuple is the given orderings for the table
        // Second entry is the simplest version of the given orderings that is functionally equivalent.
        let test_cases = vec![
            // ------- TEST CASE 1 ---------
            (
                // ORDERINGS GIVEN
                vec![
                    // [a ASC, b ASC]
                    vec![(col_a, option_asc), (col_b, option_asc)],
                ],
                // EXPECTED orderings that is succinct.
                vec![
                    // [a ASC, b ASC]
                    vec![(col_a, option_asc), (col_b, option_asc)],
                ],
            ),
            // ------- TEST CASE 2 ---------
            (
                // ORDERINGS GIVEN
                vec![
                    // [a ASC, b ASC]
                    vec![(col_a, option_asc), (col_b, option_asc)],
                    // [a ASC, b ASC, c ASC]
                    vec![
                        (col_a, option_asc),
                        (col_b, option_asc),
                        (col_c, option_asc),
                    ],
                ],
                // EXPECTED orderings that is succinct.
                vec![
                    // [a ASC, b ASC, c ASC]
                    vec![
                        (col_a, option_asc),
                        (col_b, option_asc),
                        (col_c, option_asc),
                    ],
                ],
            ),
            // ------- TEST CASE 3 ---------
            (
                // ORDERINGS GIVEN
                vec![
                    // [a ASC, b DESC]
                    vec![(col_a, option_asc), (col_b, option_desc)],
                    // [a ASC]
                    vec![(col_a, option_asc)],
                    // [a ASC, c ASC]
                    vec![(col_a, option_asc), (col_c, option_asc)],
                ],
                // EXPECTED orderings that is succinct.
                vec![
                    // [a ASC, b DESC]
                    vec![(col_a, option_asc), (col_b, option_desc)],
                    // [a ASC, c ASC]
                    vec![(col_a, option_asc), (col_c, option_asc)],
                ],
            ),
            // ------- TEST CASE 4 ---------
            (
                // ORDERINGS GIVEN
                vec![
                    // [a ASC, b ASC]
                    vec![(col_a, option_asc), (col_b, option_asc)],
                    // [a ASC, b ASC, c ASC]
                    vec![
                        (col_a, option_asc),
                        (col_b, option_asc),
                        (col_c, option_asc),
                    ],
                    // [a ASC]
                    vec![(col_a, option_asc)],
                ],
                // EXPECTED orderings that is succinct.
                vec![
                    // [a ASC, b ASC, c ASC]
                    vec![
                        (col_a, option_asc),
                        (col_b, option_asc),
                        (col_c, option_asc),
                    ],
                ],
            ),
            // ------- TEST CASE 5 ---------
            // Empty ordering
            (
                vec![vec![]],
                // No ordering in the state (empty ordering is ignored).
                vec![],
            ),
            // ------- TEST CASE 6 ---------
            (
                // ORDERINGS GIVEN
                vec![
                    // [a ASC, b ASC]
                    vec![(col_a, option_asc), (col_b, option_asc)],
                    // [b ASC]
                    vec![(col_b, option_asc)],
                ],
                // EXPECTED orderings that is succinct.
                vec![
                    // [a ASC]
                    vec![(col_a, option_asc)],
                    // [b ASC]
                    vec![(col_b, option_asc)],
                ],
            ),
            // ------- TEST CASE 7 ---------
            // b, a
            // c, a
            // d, b, c
            (
                // ORDERINGS GIVEN
                vec![
                    // [b ASC, a ASC]
                    vec![(col_b, option_asc), (col_a, option_asc)],
                    // [c ASC, a ASC]
                    vec![(col_c, option_asc), (col_a, option_asc)],
                    // [d ASC, b ASC, c ASC]
                    vec![
                        (col_d, option_asc),
                        (col_b, option_asc),
                        (col_c, option_asc),
                    ],
                ],
                // EXPECTED orderings that is succinct.
                vec![
                    // [b ASC, a ASC]
                    vec![(col_b, option_asc), (col_a, option_asc)],
                    // [c ASC, a ASC]
                    vec![(col_c, option_asc), (col_a, option_asc)],
                    // [d ASC]
                    vec![(col_d, option_asc)],
                ],
            ),
            // ------- TEST CASE 8 ---------
            // b, e
            // c, a
            // d, b, e, c, a
            (
                // ORDERINGS GIVEN
                vec![
                    // [b ASC, e ASC]
                    vec![(col_b, option_asc), (col_e, option_asc)],
                    // [c ASC, a ASC]
                    vec![(col_c, option_asc), (col_a, option_asc)],
                    // [d ASC, b ASC, e ASC, c ASC, a ASC]
                    vec![
                        (col_d, option_asc),
                        (col_b, option_asc),
                        (col_e, option_asc),
                        (col_c, option_asc),
                        (col_a, option_asc),
                    ],
                ],
                // EXPECTED orderings that is succinct.
                vec![
                    // [b ASC, e ASC]
                    vec![(col_b, option_asc), (col_e, option_asc)],
                    // [c ASC, a ASC]
                    vec![(col_c, option_asc), (col_a, option_asc)],
                    // [d ASC]
                    vec![(col_d, option_asc)],
                ],
            ),
            // ------- TEST CASE 9 ---------
            // b
            // a, b, c
            // d, a, b
            (
                // ORDERINGS GIVEN
                vec![
                    // [b ASC]
                    vec![(col_b, option_asc)],
                    // [a ASC, b ASC, c ASC]
                    vec![
                        (col_a, option_asc),
                        (col_b, option_asc),
                        (col_c, option_asc),
                    ],
                    // [d ASC, a ASC, b ASC]
                    vec![
                        (col_d, option_asc),
                        (col_a, option_asc),
                        (col_b, option_asc),
                    ],
                ],
                // EXPECTED orderings that is succinct.
                vec![
                    // [b ASC]
                    vec![(col_b, option_asc)],
                    // [a ASC, b ASC, c ASC]
                    vec![
                        (col_a, option_asc),
                        (col_b, option_asc),
                        (col_c, option_asc),
                    ],
                    // [d ASC]
                    vec![(col_d, option_asc)],
                ],
            ),
        ];
        for (orderings, expected) in test_cases {
            let orderings = convert_to_orderings(&orderings);
            let expected = convert_to_orderings(&expected);
            let actual = OrderingEquivalenceClass::new(orderings.clone());
            let actual = actual.orderings;
            let err_msg = format!(
                "orderings: {:?}, expected: {:?}, actual :{:?}",
                orderings, expected, actual
            );
            assert_eq!(actual.len(), expected.len(), "{}", err_msg);
            for elem in actual {
                assert!(expected.contains(&elem), "{}", err_msg);
            }
        }

        Ok(())
    }
}
