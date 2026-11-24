use std::path::{Path, PathBuf};

use nsv_files::resolve;

#[test]
fn resolve_never_leaves_the_root_lexically_unnoticed() {
    let root = Path::new("/srv/client");
    let at = |path: &str| PathBuf::from(path);
    assert_eq!(resolve(root, ""), at("/srv/client"));
    assert_eq!(resolve(root, "a/./b"), at("/srv/client/a/b"));
    assert_eq!(resolve(root, "a//b/"), at("/srv/client/a/b"));
    assert_eq!(resolve(root, "./a/../b"), at("/srv/client/b"));
    assert_eq!(resolve(root, "../secret.txt"), at("/srv/secret.txt"));
    assert_eq!(
        resolve(root, "../client-evil.txt"),
        at("/srv/client-evil.txt")
    );
    assert!(!resolve(root, "../client-evil.txt").starts_with(root));
    // `..` at the root stays there.
    assert_eq!(resolve(root, "../../../../etc/passwd"), at("/etc/passwd"));
    assert_eq!(resolve(Path::new("/"), ".."), at("/"));
    // An absolute path replaces the base, as in Node: the asset root trims the request's
    // leading `/` before resolving, and its containment check refuses the rest.
    assert_eq!(resolve(root, "/etc/passwd"), at("/etc/passwd"));
    assert!(!resolve(root, "/srv/client/../client-evil.txt").starts_with(root));
}
