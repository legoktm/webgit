//! Reader for git's commit-graph file (`objects/info/commit-graph`).
//!
//! The reader lives in the `gib-commitgraph` crate and is re-exported here;
//! [`Repo::commit_graph`](crate::Repo::commit_graph) hands out the one a
//! repository was opened with.

pub use gib_commitgraph::{CommitGraph, CommitGraphEntry, bloom};

#[cfg(test)]
mod tests {
    use crate::test::open_test_repo;
    use gib_testkit::make_basic_repo;

    /// Opening a repository loads its commit-graph, which is the wiring the
    /// `gib-commitgraph` tests deliberately don't know about.
    #[test]
    fn repo_open_loads_the_graph() {
        let test_repo = make_basic_repo().unwrap();
        assert!(open_test_repo(&test_repo).commit_graph().is_none());
        test_repo
            .run_git(["commit-graph", "write", "--reachable"])
            .unwrap();
        assert!(open_test_repo(&test_repo).commit_graph().is_some());
    }
}
