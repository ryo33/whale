use std::sync::atomic::AtomicU64;

use crate::{node::Node, query_id::QueryId};

pub struct Graph {
    nodes: ConcurrentMap<QueryId, AtomicArc<Node>>,
    global_version: AtomicU64,
}
