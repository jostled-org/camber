#[path = "process_bench_smoke/support.rs"]
pub mod support;

#[path = "process_bench_smoke/go_cleanup_contract.rs"]
mod external_go;
#[path = "process_bench_smoke/fixtures.rs"]
mod fixtures;
#[path = "external_bench_tools/go.rs"]
pub mod go_support;
#[path = "external_bench_tools/resources.rs"]
pub mod resources;
#[path = "process_bench_smoke/servers.rs"]
mod servers;
