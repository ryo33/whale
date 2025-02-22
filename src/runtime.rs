use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use papaya::{Compute, HashMap, Operation};

use crate::{
    node::Node, Dependencies, Invalidation, InvalidationCollector, InvalidationPropagation,
    InvalidationReason, Pointer, QueryId, RevisionPointer, UninvalidationCollector, Version,
};

#[derive(Debug, Default, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
/// Runtime is a data structure that manages the all state of the dependency tracking system.
///
/// This is cheap to clone, so you can pass it around by just cloning it.
pub struct Runtime {
    nodes: Arc<HashMap<QueryId, Node, ahash::RandomState>>,
    next_version: Arc<AtomicU64>,
}

#[test]
fn test_send_sync() {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    assert_send::<Runtime>();
    assert_sync::<Runtime>();
}

impl Runtime {
    /// Create a new runtime.
    pub fn new() -> Self {
        Default::default()
    }

    /// Get the node for a query.
    ///
    /// This may read a not invalidated node while processing a new version of dependencies or removing a node.
    pub fn get(&self, query_id: QueryId) -> Option<Node> {
        self.nodes.pin().get(&query_id).cloned()
    }

    /// Iterate over all nodes.
    pub fn keys(&self) -> Vec<QueryId> {
        self.nodes.pin().keys().cloned().collect()
    }

    /// Helper method to notify collector and propagate invalidation if needed
    fn notify_and_propagate(
        &self,
        target: Pointer,
        invalidation: Invalidation,
        collector: &mut impl InvalidationCollector,
    ) {
        if collector.notify(target, invalidation) == InvalidationPropagation::Propagate {
            self.invalidate(target, invalidation, collector);
        }
    }

    /// Remove a node from the runtime. If the node is returned and has dependents, you should update them.
    #[must_use]
    pub fn remove(
        &self,
        query_id: QueryId,
        collector: &mut impl InvalidationCollector,
    ) -> QueryRemovalResult {
        let removed = self.nodes.pin().remove(&query_id).cloned();
        if let Some(removed) = &removed {
            for dependent in removed.dependents.iter() {
                self.notify_and_propagate(
                    dependent,
                    Invalidation::new_source(
                        removed.revision_pointer(),
                        InvalidationReason::Removed,
                    ),
                    collector,
                );
            }
        }
        QueryRemovalResult { removed }
    }

    /// Helper method to notify collector and propagate uninvalidation if needed
    fn notify_and_propagate_uninvalidation(
        &self,
        target: Pointer,
        source: RevisionPointer,
        collector: &mut impl InvalidationCollector,
        uninvalidation_collector: &mut impl UninvalidationCollector,
    ) {
        if uninvalidation_collector.notify(target, source) == InvalidationPropagation::Propagate {
            self.remove_invalidator(target, source, collector, uninvalidation_collector);
        }
    }

    /// Uninvalidate a specific revision pointer.
    pub fn uninvalidate(
        &self,
        revision_pointer: RevisionPointer,
        collector: &mut impl InvalidationCollector,
        uninvalidation_collector: &mut impl UninvalidationCollector,
    ) {
        let pinned = self.nodes.pin();
        enum AbortReason {
            NotFound,
            AlreadyUpdated,
            MoreInvalidated,
            AlreadyUninvalidated,
        }
        let result = pinned.compute(revision_pointer.pointer.query_id, |node| {
            let Some((_, node)) = node else {
                return Operation::Abort(AbortReason::NotFound);
            };
            if !node.is_invalidated() {
                return Operation::Abort(AbortReason::AlreadyUninvalidated);
            }
            if node.version != revision_pointer.pointer.version {
                return Operation::Abort(AbortReason::AlreadyUpdated);
            }
            if node.invalidation_revision.0 > revision_pointer.invalidation_revision.0 {
                return Operation::Abort(AbortReason::MoreInvalidated);
            }
            let mut node = node.clone();
            node.invalidations = Default::default();
            // This does not increase the invalidation version.
            Operation::Insert(node)
        });
        match result {
            Compute::Inserted(_, _) => unreachable!(),
            Compute::Updated {
                old: (_, old),
                new: _,
            } => {
                // Recursively uninvalidate dependents if they were invalidated by this node
                for dependent in old.dependents.iter() {
                    self.notify_and_propagate_uninvalidation(
                        dependent,
                        revision_pointer,
                        collector,
                        uninvalidation_collector,
                    );
                }
            }
            Compute::Removed(_, _) => unreachable!(),
            Compute::Aborted(_) => {}
        }
    }

