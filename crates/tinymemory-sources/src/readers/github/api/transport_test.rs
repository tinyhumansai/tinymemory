//! Task-local deterministic GitHub transport used by reader tests.

tokio::task_local! {
    static TEST_RESPONSES: std::cell::RefCell<
        std::collections::VecDeque<Result<String, String>>
    >;
}

/// Run a future with a task-local sequence of GitHub responses.
pub(crate) async fn with_test_responses<F>(
    responses: Vec<Result<String, String>>,
    future: F,
) -> F::Output
where
    F: std::future::Future,
{
    TEST_RESPONSES
        .scope(std::cell::RefCell::new(responses.into()), future)
        .await
}

/// Return the next deterministic response, or no override outside its scope.
pub(crate) fn take_response(api_path: &str) -> Option<Result<String, String>> {
    TEST_RESPONSES
        .try_with(|responses| {
            Some(responses.borrow_mut().pop_front().unwrap_or_else(|| {
                Err(format!(
                    "no deterministic GitHub response queued for {api_path}"
                ))
            }))
        })
        .ok()
        .flatten()
}
