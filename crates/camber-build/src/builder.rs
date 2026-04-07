use crate::codegen;
use std::io;
use std::path::{Path, PathBuf};

/// Compile `.proto` files and generate async service traits.
///
/// Runs `tonic-build` to produce standard async gRPC server code, then
/// generates an additional async trait and bridge struct per service.
/// The trait has async methods. The bridge delegates directly via `.await`.
///
/// Call this from your crate's `build.rs`.
pub fn compile_protos(
    protos: &[impl AsRef<Path>],
    includes: &[impl AsRef<Path>],
) -> io::Result<()> {
    configure().compile_protos(protos, includes)
}

/// Return a builder for customizing proto compilation.
pub fn configure() -> Builder {
    Builder {
        file_descriptor_set_path: None,
    }
}

/// Builder for customizing `.proto` compilation.
pub struct Builder {
    file_descriptor_set_path: Option<PathBuf>,
}

impl Builder {
    /// Generate a file containing the encoded `FileDescriptorSet`.
    /// Required for gRPC reflection.
    pub fn file_descriptor_set_path(mut self, path: impl AsRef<Path>) -> Self {
        self.file_descriptor_set_path = Some(path.as_ref().to_path_buf());
        self
    }

    /// Compile the `.proto` files and generate code.
    pub fn compile_protos(
        self,
        protos: &[impl AsRef<Path>],
        includes: &[impl AsRef<Path>],
    ) -> io::Result<()> {
        let tonic = tonic_build::configure()
            .build_client(true)
            .build_server(true)
            .build_transport(false);

        let mut config = prost_build::Config::new();
        configure_protoc(&mut config)?;
        if let Some(path) = &self.file_descriptor_set_path {
            config.file_descriptor_set_path(path);
        }
        config.service_generator(Box::new(codegen::ServiceGenerator::new(tonic)));
        let include_paths = include_paths(includes)?;
        config.compile_protos(protos, &include_paths)
    }
}

fn configure_protoc(config: &mut prost_build::Config) -> io::Result<()> {
    let protoc = protoc_bin_vendored::protoc_bin_path().map_err(other_io_error)?;
    config.protoc_executable(protoc);
    Ok(())
}

fn include_paths(includes: &[impl AsRef<Path>]) -> io::Result<Vec<PathBuf>> {
    let mut include_paths = includes
        .iter()
        .map(|path| path.as_ref().to_path_buf())
        .collect::<Vec<_>>();
    let vendored_include = protoc_bin_vendored::include_path().map_err(other_io_error)?;
    if !include_paths.iter().any(|path| path == &vendored_include) {
        include_paths.push(vendored_include);
    }
    Ok(include_paths)
}

fn other_io_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}