    /// Remove an invalidator from a specific pointer.
    pub fn remove_invalidator(
        &self,
        pointer: Pointer,
        invalidator: RevisionPointer,
        collector: &mut impl InvalidationCollector,
        uninvalidation_collector: &mut impl UninvalidationCollector,
    ) {
        let pinned = self.nodes.pin();
        let result = pinned.compute(pointer.query_id, |node| {
            let Some((_, node)) = node else {
                return Operation::Abort(());
            };
            if node.version != pointer.version {
                return Operation::Abort(());
            }
            if !node.is_invalidated() {
                return Operation::Abort(());
            }
            if let Some(node) = node.remove_invalidation(invalidator) {
                Operation::Insert(node)
            } else {
                Operation::Abort(())
            }
        });
        match result {
            Compute::Inserted(_, _) => unreachable!(),
            Compute::Updated {
                old: (_, old),
                new: (_, new),
            } => {
                // Check if the invalidator has a newer version
                self.ensure_tracking_dependent_by_latest_version(invalidator, pointer, collector);
                if !new.is_invalidated() {
                    // recursively uninvalidate dependents
                    for dependent in new.dependents.iter() {
                        self.notify_and_propagate_uninvalidation(
                            dependent,
                            old.revision_pointer(),
                            collector,
                            uninvalidation_collector,
                        );
                    }
                }
            }
            Compute::Removed(_, _) => unreachable!(),
            Compute::Aborted(_) => {}
        }
    }

    fn ensure_tracking_dependent_by_latest_version(
        &self,
        pointer: RevisionPointer,
        dependent: Pointer,
        collector: &mut impl InvalidationCollector,
    ) {
        let pinned = self.nodes.pin();
        let result = pinned.compute(pointer.pointer.query_id, |node| {
            let Some((_, node)) = node else {
                return Operation::Abort(());
            };
            if node.version > pointer.pointer.version {
                let mut node = node.clone();
                node.dependents = node.dependents.added(dependent);
                Operation::Insert(node)
            } else {
                Operation::Abort(())
            }
        });
        match result {
            Compute::Inserted(_, _) => unreachable!(),
            Compute::Updated {
                new: (_, latest), ..
            } => {
                self.notify_and_propagate(
                    dependent,
                    Invalidation::new_source(
                        latest.revision_pointer(),
                        InvalidationReason::NewVersion,
                    ),
                    collector,
                );
            }
            Compute::Removed(_, _) => unreachable!(),
            Compute::Aborted(_) => {}
        }
    }

    /// Detect a cycle in the dependency graph.
    pub fn has_cycle(&self, query_id: QueryId) -> bool {
        let mut set = ahash::HashSet::default();
        self.has_cycle_with_set(query_id, &mut set)
    }

    /// Detect a cycle in the dependency graph with a set.
    pub fn has_cycle_with_set(&self, query_id: QueryId, set: &mut ahash::HashSet<QueryId>) -> bool {
        if set.contains(&query_id) {
            return true;
        }
        set.insert(query_id);
        for dependency in self.get(query_id).unwrap().dependencies.iter() {
            if self.has_cycle_with_set(dependency.query_id, set) {
                return true;
            }
        }
        false
    }

    /// Remove a query if it is not depended by any other queries. This is useful for garbage collection.
    pub fn remove_if_unused(&self, query_id: QueryId) -> Option<Node> {
        let pinned = self.nodes.pin();
        let result = pinned.compute(query_id, |node| {
            let Some((_, node)) = node else {
                return Operation::Abort(());
            };
            if node.dependents.is_empty() {
                Operation::Remove
            } else {
                Operation::Abort(())
            }
        });
        match result {
            Compute::Inserted(_, _) => unreachable!(),
            Compute::Updated { .. } => unreachable!(),
            Compute::Removed(_, node) => Some(node.clone()),
            Compute::Aborted(_) => None,
        }
    }

