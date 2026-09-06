//! The task dependency graph: `depends_on` edges, their validation, and the
//! plan file an orchestrator submits.
//!
//! `depends_on` is deliberately not `chained_to`. Chaining is the human's
//! fire-and-forget push — one parent, launched the moment it reaches Review,
//! carrying no context. A dependency is an AND-join the board *pulls*: a node
//! becomes ready only once every predecessor is finished, and the edge also
//! carries the upstream results into the node's prompt. Acyclicity is what
//! makes that terminate, so every write path that adds an edge goes through
//! [`check_acyclic`] first.

use std::collections::{HashMap, HashSet};

use serde::Deserialize;

use crate::core::error::{KanbanError, Result};
use crate::core::models::{Task, TaskStatus};

/// A dependency counts as satisfied once its task has reached Review or Done:
/// the same completion point `chained_to` fires on, so an agent `done` and a
/// human move both release the gate.
pub fn is_satisfied(task: &Task) -> bool {
    matches!(task.status, TaskStatus::Review | TaskStatus::Done)
}

/// How one task's dependencies stand right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Readiness {
    pub satisfied: Vec<String>,
    /// Dependencies that exist but have not reached Review/Done.
    pub blocked: Vec<String>,
    /// Dependencies whose task is gone (deleted or abandoned). Counted as
    /// satisfied — a vanished predecessor must not deadlock the graph — but
    /// reported so the release is never silent.
    pub missing: Vec<String>,
}

impl Readiness {
    pub fn is_ready(&self) -> bool {
        self.blocked.is_empty()
    }

    /// One-line explanation for the thread note and `kanban depends`.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.satisfied.is_empty() {
            parts.push(format!("done: {}", self.satisfied.join(", ")));
        }
        if !self.blocked.is_empty() {
            parts.push(format!("waiting on: {}", self.blocked.join(", ")));
        }
        if !self.missing.is_empty() {
            parts.push(format!("missing: {}", self.missing.join(", ")));
        }
        parts.join("; ")
    }
}

/// Index a board snapshot by task id for the graph walks below.
pub fn index_by_id(tasks: &[Task]) -> HashMap<&str, &Task> {
    tasks.iter().map(|task| (task.id.as_str(), task)).collect()
}

pub fn readiness(task: &Task, index: &HashMap<&str, &Task>) -> Readiness {
    let mut result = Readiness::default();
    for dependency in &task.depends_on {
        match index.get(dependency.as_str()) {
            Some(upstream) if is_satisfied(upstream) => result.satisfied.push(dependency.clone()),
            Some(_) => result.blocked.push(dependency.clone()),
            None => result.missing.push(dependency.clone()),
        }
    }
    result
}

/// Would `task_id` depending on `dependencies` close a cycle? Returns the
/// offending path so the caller can name it. `tasks` is the current board;
/// the proposed edges replace whatever `task_id` has today.
pub fn check_acyclic(tasks: &[Task], task_id: &str, dependencies: &[String]) -> Result<()> {
    let mut edges: HashMap<String, Vec<String>> = tasks
        .iter()
        .map(|task| (task.id.clone(), task.depends_on.clone()))
        .collect();
    edges.insert(task_id.to_string(), dependencies.to_vec());
    match find_cycle(&edges) {
        Some(path) => Err(KanbanError::Invalid(format!(
            "dependency cycle: {} — a task graph must stay acyclic or it can never become ready",
            path.join(" → ")
        ))),
        None => Ok(()),
    }
}

