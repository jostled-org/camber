/// Generates both standard tonic async code and camber async wrappers.
pub(crate) struct ServiceGenerator {
    tonic_gen: Box<dyn prost_build::ServiceGenerator>,
    service_code: String,
}

impl ServiceGenerator {
    pub(crate) fn new(tonic_builder: tonic_prost_build::Builder) -> Self {
        Self {
            tonic_gen: tonic_builder.service_generator(),
            service_code: String::new(),
        }
    }
}

impl prost_build::ServiceGenerator for ServiceGenerator {
    fn generate(&mut self, service: prost_build::Service, buf: &mut String) {
        self.tonic_gen.generate(service.clone(), buf);
        generate_service_wrapper(&service, &mut self.service_code);
    }

    fn finalize(&mut self, buf: &mut String) {
        self.tonic_gen.finalize(buf);

        if !self.service_code.is_empty() {
            buf.push_str(&self.service_code);
            self.service_code.clear();
        }
    }

    fn finalize_package(&mut self, package: &str, buf: &mut String) {
        self.tonic_gen.finalize_package(package, buf);
    }
}

fn snake_case(name: &str) -> String {
    let mut s = String::new();
    let mut it = name.chars().peekable();
    while let Some(c) = it.next() {
        s.push(c.to_ascii_lowercase());
        if next_starts_new_word(&mut it) {
            s.push('_');
        }
    }
    s
}

fn next_starts_new_word(iter: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    iter.peek().is_some_and(|next| next.is_uppercase())
}

/// Build the function signature for a unary RPC method.
///
/// Returns `None` for a streaming method. The async wrapper has one shape —
/// take a request, answer with a response — and a streaming method has no
/// spelling in it, so the wrapper covers the unary services and stands aside
/// for the rest. See [`generate_service_wrapper`].
fn unary_signature(method: &prost_build::Method) -> Option<String> {
    match (method.client_streaming, method.server_streaming) {
        (false, false) => {
            let name = &method.name;
            let input = &method.input_type;
            let output = &method.output_type;
            Some(format!(
                "async fn {name}(&self, request: tonic::Request<{input}>) -> \
                 std::result::Result<tonic::Response<{output}>, tonic::Status>"
            ))
        }
        _ => None,
    }
}

fn emit_trait_method(method: &prost_build::Method, buf: &mut String) {
    if let Some(sig) = unary_signature(method) {
        buf.push_str(&format!("        {sig};\n"));
    }
}

fn emit_bridge_method(method: &prost_build::Method, buf: &mut String) {
    if let Some(sig) = unary_signature(method) {
        let method_name = &method.name;
        buf.push_str(&format!("        {sig} {{\n"));
        buf.push_str(&format!(
            "            self.0.{method_name}(request).await\n"
        ));
        buf.push_str("        }\n");
    }
}

/// The methods on one service the async wrapper has no shape for.
fn streaming_methods(service: &prost_build::Service) -> Box<[&str]> {
    service
        .methods
        .iter()
        .filter(|method| unary_signature(method).is_none())
        .map(|method| method.name.as_str())
        .collect()
}

/// Emit the empty, documented module that stands in for a skipped wrapper.
///
/// A skipped service used to emit `compile_error!` into its wrapper instead,
/// which failed the whole generated module — every other service in the proto
/// with it, and every caller that only ever wanted tonic's own server. Nothing
/// replaces a diagnostic the compiler prints, so the reason is left where the
/// wrapper would have been: the module still exists under the name a caller
/// reaches for, reaching into it names the item that is missing rather than the
/// module, and its documentation states which method ruled the wrapper out and
/// which tonic trait to implement instead.
///
/// A plain comment would not survive: `prost-build` formats its output through
/// `prettyplease`, which parses the file and keeps only documentation.
fn note_absent_wrapper(service: &prost_build::Service, streaming: &[&str], buf: &mut String) {
    let trait_name = &service.name;
    let base = snake_case(trait_name);
    buf.push_str(&format!(
        "/// No async wrapper for the {trait_name} service.\n"
    ));
    buf.push_str("///\n");
    buf.push_str("/// The wrapper takes one request and answers with one response.\n");
    buf.push_str(&format!(
        "/// Streaming method(s) `{}` have no shape in it.\n",
        streaming.join("`, `")
    ));
    buf.push_str(&format!(
        "/// Implement `{base}_server::{trait_name}` directly.\n"
    ));
    buf.push_str(&format!("pub mod {base}_service {{}}\n"));
}

/// Emit the async wrapper for one service, or the note saying why it has none.
fn generate_service_wrapper(service: &prost_build::Service, buf: &mut String) {
    let streaming = streaming_methods(service);
    if !streaming.is_empty() {
        note_absent_wrapper(service, &streaming, buf);
        return;
    }
    let trait_name = &service.name;
    let base = snake_case(trait_name);
    let mod_name = format!("{base}_service");
    let server_type = format!("{}Server", trait_name);
    let server_mod = format!("{base}_server");

    buf.push_str(&format!(
        "/// Async wrappers for the {trait_name} service.\n"
    ));
    buf.push_str(&format!("pub mod {mod_name} {{\n"));
    buf.push_str("    use super::*;\n\n");

    // Generate async trait
    buf.push_str(&format!(
        "    /// Async `{trait_name}` gRPC service trait.\n"
    ));
    buf.push_str("    /// Implement this instead of the tonic-generated version.\n");
    buf.push_str("    #[tonic::async_trait]\n");
    buf.push_str(&format!(
        "    pub trait {trait_name}: Send + Sync + 'static {{\n"
    ));

    for method in &service.methods {
        emit_trait_method(method, buf);
    }
    buf.push_str("    }\n\n");

    // Generate bridge struct
    buf.push_str(&format!(
        "    /// Bridges an async `{trait_name}` impl to the tonic service trait.\n"
    ));
    buf.push_str(&format!(
        "    pub struct Bridge<T: {trait_name}>(pub T);\n\n"
    ));

    // Implement the tonic async trait via the bridge
    buf.push_str("    #[tonic::async_trait]\n");
    buf.push_str(&format!(
        "    impl<T: {trait_name}> {server_mod}::{trait_name} for Bridge<T> {{\n"
    ));

    for method in &service.methods {
        emit_bridge_method(method, buf);
    }
    buf.push_str("    }\n\n");

    // Helper function to create a tonic server from an async impl
    buf.push_str(&format!(
        "    /// Create a tonic `{server_type}` from an async service implementation.\n"
    ));
    buf.push_str(&format!(
        "    pub fn serve<T: {trait_name}>(service: T) -> {server_mod}::{server_type}<Bridge<T>> {{\n"
    ));
    buf.push_str(&format!(
        "        {server_mod}::{server_type}::new(Bridge(service))\n"
    ));
    buf.push_str("    }\n");

    buf.push_str("}\n");
}