    /// Recursively invalidate a specific version of a query, and returns the pointers of the invalidated nodes.
    pub fn invalidate(
        &self,
        pointer: Pointer,
        invalidation: Invalidation,
        collector: &mut impl InvalidationCollector,
    ) {
        if invalidation.source.pointer == pointer {
            eprintln!("cyclic dependency detected on {:?}", pointer);
            return;
        }
        let pinned = self.nodes.pin();
        let result = pinned.compute(pointer.query_id, |node| {
            let Some((_, node)) = node else {
                return Operation::Abort(());
            };
            if node.pointer().has_influence_on_update(pointer) {
                let mut node = node.clone();
                node.invalidations = node.invalidations.pushed(invalidation);
                node.invalidation_revision.0 += 1;
                Operation::Insert(node)
            } else {
                Operation::Abort(())
            }
        });
        match result {
            Compute::Inserted(_, _) => unreachable!(),
            Compute::Updated {
                old: _,
                new: (_, node),
            } => {
                if collector.notify(pointer, invalidation) == InvalidationPropagation::Propagate {
                    // Recursively invalidate dependents
                    for dependent in node.dependents.iter() {
                        self.invalidate(
                            dependent,
                            Invalidation {
                                source: invalidation.source,
                                dependency: node.revision_pointer(),
                                reason: InvalidationReason::DependencyInvalidated,
                            },
                            collector,
                        );
                    }
                }
            }
            Compute::Removed(_, _) => unreachable!(),
            Compute::Aborted(_) => {}
        }
    }

    /// Register a new version for a query. If the previous version is returned, you should update all dependents.
    #[must_use]
    pub fn register(
        &self,
        query_id: QueryId,
        dependencies: Vec<Pointer>,
        collector: &mut impl InvalidationCollector,
    ) -> QueryRegistrationResult {
        let dependencies = Dependencies::new(dependencies);
        let (new_node, old_node) = self.create_new_version(query_id, dependencies.clone());

        let invalidations = self.update_dependency_graph(new_node.pointer(), dependencies);
        let new_node = self
            .extend_invalidations(new_node.pointer(), invalidations)
            .unwrap_or(new_node);

        if let Some(old_node) = &old_node {
            for dependent in old_node.dependents.iter() {
                self.notify_and_propagate(
                    dependent,
                    Invalidation::new_source(
                        new_node.revision_pointer(),
                        InvalidationReason::NewVersion,
                    ),
                    collector,
                );
            }
        }

        QueryRegistrationResult {
            node: new_node,
            old_node,
        }
    }

    /// Register a new version for a query that has no change. So, dependents are not invalidated.
    ///
    /// This returns invalidated pointers that are invalidated when the dependencies are updated.
    pub fn update_dependencies(
        &self,
        query_id: QueryId,
        dependencies: Vec<Pointer>,
        collector: &mut impl InvalidationCollector,
    ) -> QueryRegistrationResult {
        let dependencies = Dependencies::new(dependencies);
        let (new_node, old_node) = self.create_new_version(query_id, dependencies.clone());

        let invalidations = self.update_dependency_graph(new_node.pointer(), dependencies);
        let new_node = self
            .extend_invalidations(new_node.pointer(), invalidations)
            .unwrap_or(new_node);

        if let Some(old_node) = &old_node {
            for dependent in old_node.dependents.iter() {
                self.update_dependency_version(
                    dependent,
                    old_node.revision_pointer(),
                    new_node.revision_pointer(),
                );
                if new_node.is_invalidated() {
                    self.notify_and_propagate(
                        dependent,
                        Invalidation::new_source(
                            new_node.revision_pointer(),
                            InvalidationReason::DependencyInvalidated,
                        ),
                        collector,
                    );
                }
            }
        }
        QueryRegistrationResult {
            node: new_node,
            old_node,
        }
    }

    #[must_use]
    fn create_new_version(
        &self,
        query_id: QueryId,
        dependencies: Dependencies,
    ) -> (Node, Option<Node>) {
        // Register the new version
        enum InsertAbortReason {
            AlreadyOld,
        }
        let pinned = self.nodes.pin();
        let result = pinned.compute(query_id, |previous| {
            let version = Version(self.next_version.fetch_add(1, Ordering::Relaxed));
            if let Some((_, node)) = previous {
                if node.version.0 >= version.0 {
                    return Operation::Abort(InsertAbortReason::AlreadyOld);
                }
            }

            Operation::Insert(Node {
                id: query_id,
                version,
                dependencies: dependencies.clone(),
                dependents: Default::default(),
                invalidation_revision: Default::default(),
                invalidations: Default::default(),
            })
        });

        match result {
            Compute::Inserted(_key, new) => (new.clone(), None),
            Compute::Updated {
                old: (_, old),
                new: (_, new),
            } => (new.clone(), Some(old.clone())),
            Compute::Removed(_, _) => unreachable!(),
            Compute::Aborted(InsertAbortReason::AlreadyOld) => unreachable!(
                "version is calculated within atomic computation, so latter version should be always newer"
            ),
        }
    }

