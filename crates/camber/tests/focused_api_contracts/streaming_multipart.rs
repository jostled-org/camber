use camber::RuntimeError;
use camber::http::mock::MultipartSession;
use camber::http::{MultipartField, MultipartLimits, MultipartStream};

/// The documented defaults, restated here so a change to either side is a
/// failure rather than a silent agreement.
const DEFAULT_FIELDS: usize = 128;
const DEFAULT_FIELD_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_HEADERS: usize = 32;
const DEFAULT_HEADER_BYTES: usize = 16 * 1024;
const DEFAULT_BOUNDARY_BYTES: usize = 70;
const DEFAULT_CHUNK_BYTES: usize = 64 * 1024;
const DEFAULT_PARSER_BUFFER_BYTES: usize = 128 * 1024;

/// Every setter, named once, so a row can narrow exactly one value.
type Setter = fn(usize) -> Result<MultipartLimits, RuntimeError>;

fn with_fields(value: usize) -> Result<MultipartLimits, RuntimeError> {
    MultipartLimits::builder().max_fields(value).build()
}

fn with_field_bytes(value: usize) -> Result<MultipartLimits, RuntimeError> {
    MultipartLimits::builder().max_field_bytes(value).build()
}

fn with_headers(value: usize) -> Result<MultipartLimits, RuntimeError> {
    MultipartLimits::builder()
        .max_headers_per_field(value)
        .build()
}

fn with_header_bytes(value: usize) -> Result<MultipartLimits, RuntimeError> {
    MultipartLimits::builder()
        .max_header_bytes_per_field(value)
        .build()
}

fn with_boundary_bytes(value: usize) -> Result<MultipartLimits, RuntimeError> {
    MultipartLimits::builder().max_boundary_bytes(value).build()
}

fn with_chunk_bytes(value: usize) -> Result<MultipartLimits, RuntimeError> {
    MultipartLimits::builder().max_chunk_bytes(value).build()
}

fn with_parser_buffer(value: usize) -> Result<MultipartLimits, RuntimeError> {
    MultipartLimits::builder()
        .max_parser_buffer_bytes(value)
        .build()
}

const SETTERS: [(&str, Setter); 7] = [
    ("max_fields", with_fields),
    ("max_field_bytes", with_field_bytes),
    ("max_headers_per_field", with_headers),
    ("max_header_bytes_per_field", with_header_bytes),
    ("max_boundary_bytes", with_boundary_bytes),
    ("max_chunk_bytes", with_chunk_bytes),
    ("max_parser_buffer_bytes", with_parser_buffer),
];

/// Require one refusal to be the exact configuration category.
fn assert_invalid_argument(label: &str, result: Result<MultipartLimits, RuntimeError>) {
    match result {
        Err(RuntimeError::InvalidArgument(_)) => {}
        Err(other) => panic!("{label} must refuse as InvalidArgument, got {other:?}"),
        Ok(_) => panic!("{label} must be refused"),
    }
}

#[test]
fn multipart_limits_defaults_and_checked_builder_contract() {
    documented_defaults_are_the_builder_start();
    every_consuming_setter_composes();
    zero_values_and_an_over_protocol_boundary_are_refused();
    the_parser_buffer_holds_the_peak_the_other_limits_require();
}

/// Every documented default, and the builder that starts from them.
fn documented_defaults_are_the_builder_start() {
    let defaults = MultipartLimits::default();
    assert_eq!(defaults.max_fields(), DEFAULT_FIELDS);
    assert_eq!(defaults.max_field_bytes(), DEFAULT_FIELD_BYTES);
    assert_eq!(defaults.max_headers_per_field(), DEFAULT_HEADERS);
    assert_eq!(defaults.max_header_bytes_per_field(), DEFAULT_HEADER_BYTES);
    assert_eq!(defaults.max_boundary_bytes(), DEFAULT_BOUNDARY_BYTES);
    assert_eq!(defaults.max_chunk_bytes(), DEFAULT_CHUNK_BYTES);
    assert_eq!(
        defaults.max_parser_buffer_bytes(),
        DEFAULT_PARSER_BUFFER_BYTES
    );
    assert_eq!(
        defaults.max_reply_bytes(),
        DEFAULT_CHUNK_BYTES.max(DEFAULT_HEADER_BYTES),
        "the reply bound is the larger of one chunk and one header block"
    );

    let built = MultipartLimits::builder()
        .build()
        .expect("the defaults are a valid combination");
    assert_eq!(built, defaults, "the builder starts at the defaults");
}

/// Every consuming setter narrows exactly its own value.
fn every_consuming_setter_composes() {
    let narrowed = MultipartLimits::builder()
        .max_fields(4)
        .max_field_bytes(1024)
        .max_headers_per_field(2)
        .max_header_bytes_per_field(256)
        .max_boundary_bytes(16)
        .max_chunk_bytes(64)
        .max_parser_buffer_bytes(512)
        .build()
        .expect("every consuming setter composes into one valid combination");
    assert_eq!(narrowed.max_fields(), 4);
    assert_eq!(narrowed.max_field_bytes(), 1024);
    assert_eq!(narrowed.max_headers_per_field(), 2);
    assert_eq!(narrowed.max_header_bytes_per_field(), 256);
    assert_eq!(narrowed.max_boundary_bytes(), 16);
    assert_eq!(narrowed.max_chunk_bytes(), 64);
    assert_eq!(narrowed.max_parser_buffer_bytes(), 512);
    assert_eq!(narrowed.max_reply_bytes(), 256);
}

