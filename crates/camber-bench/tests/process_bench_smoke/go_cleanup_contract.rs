use crate::go_support::GoBuild;
use crate::support::FixtureError;

#[test]
fn go_build_drop_removes_unique_tree_after_unwind() -> Result<(), FixtureError> {
    let build = GoBuild::create("camber-go-bench-unwind-proof")?;
    let other_build = GoBuild::create("camber-go-bench-unwind-proof")?;
    let root = build.root().to_path_buf();
    let other_root = other_build.root().to_path_buf();
    assert!(root.is_dir(), "Go build tree should exist before unwind");
    assert_ne!(root, other_root, "Go builds must own distinct trees");

    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _build = build;
        let _other_build = other_build;
        // Exercise the same stack unwinding path as a failed assertion.
        std::panic::resume_unwind(Box::new("exercise assertion unwind cleanup"));
    }));

    assert!(unwind.is_err(), "fixture should unwind");
    assert!(!root.exists(), "Go build tree survived unwind: {root:?}");
    assert!(
        !other_root.exists(),
        "second Go build tree survived unwind: {other_root:?}"
    );
    Ok(())
}