    /// Update dependencies for a query atomically
    #[must_use]
    fn update_dependency_graph(
        &self,
        pointer: Pointer,
        dependencies: Dependencies,
    ) -> Vec<Invalidation> {
        let mut invalidations = vec![];
        for dependency in dependencies.iter() {
            enum AbortReason {
                NotFound,
                AlreadyInvalidated(Invalidation),
                AlreadyUpdated(Invalidation),
            }
            let pinned = self.nodes.pin();
            let result = pinned.compute(dependency.query_id, |node| {
                let Some((_, node)) = node else {
                    return Operation::Abort(AbortReason::NotFound);
                };
                if node.version != dependency.version {
                    assert!(node.version.0 > dependency.version.0);
                    return Operation::Abort(AbortReason::AlreadyUpdated(
                        Invalidation::new_source(
                            node.revision_pointer(),
                            InvalidationReason::NewVersion,
                        ),
                    ));
                }
                if node.is_invalidated() {
                    return Operation::Abort(AbortReason::AlreadyInvalidated(
                        Invalidation::new_source(
                            node.revision_pointer(),
                            InvalidationReason::DependencyInvalidated,
                        ),
                    ));
                }
                let mut node = node.clone();
                node.dependents = node.dependents.added(pointer);
                Operation::Insert(node)
            });
            match result {
                Compute::Inserted(_, _) => {}
                Compute::Updated { .. } => {}
                Compute::Removed(_, _) => {}
                Compute::Aborted(AbortReason::NotFound) => {}
                Compute::Aborted(AbortReason::AlreadyInvalidated(invalidation)) => {
                    invalidations.push(invalidation);
                }
                Compute::Aborted(AbortReason::AlreadyUpdated(invalidation)) => {
                    invalidations.push(invalidation);
                }
            }
        }
        invalidations
    }

    /// Extend invalidations to a node, and returns the new node.
    #[must_use]
    fn extend_invalidations(
        &self,
        pointer: Pointer,
        invalidations: Vec<Invalidation>,
    ) -> Option<Node> {
        if !invalidations.is_empty() {
            let pinned = self.nodes.pin();
            let result = pinned.compute(pointer.query_id, |node| {
                let Some((_, node)) = node else {
                    return Operation::Abort(());
                };
                if node.version != pointer.version {
                    return Operation::Abort(());
                }
                let mut node = node.clone();
                node.add_invalidations(invalidations.clone());
                Operation::Insert(node)
            });
            match result {
                Compute::Inserted(_, _) => unreachable!(),
                Compute::Updated {
                    old: _,
                    new: (_, new),
                } => {
                    return Some(new.clone());
                }
                Compute::Removed(_, _) => unreachable!(),
                Compute::Aborted(_) => {}
            }
        }
        None
    }

    fn update_dependency_version(
        &self,
        pointer: Pointer,
        previous: RevisionPointer,
        new: RevisionPointer,
    ) {
        let pinned = self.nodes.pin();
        pinned.update(pointer.query_id, |node| {
            node.update_version_reference(previous, new)
        });
    }
}

/// QueryRegistrationResult is a result of registering a new version.
pub struct QueryRegistrationResult {
    /// Node is the new node after registering a new version.
    pub node: Node,
    /// Previous version of the node.
    pub old_node: Option<Node>,
}

impl QueryRegistrationResult {
    /// Returns true if the node is invalidated or the old node has dependents.
    pub fn has_invalidation_effects(&self) -> bool {
        self.node.is_invalidated()
            || self
                .old_node
                .as_ref()
                .map(|n| !n.dependents.is_empty())
                .unwrap_or(false)
    }
}

/// QueryRemovalResult is a result of `Runtime::remove`.
pub struct QueryRemovalResult {
    /// Removed is the node that is removed.
    pub removed: Option<Node>,
}

