mod common;

use camber::http::{Request, Response, Router};
use camber::runtime;
use std::io::{Read, Write};

/// Build a multipart body with text fields and optional file parts.
fn build_multipart_body(boundary: &str, parts: &[TestPart<'_>]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(b"--");
        body.extend_from_slice(boundary.as_bytes());
        body.extend_from_slice(b"\r\n");

        match part.filename {
            Some(filename) => {
                body.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n",
                        part.name, filename
                    )
                    .as_bytes(),
                );
            }
            None => {
                body.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{}\"\r\n", part.name)
                        .as_bytes(),
                );
            }
        }

        if let Some(ct) = part.content_type {
            body.extend_from_slice(format!("Content-Type: {ct}\r\n").as_bytes());
        }

        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(part.data);
        body.extend_from_slice(b"\r\n");
    }

    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"--\r\n");
    body
}

struct TestPart<'a> {
    name: &'a str,
    filename: Option<&'a str>,
    content_type: Option<&'a str>,
    data: &'a [u8],
}

fn multipart_request(content_type: &str, body: Vec<u8>) -> Request {
    Request::builder()
        .method("POST")
        .expect("valid method")
        .header("Content-Type", content_type)
        .body_raw(body)
        .finish()
        .expect("valid request")
}

fn multipart_ok(req: &Request) -> camber::http::MultipartReader {
    match req.multipart() {
        Ok(reader) => reader,
        Err(err) => panic!("expected multipart parse success: {err}"),
    }
}

fn assert_bad_request(result: Result<camber::http::MultipartReader, camber::RuntimeError>) {
    match result {
        Ok(_) => panic!("expected multipart parse failure"),
        Err(err) => assert!(
            err.to_string().contains("bad request"),
            "expected bad request error, got: {err}"
        ),
    }
}

fn multipart_names_response(
    result: Result<camber::http::MultipartReader, camber::RuntimeError>,
) -> Response {
    match result {
        Ok(reader) => {
            let names: Vec<&str> = reader.parts().iter().map(|part| part.name()).collect();
            Response::text(200, &names.join(",")).expect("valid status")
        }
        Err(err) => Response::text(400, &err.to_string()).expect("valid status"),
    }
}

#[test]
fn multipart_parses_text_field() {
    let boundary = "----testboundary";
    let body = build_multipart_body(
        boundary,
        &[TestPart {
            name: "username",
            filename: None,
            content_type: None,
            data: b"alice",
        }],
    );

    let content_type = format!("multipart/form-data; boundary={boundary}");
    let req = Request::builder()
        .method("POST")
        .expect("valid method")
        .header("Content-Type", &content_type)
        .body_raw(body)
        .finish()
        .expect("valid request");

    let reader = multipart_ok(&req);
    let parts: Vec<_> = reader.parts().iter().collect();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].name(), "username");
    assert_eq!(parts[0].filename(), None);
    assert_eq!(parts[0].data(), b"alice");
}

#[test]
fn multipart_ignores_boundary_substring_inside_part_body() {
    let boundary = "----payloadboundary";
    let payload = b"prefix--payloadboundarysuffix\r\nnot-a-delimiter";
    let body = build_multipart_body(
        boundary,
        &[TestPart {
            name: "upload",
            filename: Some("blob.bin"),
            content_type: Some("application/octet-stream"),
            data: payload,
        }],
    );

    let content_type = format!("multipart/form-data; boundary={boundary}");
    let req = multipart_request(&content_type, body);

    let reader = multipart_ok(&req);
    let parts = reader.parts();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].data(), payload);
}

#[test]
fn multipart_accepts_quoted_boundary_parameter() {
    let boundary = "----quotedboundary";
    let body = build_multipart_body(
        boundary,
        &[TestPart {
            name: "field",
            filename: None,
            content_type: None,
            data: b"ok",
        }],
    );

    let content_type = format!("multipart/form-data; boundary=\"{boundary}\"");
    let req = multipart_request(&content_type, body);

    let reader = multipart_ok(&req);
    assert_eq!(reader.parts().len(), 1);
    assert_eq!(reader.parts()[0].name(), "field");
    assert_eq!(reader.parts()[0].data(), b"ok");
}

