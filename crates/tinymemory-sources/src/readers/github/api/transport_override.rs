//! Production transport override: live GitHub requests are never intercepted.

pub(super) fn take_response(_api_path: &str) -> Option<Result<String, String>> {
    None
}