#[cfg(test)]
mod tests {
    use crate::{
        Dependents, Invalidations, PropagateInvalidationCollector, PropagateUninvalidationCollector,
    };

    use super::*;

    // Test basic node creation and versioning
    #[test]
    fn test_basic_registration() {
        let rt = Runtime::new();
        let id = QueryId(1);

        // First registration
        let result = rt.register(id, vec![], &mut PropagateInvalidationCollector);
        assert_eq!(result.node.id, id);
        assert_eq!(result.node.version.0, 0);
        assert!(result.old_node.is_none());

        // Second registration should increment version
        let result2 = rt.register(id, vec![], &mut PropagateInvalidationCollector);
        assert_eq!(result2.node.version.0, 1);
        assert_eq!(result2.old_node, Some(result.node));
    }

    // Test dependency tracking basics
    #[test]
    fn test_dependency_management() {
        let rt = Runtime::new();
        let a = QueryId(1);
        let b = QueryId(2);

        // b depends on a
        let result = rt.register(a, vec![], &mut PropagateInvalidationCollector);
        let b_result = rt.register(
            b,
            vec![result.node.pointer()],
            &mut PropagateInvalidationCollector,
        );

        // Verify dependency tracking
        let a_node = rt.get(a).unwrap();
        assert_eq!(
            a_node.dependents,
            Dependents::from_iter([b_result.node.pointer()])
        );
        assert_eq!(
            b_result.node.dependencies,
            Dependencies::from_iter([a_node.pointer()])
        );
    }

    // Test basic invalidation flow
    #[test]
    fn test_dependency_invalidation() {
        let rt = Runtime::new();
        let a = QueryId(1);
        let b = QueryId(2);

        // Setup initial state
        let result = rt.register(a, vec![], &mut PropagateInvalidationCollector);
        let _ = rt.register(
            b,
            vec![result.node.pointer()],
            &mut PropagateInvalidationCollector,
        );

        // Update a to invalidate b
        let a_update = rt.register(a, vec![], &mut PropagateInvalidationCollector);
        let b_node = rt.get(b).unwrap();
        assert_eq!(
            b_node.invalidations,
            Invalidations::from_iter([Invalidation {
                source: a_update.node.revision_pointer(),
                dependency: a_update.node.revision_pointer(),
                reason: InvalidationReason::NewVersion,
            }])
        );
    }

    // Test removal invalidation
    #[test]
    fn test_removal_invalidation() {
        let rt = Runtime::new();
        let a = QueryId(1);
        let b = QueryId(2);

        let result = rt.register(a, vec![], &mut PropagateInvalidationCollector);
        let _ = rt.register(
            b,
            vec![result.node.pointer()],
            &mut PropagateInvalidationCollector,
        );

        // Remove a and check b invalidation
        let removal = rt.remove(a, &mut PropagateInvalidationCollector);
        assert!(removal.removed.is_some());
        let b_node = rt.get(b).unwrap();
        assert!(b_node.is_invalidated());
        assert_eq!(
            b_node.invalidations,
            Invalidations::from_iter([Invalidation {
                source: result.node.revision_pointer(),
                dependency: result.node.revision_pointer(),
                reason: InvalidationReason::Removed,
            }])
        );
    }

    // Test dependency chain invalidation
    #[test]
    fn test_transitive_invalidation() {
        let rt = Runtime::new();
        let a = QueryId(1);
        let b = QueryId(2);
        let c = QueryId(3);

        let result = rt.register(a, vec![], &mut PropagateInvalidationCollector);
        let b_result = rt.register(
            b,
            vec![result.node.pointer()],
            &mut PropagateInvalidationCollector,
        );
        let _ = rt.register(
            c,
            vec![b_result.node.pointer()],
            &mut PropagateInvalidationCollector,
        );

        // Update a should invalidate both b and c
        let a_new = rt
            .register(a, vec![], &mut PropagateInvalidationCollector)
            .node;
        let b_node = rt.get(b).unwrap();
        let c_node = rt.get(c).unwrap();
        assert!(b_node.is_invalidated());
        assert!(c_node.is_invalidated());
        assert_eq!(
            b_node.invalidations,
            Invalidations::from_iter([Invalidation::new_source(
                a_new.revision_pointer(),
                InvalidationReason::NewVersion
            )])
        );
        assert_eq!(
            c_node.invalidations,
            Invalidations::from_iter([Invalidation {
                source: a_new.revision_pointer(),
                dependency: b_node.revision_pointer(),
                reason: InvalidationReason::DependencyInvalidated,
            }])
        );
    }

