use serde::{Deserialize, Serialize};
use std::sync::atomic::AtomicBool;

use crate::query_id::QueryId;
use crate::version::Version;

#[derive(Serialize, Deserialize, Debug)]
pub struct Node {
    query_id: QueryId,
    version: Version,
    dependencies: Vec<QueryId>,
    dependents: Vec<QueryId>,
    invalidated: AtomicBool,
}

impl Node {
    pub fn new(query_id: QueryId, version: u64, dependencies: Vec<QueryId>) -> Self {
        Node {
            query_id,
            version: Version::new(version),
            dependencies,
            dependents: Vec::new(),
            invalidated: AtomicBool::new(false),
        }
    }
}
