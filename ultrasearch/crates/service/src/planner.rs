use ipc::QueryExpr;

/// Optimizes a raw query AST for execution.
pub struct QueryPlanner;

impl QueryPlanner {
    /// Optimize the query expression.
    pub fn optimize(expr: QueryExpr) -> QueryExpr {
        // TODO: Analyze `ext:` terms to select specialized fields/analyzers in the future.
        Self::flatten(Self::push_down_not(expr))
    }

    /// Distribute NOTs: `Not(And([A, B]))` -> `Or([Not(A), Not(B)])` (De Morgan's).
    /// This canonicalizes negations to be closer to leaves.
    fn push_down_not(expr: QueryExpr) -> QueryExpr {
        match expr {
            QueryExpr::Not(inner) => match *inner {
                QueryExpr::Not(sub) => Self::push_down_not(*sub), // Double negation
                QueryExpr::And(subs) => {
                    // Not(A and B) -> Not(A) or Not(B)
                    QueryExpr::Or(
                        subs.into_iter()
                            .map(|s| QueryExpr::Not(Box::new(Self::push_down_not(s))))
                            .collect(),
                    )
                }
                QueryExpr::Or(subs) => {
                    // Not(A or B) -> Not(A) and Not(B)
                    QueryExpr::And(
                        subs.into_iter()
                            .map(|s| QueryExpr::Not(Box::new(Self::push_down_not(s))))
                            .collect(),
                    )
                }
                leaf => QueryExpr::Not(Box::new(Self::push_down_not(leaf))),
            },
            QueryExpr::And(subs) => {
                QueryExpr::And(subs.into_iter().map(Self::push_down_not).collect())
            }
            QueryExpr::Or(subs) => {
                QueryExpr::Or(subs.into_iter().map(Self::push_down_not).collect())
            }
            leaf => leaf,
        }
    }

    /// Flatten nested ANDs and ORs.
    /// `And([And([A, B]), C])` -> `And([A, B, C])`.
    fn flatten(expr: QueryExpr) -> QueryExpr {
        match expr {
            QueryExpr::And(subs) => {
                let mut flat = Vec::with_capacity(subs.len());
                for sub in subs {
                    let sub = Self::flatten(sub);
                    if let QueryExpr::And(inner) = sub {
                        flat.extend(inner);
                    } else {
                        flat.push(sub);
                    }
                }
                // Optimize single-child AND
                if flat.len() == 1 {
                    flat.pop().unwrap()
                } else {
                    QueryExpr::And(flat)
                }
            }
            QueryExpr::Or(subs) => {
                let mut flat = Vec::with_capacity(subs.len());
                for sub in subs {
                    let sub = Self::flatten(sub);
                    if let QueryExpr::Or(inner) = sub {
                        flat.extend(inner);
                    } else {
                        flat.push(sub);
                    }
                }
                if flat.len() == 1 {
                    flat.pop().unwrap()
                } else {
                    QueryExpr::Or(flat)
                }
            }
            QueryExpr::Not(inner) => QueryExpr::Not(Box::new(Self::flatten(*inner))),
            leaf => leaf,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipc::{TermExpr, TermModifier};

    fn term(val: &str) -> QueryExpr {
        QueryExpr::Term(TermExpr {
            field: None,
            value: val.into(),
            modifier: TermModifier::Term,
        })
    }

    #[test]
    fn test_flatten_and() {
        let q = QueryExpr::And(vec![
            term("A"),
            QueryExpr::And(vec![term("B"), term("C")]),
            term("D"),
        ]);
        let optimized = QueryPlanner::optimize(q);
        assert!(matches!(&optimized, QueryExpr::And(_)), "expected And");

        if let QueryExpr::And(subs) = optimized {
            assert_eq!(subs.len(), 4);
        }
    }

    #[test]
    fn test_push_down_not() {
        // Not(A or B) -> Not(A) and Not(B)
        let q = QueryExpr::Not(Box::new(QueryExpr::Or(vec![term("A"), term("B")])));
        let optimized = QueryPlanner::optimize(q);
        assert!(matches!(&optimized, QueryExpr::And(_)), "expected And");

        if let QueryExpr::And(subs) = optimized {
            assert_eq!(subs.len(), 2);
            assert!(matches!(subs[0], QueryExpr::Not(_)));
            assert!(matches!(subs[1], QueryExpr::Not(_)));
        }
    }
}