    // Test cycle detection
    #[test]
    fn test_cycle_detection() {
        let rt = Runtime::new();
        let a = QueryId(1);
        let b = QueryId(2);

        let a_result = rt.register(a, vec![], &mut PropagateInvalidationCollector);
        let b_result = rt.register(
            b,
            vec![a_result.node.pointer()],
            &mut PropagateInvalidationCollector,
        );
        let _ = rt
            .register(
                a,
                vec![b_result.node.pointer()],
                &mut PropagateInvalidationCollector,
            )
            .node;

        assert!(rt.has_cycle(a));
        assert!(rt.has_cycle(b));
    }

    // loop invalidation in b, c, and d, but a triggers the invalidation.
    #[test]
    fn test_cyclic_invalidation_from_outer_source() {
        let rt = Runtime::new();
        let a = QueryId(1);
        let b = QueryId(2);
        let c = QueryId(3);
        let d = QueryId(4);

        let a_result = rt.register(a, vec![], &mut PropagateInvalidationCollector);
        let b_result = rt.register(
            b,
            vec![a_result.node.pointer()],
            &mut PropagateInvalidationCollector,
        );
        let c_result = rt.register(
            c,
            vec![b_result.node.pointer()],
            &mut PropagateInvalidationCollector,
        );
        let d_result = rt.register(
            d,
            vec![c_result.node.pointer()],
            &mut PropagateInvalidationCollector,
        );
        let b_result = rt.register(
            b,
            vec![a_result.node.pointer(), d_result.node.pointer()],
            &mut PropagateInvalidationCollector,
        );

        let a_new = rt
            .register(a, vec![], &mut PropagateInvalidationCollector)
            .node;

        let b_node = rt.get(b).unwrap();
        let c_node = rt.get(c).unwrap();
        let d_node = rt.get(d).unwrap();
        assert!(b_node.is_invalidated());
        assert!(c_node.is_invalidated());
        assert!(d_node.is_invalidated());
        assert_eq!(
            b_node.invalidations,
            Invalidations::from_iter([Invalidation::new_source(
                a_new.revision_pointer(),
                InvalidationReason::NewVersion,
            )])
        );
        assert_eq!(
            c_node.invalidations,
            Invalidations::from_iter([Invalidation::new_source(
                b_result.node.revision_pointer(),
                InvalidationReason::NewVersion,
            )])
        );
        assert_eq!(
            d_node.invalidations,
            Invalidations::from_iter([Invalidation {
                source: b_result.node.revision_pointer(),
                dependency: c_node.revision_pointer(),
                reason: InvalidationReason::DependencyInvalidated,
            }])
        );
    }

    // Test concurrent invalidations
    #[test]
    fn test_multiple_invalidation_paths() {
        let rt = Runtime::new();
        let a = QueryId(1);
        let b = QueryId(2);
        let c = QueryId(3);

        let a_result = rt.register(a, vec![], &mut PropagateInvalidationCollector);
        let b_result = rt.register(
            b,
            vec![a_result.node.pointer()],
            &mut PropagateInvalidationCollector,
        );
        let _ = rt.register(
            c,
            vec![a_result.node.pointer(), b_result.node.pointer()],
            &mut PropagateInvalidationCollector,
        );

        let a_new = rt
            .register(a, vec![], &mut PropagateInvalidationCollector)
            .node;
        let b_node = rt.get(b).unwrap();
        let c_node = rt.get(c).unwrap();
        assert_eq!(
            c_node.invalidations,
            Invalidations::from_iter([
                Invalidation {
                    source: a_new.revision_pointer(),
                    dependency: b_node.revision_pointer(),
                    reason: InvalidationReason::DependencyInvalidated,
                },
                Invalidation {
                    source: a_new.revision_pointer(),
                    dependency: a_new.revision_pointer(),
                    reason: InvalidationReason::NewVersion,
                },
            ])
        );
    }