#[test]
fn multipart_accepts_quoted_part_parameters_with_semicolons() {
    let boundary = "----quotedparams";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"meta;name\"; filename=\"a;b.txt\"\r\nContent-Type: text/plain\r\n\r\nhello\r\n--{boundary}--\r\n"
    )
    .into_bytes();
    let content_type = format!("multipart/form-data; boundary={boundary}");

    let request = multipart_request(&content_type, body);
    let reader = multipart_ok(&request);
    let parts = reader.parts();

    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].name(), "meta;name");
    assert_eq!(parts[0].filename(), Some("a;b.txt"));
    assert_eq!(parts[0].content_type(), Some("text/plain"));
    assert_eq!(parts[0].data(), b"hello");
}

#[test]
fn multipart_rejects_invalid_start_or_closing_delimiter_framing() {
    let boundary = "----badframing";
    let content_type = format!("multipart/form-data; boundary={boundary}");

    let invalid_start = format!(
        "--{boundary}Content-Disposition: form-data; name=\"field\"\r\n\r\nvalue\r\n--{boundary}--\r\n"
    )
    .into_bytes();
    assert_bad_request(multipart_request(&content_type, invalid_start).multipart());

    let invalid_closing = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"field\"\r\n\r\nvalue--{boundary}--\r\n"
    )
    .into_bytes();
    assert_bad_request(multipart_request(&content_type, invalid_closing).multipart());
}

#[test]
fn multipart_parses_file_upload() {
    let boundary = "----fileboundary";
    let file_data = b"<html><body>hello</body></html>";
    let body = build_multipart_body(
        boundary,
        &[TestPart {
            name: "document",
            filename: Some("page.html"),
            content_type: Some("text/html"),
            data: file_data,
        }],
    );

    let content_type = format!("multipart/form-data; boundary={boundary}");
    let req = Request::builder()
        .method("POST")
        .expect("valid method")
        .header("Content-Type", &content_type)
        .body_raw(body)
        .finish()
        .expect("valid request");

    let reader = multipart_ok(&req);
    let parts: Vec<_> = reader.parts().iter().collect();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].name(), "document");
    assert_eq!(parts[0].filename(), Some("page.html"));
    assert_eq!(parts[0].content_type(), Some("text/html"));
    assert_eq!(parts[0].data(), file_data);
}

#[test]
fn multipart_parses_mixed_fields_and_files() {
    let boundary = "----mixedboundary";
    let body = build_multipart_body(
        boundary,
        &[
            TestPart {
                name: "title",
                filename: None,
                content_type: None,
                data: b"My Document",
            },
            TestPart {
                name: "author",
                filename: None,
                content_type: None,
                data: b"Bob",
            },
            TestPart {
                name: "attachment",
                filename: Some("data.csv"),
                content_type: Some("text/csv"),
                data: b"a,b,c\n1,2,3",
            },
        ],
    );

    let content_type = format!("multipart/form-data; boundary={boundary}");
    let req = Request::builder()
        .method("POST")
        .expect("valid method")
        .header("Content-Type", &content_type)
        .body_raw(body)
        .finish()
        .expect("valid request");

    let reader = multipart_ok(&req);
    let parts = reader.parts();
    assert_eq!(parts.len(), 3);

    assert_eq!(parts[0].name(), "title");
    assert_eq!(parts[0].filename(), None);
    assert_eq!(parts[0].data(), b"My Document");

    assert_eq!(parts[1].name(), "author");
    assert_eq!(parts[1].data(), b"Bob");

    assert_eq!(parts[2].name(), "attachment");
    assert_eq!(parts[2].filename(), Some("data.csv"));
    assert_eq!(parts[2].content_type(), Some("text/csv"));
    assert_eq!(parts[2].data(), b"a,b,c\n1,2,3");
}

#[test]
fn multipart_returns_error_for_non_multipart_body() {
    let req = Request::builder()
        .method("POST")
        .expect("valid method")
        .header("Content-Type", "application/json")
        .body("{}")
        .finish()
        .expect("valid request");

    let result = req.multipart();
    assert!(result.is_err());
    match result {
        Ok(_) => panic!("expected multipart parse failure"),
        Err(err) => assert!(err.to_string().contains("bad request")),
    }
}

