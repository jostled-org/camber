pub(crate) fn block_on<F: std::future::Future>(
    future: F,
) -> Result<F::Output, crate::RuntimeError> {
    let handle = tokio::runtime::Handle::try_current().map_err(|error| {
        crate::RuntimeError::MessageQueue(
            format!("sync message-queue API requires a Tokio multi-thread runtime: {error}").into(),
        )
    })?;
    match handle.runtime_flavor() {
        tokio::runtime::RuntimeFlavor::MultiThread => {
            Ok(tokio::task::block_in_place(|| handle.block_on(future)))
        }
        _ => Err(crate::RuntimeError::MessageQueue(
            "sync message-queue API requires a Tokio multi-thread runtime".into(),
        )),
    }
}
