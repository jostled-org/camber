use camber::RuntimeError;
use camber::http::Response;
use serde::Serialize;

#[test]
fn response_text_rejects_invalid_status() {
    assert!(matches!(
        Response::text(0, "bad"),
        Err(RuntimeError::InvalidArgument(_))
    ));

    assert!(matches!(
        Response::text(600, "bad"),
        Err(RuntimeError::InvalidArgument(_))
    ));

    let resp = Response::text(200, "ok").expect("valid status");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "ok");
}

#[test]
fn response_json_rejects_invalid_status() {
    let data = serde_json::json!({"key": "value"});

    assert!(matches!(
        Response::json(99, &data),
        Err(RuntimeError::InvalidArgument(_))
    ));

    assert!(matches!(
        Response::json(600, &data),
        Err(RuntimeError::InvalidArgument(_))
    ));

    let resp = Response::json(200, &data).expect("valid status");
    assert_eq!(resp.status(), 200);
    assert!(resp.body().contains("key"));
}

#[test]
fn response_json_propagates_serialization_error() {
    /// A type whose Serialize impl always fails.
    struct BadValue;
    impl Serialize for BadValue {
        fn serialize<S: serde::Serializer>(&self, _s: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("intentional failure"))
        }
    }

    let result = Response::json(200, &BadValue);
    match result {
        Err(RuntimeError::InvalidArgument(msg)) => {
            assert!(
                msg.contains("serialization"),
                "error should mention serialization: {msg}"
            );
        }
        Err(other) => panic!("expected InvalidArgument, got: {other}"),
        Ok(_) => panic!("expected Err, got Ok"),
    }
}

#[test]
fn response_empty_rejects_invalid_status() {
    assert!(matches!(
        Response::empty(0),
        Err(RuntimeError::InvalidArgument(_))
    ));

    assert!(matches!(
        Response::empty(1000),
        Err(RuntimeError::InvalidArgument(_))
    ));

    assert_eq!(Response::empty(204).expect("valid status").status(), 204);
}

#[test]
fn response_bytes_rejects_invalid_status() {
    assert!(matches!(
        Response::bytes(0, vec![1, 2, 3]),
        Err(RuntimeError::InvalidArgument(_))
    ));

    assert!(matches!(
        Response::bytes(600, vec![]),
        Err(RuntimeError::InvalidArgument(_))
    ));

    let resp = Response::bytes(200, vec![1, 2, 3]).expect("valid status");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body_bytes(), &[1, 2, 3]);
}

#[test]
fn response_text_boundary_status_codes() {
    // 100 is the minimum valid status
    assert!(Response::text(100, "continue").is_ok());
    // 599 is the maximum valid status
    assert!(Response::text(599, "custom").is_ok());
    // 99 is below minimum
    assert!(Response::text(99, "bad").is_err());
}
