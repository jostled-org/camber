use camber::RuntimeError;
use camber::http::{Request, RequestBuilder};

#[test]
fn request_builder_method_rejects_unknown() {
    let result: Result<RequestBuilder, RuntimeError> = Request::builder().method("BOGUS");
    match result {
        Err(RuntimeError::InvalidArgument(_)) => {}
        Err(other) => panic!("expected InvalidArgument, got: {other}"),
        Ok(_) => panic!("expected Err, got Ok"),
    }
}

#[test]
fn request_builder_method_accepts_valid() {
    let builder = Request::builder().method("POST").expect("POST is valid");
    let req = builder.finish().expect("finish should succeed");
    assert_eq!(req.method(), "POST");
}

#[test]
fn request_builder_finish_rejects_invalid_path() {
    let result = Request::builder().path("\0bad").finish();
    match result {
        Err(RuntimeError::InvalidArgument(_)) => {}
        Err(other) => panic!("expected InvalidArgument, got: {other}"),
        Ok(_) => panic!("expected Err, got Ok"),
    }
}

#[test]
fn request_builder_finish_rejects_invalid_header() {
    let result = Request::builder().header("\0", "value").finish();
    match result {
        Err(RuntimeError::InvalidArgument(_)) => {}
        Err(other) => panic!("expected InvalidArgument, got: {other}"),
        Ok(_) => panic!("expected Err, got Ok"),
    }
}

#[test]
fn request_builder_renamed_from_build() {
    let builder: RequestBuilder = Request::builder();
    let req = builder.finish().expect("finish should succeed");
    assert_eq!(req.method(), "GET");
    assert_eq!(req.path(), "/");
}