/// First cycle in an adjacency map, as the path that closes it
/// (`A → B → A`). Iterative DFS with the usual three colours: an edge back
/// into the current stack is the cycle.
pub fn find_cycle(edges: &HashMap<String, Vec<String>>) -> Option<Vec<String>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Open,
        Done,
    }
    let mut marks: HashMap<&str, Mark> = HashMap::new();
    // Deterministic start order: the same board must always report the same
    // cycle, whatever the hash map iteration order happens to be.
    let mut roots: Vec<&str> = edges.keys().map(String::as_str).collect();
    roots.sort_unstable();
    for root in roots {
        if marks.contains_key(root) {
            continue;
        }
        let mut stack: Vec<(&str, usize)> = vec![(root, 0)];
        marks.insert(root, Mark::Open);
        while let Some((node, next)) = stack.pop() {
            let children = edges.get(node).map(Vec::as_slice).unwrap_or(&[]);
            if next >= children.len() {
                marks.insert(node, Mark::Done);
                continue;
            }
            stack.push((node, next + 1));
            let child = children[next].as_str();
            match marks.get(child) {
                Some(Mark::Done) => {}
                Some(Mark::Open) => {
                    let mut path: Vec<String> = stack
                        .iter()
                        .skip_while(|(name, _)| *name != child)
                        .map(|(name, _)| (*name).to_string())
                        .collect();
                    path.push(child.to_string());
                    return Some(path);
                }
                None => {
                    // Unknown ids are leaves, not nodes: a dependency on a
                    // task that no longer exists cannot be part of a cycle.
                    if edges.contains_key(child) {
                        marks.insert(child, Mark::Open);
                        stack.push((child, 0));
                    }
                }
            }
        }
    }
    None
}

/// One node of an orchestrator plan.
#[derive(Debug, Clone, Deserialize)]
pub struct PlanNode {
    /// Plan-local handle the other nodes' `depends_on` entries point at.
    pub key: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// Plan keys and/or existing task ids this node waits for.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// The edge's context contract: what this node needs from upstream.
    #[serde(default)]
    pub needs: Option<String>,
    /// `orchestration.roles` profile the node runs on.
    #[serde(default)]
    pub role: Option<String>,
    /// Run the reviewer bot on this node even when it is off board-wide.
    #[serde(default)]
    pub reviewer: bool,
    /// Run the designer bot on this node even when it is off board-wide.
    #[serde(default)]
    pub designer: bool,
}

/// The YAML an orchestrator writes and `kanban plan` ingests.
#[derive(Debug, Clone, Deserialize)]
pub struct Plan {
    /// Free-text rationale recorded on the parent's thread.
    #[serde(default)]
    pub summary: Option<String>,
    pub nodes: Vec<PlanNode>,
}

impl Plan {
    pub fn parse(yaml: &str) -> Result<Self> {
        let plan: Plan = serde_yaml_ng::from_str(yaml)
            .map_err(|err| KanbanError::Invalid(format!("invalid plan file: {err}")))?;
        Ok(plan)
    }

