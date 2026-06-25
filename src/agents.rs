//! The coxn side of an aden task partition (`aden scope --agents`).
//!
//! aden emits a partition as per-sub-scope manifest files plus a line-oriented
//! index on stdout (see `docs/routing.adoc` section 3). coxn reads that index
//! the cheap way -- tab-separated fields, no JSON -- and orders the sub-scopes by
//! dependency. This is the foundation the sub-agent runner builds on: one pump
//! per sub-scope, model chosen by role, gated by the sub-scope's own manifest.

/// One entry in a task partition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubScope {
    /// Stable id, e.g. `<task-slug>-0` or `<task-slug>-merge`.
    pub id: String,
    /// Role tag for model routing (scout / synth / orchestrate or user-defined).
    pub role: String,
    /// Repo-relative path to this sub-scope's manifest (the gate consumes it).
    pub manifest: String,
    /// Ids this sub-scope depends on (must complete first).
    pub depends_on: Vec<String>,
}

/// Parse the partition index. Each non-empty line is four tab-separated fields:
/// `<id>\t<role>\t<manifest-path>\t<comma-separated deps>` (the fourth empty for
/// a leaf). Lines without at least the first three fields are skipped (lenient).
pub fn parse_index(index: &str) -> Vec<SubScope> {
    index
        .lines()
        .filter_map(|line| {
            if line.trim().is_empty() {
                return None;
            }
            let mut fields = line.split('\t');
            let id = fields.next()?.trim();
            let role = fields.next()?.trim();
            let manifest = fields.next()?.trim();
            if id.is_empty() || role.is_empty() || manifest.is_empty() {
                return None;
            }
            let depends_on = fields
                .next()
                .unwrap_or("")
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            Some(SubScope {
                id: id.to_string(),
                role: role.to_string(),
                manifest: manifest.to_string(),
                depends_on,
            })
        })
        .collect()
}

/// Order sub-scopes so each appears after the ones it depends on (a stable
/// topological sort). A scope whose deps are missing or cyclic is emitted once
/// no further progress is possible, so the function always returns every scope.
pub fn dependency_order(scopes: &[SubScope]) -> Vec<&SubScope> {
    let mut done: Vec<&str> = Vec::new();
    let mut ordered: Vec<&SubScope> = Vec::new();
    let mut remaining: Vec<&SubScope> = scopes.iter().collect();
    while !remaining.is_empty() {
        // Emit every scope whose deps are all already emitted, preserving order.
        let ready: Vec<usize> = remaining
            .iter()
            .enumerate()
            .filter(|(_, s)| s.depends_on.iter().all(|d| done.contains(&d.as_str())))
            .map(|(i, _)| i)
            .collect();
        let take = if ready.is_empty() {
            // A cycle or a missing dep: break it by taking the first remaining.
            vec![0]
        } else {
            ready
        };
        for &i in &take {
            ordered.push(remaining[i]);
            done.push(&remaining[i].id);
        }
        // Remove taken (high to low so indices stay valid).
        for &i in take.iter().rev() {
            remaining.remove(i);
        }
    }
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX: &str = "\
partition-demo-0\tscout\t.aden/agents/partition-demo-0.json\t
partition-demo-1\tsynth\t.aden/agents/partition-demo-1.json\t
partition-demo-merge\torchestrate\t.aden/agents/partition-demo-merge.json\tpartition-demo-0,partition-demo-1
";

    #[test]
    fn parse_index_reads_four_columns_and_deps() {
        let scopes = parse_index(INDEX);
        assert_eq!(scopes.len(), 3);
        assert_eq!(scopes[0].id, "partition-demo-0");
        assert_eq!(scopes[0].role, "scout");
        assert_eq!(scopes[0].manifest, ".aden/agents/partition-demo-0.json");
        assert!(scopes[0].depends_on.is_empty(), "leaf has no deps");
        assert_eq!(
            scopes[2].depends_on,
            vec![
                "partition-demo-0".to_string(),
                "partition-demo-1".to_string()
            ]
        );
    }

    #[test]
    fn parse_index_skips_malformed_lines() {
        let scopes = parse_index("\n  \nonly-one-field\nid\trole\tpath\n");
        assert_eq!(scopes.len(), 1);
        assert_eq!(scopes[0].id, "id");
        assert!(scopes[0].depends_on.is_empty());
    }

    #[test]
    fn dependency_order_puts_deps_first() {
        let scopes = parse_index(INDEX);
        let ordered = dependency_order(&scopes);
        let ids: Vec<&str> = ordered.iter().map(|s| s.id.as_str()).collect();
        // The merge (depends on both leaves) must come last.
        assert_eq!(ids.last(), Some(&"partition-demo-merge"));
        let merge_pos = ids
            .iter()
            .position(|i| *i == "partition-demo-merge")
            .unwrap();
        let leaf_pos = ids.iter().position(|i| *i == "partition-demo-0").unwrap();
        assert!(leaf_pos < merge_pos);
    }

    #[test]
    fn dependency_order_breaks_cycles_and_returns_all() {
        // Two scopes that depend on each other: still both returned.
        let scopes = vec![
            SubScope {
                id: "a".into(),
                role: "x".into(),
                manifest: "a.json".into(),
                depends_on: vec!["b".into()],
            },
            SubScope {
                id: "b".into(),
                role: "y".into(),
                manifest: "b.json".into(),
                depends_on: vec!["a".into()],
            },
        ];
        assert_eq!(dependency_order(&scopes).len(), 2);
    }
}