#[test]
fn multipart_rejects_part_without_name_parameter() {
    let boundary = "----missingname";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; filename=\"file.txt\"\r\n\r\nhello\r\n--{boundary}--\r\n"
    )
    .into_bytes();
    let content_type = format!("multipart/form-data; boundary={boundary}");

    assert_bad_request(multipart_request(&content_type, body).multipart());
}

#[test]
fn multipart_rejects_duplicate_name_parameter() {
    let boundary = "----duplicatename";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"first\"; name=\"second\"\r\n\r\nhello\r\n--{boundary}--\r\n"
    )
    .into_bytes();
    let content_type = format!("multipart/form-data; boundary={boundary}");

    assert_bad_request(multipart_request(&content_type, body).multipart());
}

#[test]
fn multipart_rejects_duplicate_boundary_parameter() {
    let boundary = "----dupboundary";
    let body = build_multipart_body(
        boundary,
        &[TestPart {
            name: "field",
            filename: None,
            content_type: None,
            data: b"ok",
        }],
    );
    let content_type = format!("multipart/form-data; boundary={boundary}; boundary=other");

    assert_bad_request(multipart_request(&content_type, body).multipart());
}

#[test]
fn multipart_preserves_repeated_fields_and_binary_payloads() {
    let boundary = "----repeatedfields";
    let binary = b"\x00\x01\xff--not-a-boundary\r\n\x10";
    let body = build_multipart_body(
        boundary,
        &[
            TestPart {
                name: "tag",
                filename: None,
                content_type: None,
                data: b"one",
            },
            TestPart {
                name: "tag",
                filename: None,
                content_type: None,
                data: b"two",
            },
            TestPart {
                name: "blob",
                filename: Some("blob.bin"),
                content_type: Some("application/octet-stream"),
                data: binary,
            },
        ],
    );

    let content_type = format!("multipart/form-data; boundary={boundary}");
    let request = multipart_request(&content_type, body);
    let reader = multipart_ok(&request);
    let parts = reader.parts();

    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0].name(), "tag");
    assert_eq!(parts[0].data(), b"one");
    assert_eq!(parts[1].name(), "tag");
    assert_eq!(parts[1].data(), b"two");
    assert_eq!(parts[2].name(), "blob");
    assert_eq!(parts[2].filename(), Some("blob.bin"));
    assert_eq!(parts[2].content_type(), Some("application/octet-stream"));
    assert_eq!(parts[2].data(), binary);
}

#[test]
fn multipart_round_trip_through_server() {
    common::test_runtime().run(|| {
        let mut router = Router::new();
        router.post("/upload", |req: &Request| {
            let result = req.multipart();
            async move { multipart_names_response(result) }
        });
        let addr = common::spawn_server(router);

        let boundary = "----serverboundary";
        let body = build_multipart_body(
            boundary,
            &[
                TestPart {
                    name: "field1",
                    filename: None,
                    content_type: None,
                    data: b"value1",
                },
                TestPart {
                    name: "field2",
                    filename: None,
                    content_type: None,
                    data: b"value2",
                },
            ],
        );

        let connect = std::net::TcpStream::connect(addr);
        assert!(connect.is_ok(), "connect failed: {connect:?}");
        let Ok(mut stream) = connect else {
            return;
        };
        let request = format!(
            "POST /upload HTTP/1.1\r\nHost: {addr}\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let write_headers = stream.write_all(request.as_bytes());
        assert!(write_headers.is_ok(), "write failed: {write_headers:?}");
        let write_body = stream.write_all(&body);
        assert!(write_body.is_ok(), "write failed: {write_body:?}");

        let mut response = String::new();
        let read = stream.read_to_string(&mut response);
        assert!(read.is_ok(), "read failed: {read:?}");

        assert!(response.contains("200"));
        assert!(response.contains("field1,field2"));

        runtime::request_shutdown();
    }).unwrap();
}
