//! Step 0 feasibility smoke test: git2 (vendored libgit2, no openssl/ssh)
//! can initialize a repository and commit a file entirely locally.

use std::fs;

use git2::Repository;

#[test]
fn git2_initializes_and_commits_locally() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    {
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "tiller-smoke").unwrap();
        cfg.set_str("user.email", "tiller@local").unwrap();
    }

    fs::write(dir.path().join("hello.txt"), "hello tiller").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("hello.txt")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = repo.signature().unwrap();
    let head = repo
        .commit(Some("HEAD"), &sig, &sig, "smoke", &tree, &[])
        .unwrap();

    assert_eq!(repo.head().unwrap().target(), Some(head));
}
