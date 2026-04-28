//! Dependency graph and topological sort
//!
//! Builds a DAG from hook specs and resolves execution order.

use std::collections::HashMap;

use petgraph::algo::toposort;
use petgraph::graph::{DiGraph, NodeIndex};

use crate::hooks::{HookId, HookSpec};

/// Error during dependency resolution
#[derive(Debug)]
pub enum DependencyError {
    CyclicDependency(String),
    UnknownDependency(String, String),
    /// **Attack #180 fix**: Dependency chain too deep.
    ExcessiveDepth {
        depth: usize,
        max: usize,
    },
}

impl std::fmt::Display for DependencyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CyclicDependency(hook) => {
                write!(f, "circular dependency detected involving hook: {hook}")
            }
            Self::UnknownDependency(hook, dep) => {
                write!(f, "hook '{hook}' depends on unknown hook '{dep}'")
            }
            Self::ExcessiveDepth { depth, max } => {
                write!(
                    f,
                    "hook dependency chain depth ({depth}) exceeds maximum ({max})"
                )
            }
        }
    }
}

impl std::error::Error for DependencyError {}

/// Resolved execution plan — hooks grouped into parallel batches
#[derive(Debug, Clone)]
pub struct ExecutionPlan {
    /// Batches of hooks that can run in parallel.
    /// Batches are ordered — batch N must complete before batch N+1 starts.
    pub batches: Vec<Vec<HookId>>,
}

/// Resolve hook dependencies into an ordered execution plan.
///
/// Hooks with no dependency edges between them are grouped into the same batch
/// for parallel execution. Hooks with dependencies are ordered correctly.
pub fn resolve(specs: &[HookSpec]) -> Result<ExecutionPlan, DependencyError> {
    if specs.is_empty() {
        return Ok(ExecutionPlan { batches: vec![] });
    }

    // Build index: HookId → spec
    let _id_map: HashMap<&HookId, &HookSpec> = specs.iter().map(|s| (&s.id, s)).collect();

    // Build petgraph DAG
    let mut graph = DiGraph::<&HookId, ()>::new();
    let mut node_map: HashMap<&HookId, NodeIndex> = HashMap::new();

    // Add all hooks as nodes
    for spec in specs {
        let idx = graph.add_node(&spec.id);
        node_map.insert(&spec.id, idx);
    }

    // Add dependency edges
    for spec in specs {
        let to_idx = node_map[&spec.id];
        for dep_id in &spec.depends_on {
            let from_idx = node_map.get(dep_id).ok_or_else(|| {
                DependencyError::UnknownDependency(spec.id.to_string(), dep_id.to_string())
            })?;
            // Edge from dependency → dependent (dep must run first)
            graph.add_edge(*from_idx, to_idx, ());
        }
    }

    // Topological sort
    let sorted = toposort(&graph, None).map_err(|cycle| {
        let node_id = graph[cycle.node_id()];
        DependencyError::CyclicDependency(node_id.to_string())
    })?;

    // Group into parallel batches based on depth
    // Depth = longest path from any root to this node
    let mut depths: HashMap<NodeIndex, usize> = HashMap::new();
    for &node_idx in &sorted {
        let max_parent_depth = graph
            .neighbors_directed(node_idx, petgraph::Direction::Incoming)
            .map(|parent| depths.get(&parent).copied().unwrap_or(0) + 1)
            .max()
            .unwrap_or(0);
        depths.insert(node_idx, max_parent_depth);
    }

    // **Attack #180 fix**: Reject excessively deep dependency chains.
    // A long chain creates many sequential batches, delaying phase_gate
    // execution. 50 levels is far beyond any legitimate hook config.
    const MAX_CHAIN_DEPTH: usize = 50;

    // Find max depth
    let max_depth = depths.values().copied().max().unwrap_or(0);
    if max_depth > MAX_CHAIN_DEPTH {
        return Err(DependencyError::ExcessiveDepth {
            depth: max_depth,
            max: MAX_CHAIN_DEPTH,
        });
    }

    // Build batches
    let mut batches: Vec<Vec<HookId>> = vec![vec![]; max_depth + 1];
    for &node_idx in &sorted {
        let depth = depths[&node_idx];
        let hook_id = graph[node_idx].clone();
        batches[depth].push(hook_id);
    }

    // Remove empty batches
    batches.retain(|b| !b.is_empty());

    Ok(ExecutionPlan { batches })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::HookEvent;

    fn spec(id: &str, deps: Vec<&str>) -> HookSpec {
        HookSpec {
            id: HookId::new(id),
            event: HookEvent::UserPromptSubmit,
            matcher: vec![],
            depends_on: deps.into_iter().map(HookId::new).collect(),
            has_api_call: false,
        }
    }

    #[test]
    fn test_no_hooks() {
        let plan = resolve(&[]).unwrap();
        assert!(plan.batches.is_empty());
    }

    #[test]
    fn test_independent_hooks_single_batch() {
        let specs = vec![spec("a", vec![]), spec("b", vec![]), spec("c", vec![])];
        let plan = resolve(&specs).unwrap();
        assert_eq!(plan.batches.len(), 1);
        assert_eq!(plan.batches[0].len(), 3);
    }

    #[test]
    fn test_linear_chain() {
        let specs = vec![
            spec("a", vec![]),
            spec("b", vec!["a"]),
            spec("c", vec!["b"]),
        ];
        let plan = resolve(&specs).unwrap();
        assert_eq!(plan.batches.len(), 3);
        assert_eq!(plan.batches[0][0].as_str(), "a");
        assert_eq!(plan.batches[1][0].as_str(), "b");
        assert_eq!(plan.batches[2][0].as_str(), "c");
    }

    #[test]
    fn test_diamond_dependency() {
        // a → b, a → c, b → d, c → d
        let specs = vec![
            spec("a", vec![]),
            spec("b", vec!["a"]),
            spec("c", vec!["a"]),
            spec("d", vec!["b", "c"]),
        ];
        let plan = resolve(&specs).unwrap();
        assert_eq!(plan.batches.len(), 3);
        assert_eq!(plan.batches[0].len(), 1); // a
        assert_eq!(plan.batches[1].len(), 2); // b, c (parallel)
        assert_eq!(plan.batches[2].len(), 1); // d
    }

    #[test]
    fn test_cyclic_dependency_error() {
        let specs = vec![spec("a", vec!["b"]), spec("b", vec!["a"])];
        assert!(matches!(
            resolve(&specs),
            Err(DependencyError::CyclicDependency(_))
        ));
    }

    #[test]
    fn test_unknown_dependency_error() {
        let specs = vec![spec("a", vec!["nonexistent"])];
        assert!(matches!(
            resolve(&specs),
            Err(DependencyError::UnknownDependency(_, _))
        ));
    }

    #[test]
    fn test_excessive_depth_rejected() {
        // Build a chain of 60 hooks: h0 → h1 → h2 → ... → h59
        let specs: Vec<HookSpec> = (0..60)
            .map(|i| {
                let deps = if i == 0 {
                    vec![]
                } else {
                    vec![format!("h{}", i - 1).as_str().to_string()]
                };
                HookSpec {
                    id: HookId::new(format!("h{i}")),
                    event: HookEvent::UserPromptSubmit,
                    matcher: vec![],
                    depends_on: deps.into_iter().map(|d| HookId::new(&d)).collect(),
                    has_api_call: false,
                }
            })
            .collect();

        assert!(matches!(
            resolve(&specs),
            Err(DependencyError::ExcessiveDepth { .. })
        ));
    }
}
