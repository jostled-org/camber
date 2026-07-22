#[renamed_camber::test]
async fn renamed_runtime_dependency_executes_generated_test() {
    let handle = renamed_camber::spawn_async(async { 42 });
    assert!(matches!(handle.await, Ok(42)));
}

#[renamed_macros::test]
async fn renamed_proc_macro_dependency_executes_generated_test() {
    let value = std::future::ready(42).await;
    assert_eq!(value, 42);
}

mod shadowed_scope {
    mod camber {}
    mod renamed_camber {}

    #[renamed_macros::test]
    async fn lexical_names_do_not_capture_generated_runtime_path() {
        assert_eq!(std::future::ready(42).await, 42);
    }
}

#[renamed_camber::test]
#[should_panic(expected = "preserved panic")]
async fn preserves_should_panic_attribute() {
    panic!("preserved panic");
}

#[renamed_camber::test]
#[ignore = "attribute preservation fixture"]
async fn preserves_ignore_attribute() {
    panic!("ignored test executed");
}