    #[tokio::test]
    async fn test_concurrent_updates() {
        let rt = Arc::new(Runtime::new());
        let id = QueryId(1);

        let handles: Vec<_> = (0..100)
            .map(|_| {
                let rt = rt.clone();
                tokio::spawn(async move {
                    let _ = rt.register(id, vec![], &mut PropagateInvalidationCollector);
                })
            })
            .collect();

        for handle in handles {
            handle.await.unwrap();
        }

        let node = rt.get(id).unwrap();
        assert!(node.version.0 == 99); // Changed from exact equality to >= since we might have retries
    }

    // Test garbage collection of unused nodes
    #[test]
    fn test_remove_if_unused() {
        let rt = Runtime::new();
        let a = QueryId(1);
        let b = QueryId(2);

        // a with no dependencies
        let _ = rt.register(a, vec![], &mut PropagateInvalidationCollector);
        assert!(rt.remove_if_unused(a).is_some());

        // b depends on a
        let QueryRegistrationResult { node, .. } =
            rt.register(a, vec![], &mut PropagateInvalidationCollector);
        let _ = rt.register(b, vec![node.pointer()], &mut PropagateInvalidationCollector);
        assert!(rt.remove_if_unused(a).is_none());
    }

    // Test dependency version updates
    #[test]
    fn test_dependency_version_update() {
        let rt = Runtime::new();
        let a = QueryId(1);
        let b = QueryId(2);

        // Initial version
        let a1 = rt
            .register(a, vec![], &mut PropagateInvalidationCollector)
            .node;
        let _b1 = rt
            .register(b, vec![a1.pointer()], &mut PropagateInvalidationCollector)
            .node;

        // Update a
        let a2 = rt
            .register(a, vec![], &mut PropagateInvalidationCollector)
            .node;

        // Update b's dependencies
        let b2 = rt
            .update_dependencies(b, vec![a2.pointer()], &mut PropagateInvalidationCollector)
            .node;
        assert_eq!(
            b2.dependencies.iter().next().unwrap().version.0,
            a2.version.0
        );
    }

    // Test complex dependency graph (diamond shape)
    #[test]
    fn test_diamond_dependency() {
        let rt = Runtime::new();
        let a = QueryId(1);
        let b = QueryId(2);
        let c = QueryId(3);
        let d = QueryId(4);

        let a_result = rt.register(a, vec![], &mut PropagateInvalidationCollector);
        let b_result = rt.register(
            b,
            vec![a_result.node.pointer()],
            &mut PropagateInvalidationCollector,
        );
        let c_result = rt.register(
            c,
            vec![a_result.node.pointer()],
            &mut PropagateInvalidationCollector,
        );
        let _ = rt.register(
            d,
            vec![b_result.node.pointer(), c_result.node.pointer()],
            &mut PropagateInvalidationCollector,
        );

        // Update a should invalidate all
        let a_new = rt
            .register(a, vec![], &mut PropagateInvalidationCollector)
            .node;
        let b_node = rt.get(b).unwrap();
        let c_node = rt.get(c).unwrap();
        assert!(b_node.is_invalidated());
        assert!(c_node.is_invalidated());
        assert_eq!(
            rt.get(d).unwrap().invalidations,
            Invalidations::from_iter([
                Invalidation {
                    source: a_new.revision_pointer(),
                    dependency: b_node.revision_pointer(),
                    reason: InvalidationReason::DependencyInvalidated,
                },
                Invalidation {
                    source: a_new.revision_pointer(),
                    dependency: c_node.revision_pointer(),
                    reason: InvalidationReason::DependencyInvalidated,
                },
            ])
        );
    }

    // Test un-invalidation process
    #[test]
    fn test_uninvalidation() {
        let rt = Runtime::new();
        let a = QueryId(1);
        let b = QueryId(2);

        // Create dependency
        let a_node = rt
            .register(a, vec![], &mut PropagateInvalidationCollector)
            .node;
        let _b_node = rt
            .register(
                b,
                vec![a_node.pointer()],
                &mut PropagateInvalidationCollector,
            )
            .node;

        // Invalidate b by updating a and get the new revision pointer
        let a_new = rt
            .register(a, vec![], &mut PropagateInvalidationCollector)
            .node;
        let b_node = rt.get(b).unwrap();
        assert!(b_node.is_invalidated());

        // Remove the invalidator from b's invalidations using the source pointer
        rt.remove_invalidator(
            b_node.pointer(),
            a_new.revision_pointer(),
            &mut PropagateInvalidationCollector,
            &mut PropagateUninvalidationCollector,
        );
        assert!(!rt.get(b).unwrap().is_invalidated());
    }