/// A zero value and a boundary above the protocol maximum are configuration
/// errors, and so is a peak that overflows.
fn zero_values_and_an_over_protocol_boundary_are_refused() {
    for (label, setter) in SETTERS {
        assert_invalid_argument(&format!("{label} of zero"), setter(0));
    }

    assert_invalid_argument(
        "a boundary above the protocol maximum",
        with_boundary_bytes(71),
    );
    with_boundary_bytes(70).expect("the protocol maximum itself is admitted");

    assert_invalid_argument(
        "a header peak that overflows",
        MultipartLimits::builder()
            .max_header_bytes_per_field(usize::MAX)
            .max_parser_buffer_bytes(usize::MAX)
            .build(),
    );
    assert_invalid_argument(
        "a chunk peak that overflows",
        MultipartLimits::builder()
            .max_chunk_bytes(usize::MAX)
            .max_parser_buffer_bytes(usize::MAX)
            .build(),
    );
}

/// The required parser buffer is `max(2 * header, chunk + boundary + 5)`, at the
/// boundary in both directions.
fn the_parser_buffer_holds_the_peak_the_other_limits_require() {
    // header_peak = 2 * header, and it decides the requirement here.
    let header_bound = MultipartLimits::builder()
        .max_header_bytes_per_field(256)
        .max_chunk_bytes(16)
        .max_boundary_bytes(8);
    assert_invalid_argument(
        "a parser buffer one byte below the header peak",
        header_bound.max_parser_buffer_bytes(511).build(),
    );
    header_bound
        .max_parser_buffer_bytes(512)
        .build()
        .expect("a parser buffer exactly at the header peak is admitted");

    // chunk_peak = chunk + boundary + 5, and it decides the requirement here.
    let chunk_bound = MultipartLimits::builder()
        .max_header_bytes_per_field(8)
        .max_chunk_bytes(1024)
        .max_boundary_bytes(70);
    assert_invalid_argument(
        "a parser buffer one byte below the chunk peak",
        chunk_bound.max_parser_buffer_bytes(1098).build(),
    );
    chunk_bound
        .max_parser_buffer_bytes(1099)
        .build()
        .expect("a parser buffer exactly at the chunk peak is admitted");
}

#[test]
fn multipart_stream_and_admission_values_are_not_cloneable() {
    // Two implementations, one blanket and one constrained to `Clone`. The
    // compiler resolves the marker only while no `Clone` implementation exists;
    // adding one makes the call ambiguous and this file stops compiling.
    trait AmbiguousIfClone<Witness> {
        fn owns_one_handle() -> bool {
            true
        }
    }
    impl<T: ?Sized> AmbiguousIfClone<()> for T {}
    // No `?Sized` on this one: `Clone` requires `Sized`, so relaxing it is
    // ignored, and the constrained implementation still covers every type the
    // unconstrained one resolves for.
    impl<T: Clone> AmbiguousIfClone<u8> for T {}

    assert!(
        <MultipartStream as AmbiguousIfClone<_>>::owns_one_handle(),
        "the access handle resolves the unconstrained marker, so it is not Clone"
    );
    assert!(
        <MultipartField<'static> as AmbiguousIfClone<_>>::owns_one_handle(),
        "one active field resolves the unconstrained marker, so it is not Clone"
    );
    // The admitted session owner is `MultipartSessionDriver`, which is private
    // to `crate::http` and cannot be named here. Its witness resolves the same
    // marker over the production instantiation inside the crate, so giving the
    // driver a `Clone` implementation stops `camber` compiling.
    assert!(
        camber::http::mock::multipart_session_owner_is_not_cloneable(),
        "the admitted session owner resolves the unconstrained marker, so it is not Clone"
    );
    assert!(
        <MultipartSession as AmbiguousIfClone<_>>::owns_one_handle(),
        "the controlled session that holds the driver is not Clone either"
    );

    fn requires_send<T: Send>() {}
    requires_send::<MultipartStream>();

    // Metadata is returned by borrow from the field that owns it: these compile
    // only while the accessors tie their result to `&self`.
    fn borrowed_name<'a>(field: &'a MultipartField<'_>) -> &'a str {
        field.name()
    }
    fn borrowed_filename<'a>(field: &'a MultipartField<'_>) -> Option<&'a str> {
        field.filename()
    }
    fn borrowed_content_type<'a>(field: &'a MultipartField<'_>) -> Option<&'a str> {
        field.content_type()
    }

    let _borrowing_accessors = (borrowed_name, borrowed_filename, borrowed_content_type);
}
