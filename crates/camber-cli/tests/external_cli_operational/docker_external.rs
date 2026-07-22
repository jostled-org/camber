use crate::docker_support;
use crate::support::FixtureError;

#[test]
#[ignore = "external lane docker; owner: Camber CLI maintainers; run: gh workflow run external-evidence.yml -f lane=docker"]
fn dockerfile_builds_successfully() -> Result<(), FixtureError> {
    docker_support::run_dockerfile_build()
}