    #[test]
    fn test_uninvalidation_reinvalidates_since_invalidator_has_new_version() {
        let rt = Runtime::new();
        let a = QueryId(1);
        let b = QueryId(2);
        let other = QueryId(3);

        let a_result = rt.register(a, vec![], &mut PropagateInvalidationCollector);
        let other_result = rt.register(other, vec![], &mut PropagateInvalidationCollector);
        let _b_result = rt.register(
            b,
            vec![a_result.node.pointer(), other_result.node.pointer()],
            &mut PropagateInvalidationCollector,
        );

        let a_new1 = rt
            .register(a, vec![], &mut PropagateInvalidationCollector)
            .node;
        let a_new2 = rt
            .register(a, vec![], &mut PropagateInvalidationCollector)
            .node;
        let other_new = rt
            .register(other, vec![], &mut PropagateInvalidationCollector)
            .node;

        let b_node = rt.get(b).unwrap();
        assert!(b_node.is_invalidated());
        assert_eq!(
            b_node.invalidations,
            Invalidations::from_iter([
                Invalidation::new_source(a_new1.revision_pointer(), InvalidationReason::NewVersion,),
                Invalidation::new_source(
                    other_new.revision_pointer(),
                    InvalidationReason::NewVersion
                )
            ]),
        );
        rt.remove_invalidator(
            b_node.pointer(),
            a_new1.revision_pointer(),
            &mut PropagateInvalidationCollector,
            &mut PropagateUninvalidationCollector,
        );
        assert!(rt.get(b).unwrap().is_invalidated());
        assert_eq!(
            rt.get(b).unwrap().invalidations,
            Invalidations::from_iter([
                Invalidation::new_source(
                    other_new.revision_pointer(),
                    InvalidationReason::NewVersion
                ),
                Invalidation::new_source(a_new2.revision_pointer(), InvalidationReason::NewVersion),
            ])
        );
    }

    // Test concurrent invalidations and updates
    #[tokio::test]
    async fn test_concurrent_invalidations() {
        let rt = Arc::new(Runtime::new());
        let a = QueryId(1);
        let b = QueryId(2);

        let a_node = rt
            .register(a, vec![], &mut PropagateInvalidationCollector)
            .node;
        let _ = rt.register(
            b,
            vec![a_node.pointer()],
            &mut PropagateInvalidationCollector,
        );
        let a_new = rt
            .register(a, vec![], &mut PropagateInvalidationCollector)
            .node;

        let handles: Vec<_> = (0..50)
            .map(|_| {
                let rt = rt.clone();
                tokio::spawn(async move {
                    let _ = rt.register(a, vec![], &mut PropagateInvalidationCollector);
                })
            })
            .collect();

        for handle in handles {
            handle.await.unwrap();
        }

        let b_node = rt.get(b).unwrap();
        assert!(b_node.is_invalidated());
        // a invalidated 50 times, but only the first one is registered. because new version does not have a dependent of b. It's fine because removing this invalidator makes b invalidated again because a is not the latest.
        assert_eq!(
            b_node.invalidations,
            Invalidations::from_iter([Invalidation {
                source: a_new.revision_pointer(),
                dependency: a_new.revision_pointer(),
                reason: InvalidationReason::NewVersion,
            }])
        );
    }

    // Test node removal with dependents
    #[test]
    fn test_removal_with_dependents() {
        let rt = Runtime::new();
        let a = QueryId(1);
        let b = QueryId(2);

        let a_result = rt.register(a, vec![], &mut PropagateInvalidationCollector);
        let _b_result = rt.register(
            b,
            vec![a_result.node.pointer()],
            &mut PropagateInvalidationCollector,
        );

        let removal = rt.remove(a, &mut PropagateInvalidationCollector);
        assert!(removal.removed.is_some());
        assert_eq!(
            rt.get(b).unwrap().invalidations,
            Invalidations::from_iter([Invalidation {
                source: a_result.node.revision_pointer(),
                dependency: a_result.node.revision_pointer(),
                reason: InvalidationReason::Removed,
            }])
        );
    }
}