    /// Reject a plan that could not run *before* a single node is created:
    /// unknown references, duplicate or ambiguous keys, a cycle, an unknown
    /// role profile, or more nodes than the board allows. Cheap static checks
    /// on the plan are the whole reason the graph is declared up front.
    pub fn validate(
        &self,
        parent_id: &str,
        tasks: &[Task],
        max_subtasks: usize,
        known_roles: &[String],
    ) -> Result<()> {
        if self.nodes.is_empty() {
            return Err(KanbanError::Invalid(
                "plan has no nodes: a plan must decompose the task into at least one subtask"
                    .to_string(),
            ));
        }
        if self.nodes.len() > max_subtasks {
            return Err(KanbanError::Invalid(format!(
                "plan has {} nodes, more than orchestration.orchestrator.max_subtasks ({max_subtasks})",
                self.nodes.len()
            )));
        }
        let existing: HashSet<&str> = tasks.iter().map(|task| task.id.as_str()).collect();
        let mut keys: HashSet<&str> = HashSet::new();
        for node in &self.nodes {
            let key = node.key.trim();
            if key.is_empty() {
                return Err(KanbanError::Invalid(
                    "plan node has an empty key: every node needs a handle other nodes can \
                     depend on"
                        .to_string(),
                ));
            }
            if node.title.trim().is_empty() {
                return Err(KanbanError::Invalid(format!(
                    "plan node '{key}' has an empty title"
                )));
            }
            if !keys.insert(key) {
                return Err(KanbanError::Invalid(format!(
                    "plan node key '{key}' is used twice"
                )));
            }
            if existing.contains(key) {
                return Err(KanbanError::Invalid(format!(
                    "plan node key '{key}' collides with an existing task id; keys are \
                     plan-local handles"
                )));
            }
            if let Some(role) = node
                .role
                .as_deref()
                .map(str::trim)
                .filter(|r| !r.is_empty())
                && !known_roles.iter().any(|known| known == role)
            {
                return Err(KanbanError::Invalid(format!(
                    "plan node '{key}' asks for role '{role}', which is not configured under \
                     orchestration.roles (available: {})",
                    if known_roles.is_empty() {
                        "none".to_string()
                    } else {
                        known_roles.join(", ")
                    }
                )));
            }
        }
        for node in &self.nodes {
            for dependency in &node.depends_on {
                let dependency = dependency.trim();
                if dependency == node.key.trim() {
                    return Err(KanbanError::Invalid(format!(
                        "plan node '{dependency}' depends on itself"
                    )));
                }
                if dependency == parent_id {
                    return Err(KanbanError::Invalid(format!(
                        "plan node '{}' depends on the orchestrated task {parent_id}, which \
                         waits for every node in the plan",
                        node.key.trim()
                    )));
                }
                if !keys.contains(dependency) && !existing.contains(dependency) {
                    return Err(KanbanError::Invalid(format!(
                        "plan node '{}' depends on '{dependency}', which is neither a plan key \
                         nor an existing task",
                        node.key.trim()
                    )));
                }
            }
        }
        // Cycle check over the *whole* board plus the plan: the plan keys stand
        // in for the ids the nodes will get, and the parent joins on all of
        // them, so an edge from a node to a task that (transitively) waits on
        // the parent is caught here rather than at run time.
        let mut edges: HashMap<String, Vec<String>> = tasks
            .iter()
            .map(|task| (task.id.clone(), task.depends_on.clone()))
            .collect();
        for node in &self.nodes {
            edges.insert(
                node.key.trim().to_string(),
                node.depends_on
                    .iter()
                    .map(|d| d.trim().to_string())
                    .collect(),
            );
        }
        edges.insert(
            parent_id.to_string(),
            self.nodes
                .iter()
                .map(|node| node.key.trim().to_string())
                .collect(),
        );
        if let Some(path) = find_cycle(&edges) {
            return Err(KanbanError::Invalid(format!(
                "plan is cyclic: {} — a task graph must stay acyclic or it can never become ready",
                path.join(" → ")
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, status: TaskStatus, depends_on: &[&str]) -> Task {
        let mut task = Task::new(id, format!("task {id}"));
        task.status = status;
        task.depends_on = depends_on.iter().map(|d| (*d).to_string()).collect();
        task
    }

    #[test]
    fn readiness_needs_every_dependency_finished() {
        let tasks = vec![
            task("TASK-1", TaskStatus::Review, &[]),
            task("TASK-2", TaskStatus::InProgress, &[]),
            task("TASK-3", TaskStatus::Todo, &["TASK-1", "TASK-2"]),
        ];
        let index = index_by_id(&tasks);
        let state = readiness(&tasks[2], &index);
        assert!(!state.is_ready());
        assert_eq!(state.satisfied, vec!["TASK-1".to_string()]);
        assert_eq!(state.blocked, vec!["TASK-2".to_string()]);
    }

    #[test]
    fn a_deleted_dependency_releases_the_gate_but_is_reported() {
        let tasks = vec![task("TASK-3", TaskStatus::Todo, &["TASK-9"])];
        let index = index_by_id(&tasks);
        let state = readiness(&tasks[0], &index);
        assert!(state.is_ready(), "a vanished predecessor must not deadlock");
        assert_eq!(state.missing, vec!["TASK-9".to_string()]);
    }

    #[test]
    fn a_cycle_is_rejected_with_its_path() {
        let tasks = vec![
            task("TASK-1", TaskStatus::Todo, &["TASK-2"]),
            task("TASK-2", TaskStatus::Todo, &[]),
        ];
        let err = check_acyclic(&tasks, "TASK-2", &["TASK-1".to_string()])
            .expect_err("A → B → A must be refused");
        assert!(err.to_string().contains("dependency cycle"), "{err}");
        // …and the same edge in isolation is fine.
        check_acyclic(&tasks, "TASK-2", &[]).expect("no edge, no cycle");
    }

    #[test]
    fn self_dependency_is_a_cycle() {
        let tasks = vec![task("TASK-1", TaskStatus::Todo, &[])];
        assert!(check_acyclic(&tasks, "TASK-1", &["TASK-1".to_string()]).is_err());
    }

    #[test]
    fn a_diamond_is_not_a_cycle() {
        let tasks = [
            task("TASK-1", TaskStatus::Todo, &[]),
            task("TASK-2", TaskStatus::Todo, &["TASK-1"]),
            task("TASK-3", TaskStatus::Todo, &["TASK-1"]),
            task("TASK-4", TaskStatus::Todo, &["TASK-2", "TASK-3"]),
        ];
        let edges: HashMap<String, Vec<String>> = tasks
            .iter()
            .map(|t| (t.id.clone(), t.depends_on.clone()))
            .collect();
        assert_eq!(find_cycle(&edges), None);
    }

    fn plan(yaml: &str) -> Plan {
        Plan::parse(yaml).expect("plan parses")
    }

    #[test]
    fn a_valid_plan_passes() {
        let plan = plan(
            "nodes:\n\
             - key: a\n  title: First\n\
             - key: b\n  title: Second\n  depends_on: [a]\n  role: cheap\n",
        );
        let tasks = vec![task("TASK-1", TaskStatus::InProgress, &[])];
        plan.validate("TASK-1", &tasks, 12, &["cheap".to_string()])
            .expect("valid plan");
    }

    #[test]
    fn a_plan_is_rejected_before_any_task_is_created() {
        let tasks = vec![task("TASK-1", TaskStatus::InProgress, &[])];
        let cases = [
            // unknown reference
            (
                "nodes:\n- key: a\n  title: A\n  depends_on: [ghost]\n",
                "ghost",
            ),
            // duplicate key
            (
                "nodes:\n- key: a\n  title: A\n- key: a\n  title: B\n",
                "twice",
            ),
            // cycle inside the plan
            (
                "nodes:\n- key: a\n  title: A\n  depends_on: [b]\n- key: b\n  title: B\n  depends_on: [a]\n",
                "cyclic",
            ),
            // depending on the orchestrated task itself
            (
                "nodes:\n- key: a\n  title: A\n  depends_on: [TASK-1]\n",
                "waits for every node",
            ),
            // unknown role profile
            (
                "nodes:\n- key: a\n  title: A\n  role: ghost\n",
                "orchestration.roles",
            ),
            // empty title
            ("nodes:\n- key: a\n  title: '  '\n", "empty title"),
        ];
        for (yaml, needle) in cases {
            let err = plan(yaml)
                .validate("TASK-1", &tasks, 12, &["cheap".to_string()])
                .expect_err(&format!("must be refused: {yaml}"));
            assert!(
                err.to_string().contains(needle),
                "expected {needle:?} in {err}"
            );
        }
    }

    #[test]
    fn a_plan_larger_than_the_cap_is_refused() {
        let yaml = (0..5)
            .map(|i| format!("- key: n{i}\n  title: N{i}\n"))
            .collect::<String>();
        let err = plan(&format!("nodes:\n{yaml}"))
            .validate("TASK-1", &[], 4, &[])
            .expect_err("over the cap");
        assert!(err.to_string().contains("max_subtasks"), "{err}");
    }
}
