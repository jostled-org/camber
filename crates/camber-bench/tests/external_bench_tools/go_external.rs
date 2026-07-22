use crate::go_support;
use crate::support::FixtureError;

#[test]
#[ignore = "external lane go; owner: Camber benchmark maintainers; run: gh workflow run external-evidence.yml -f lane=go"]
fn go_server_responds_if_go_available() -> Result<(), FixtureError> {
    go_support::run_go_server_response_check()
}
