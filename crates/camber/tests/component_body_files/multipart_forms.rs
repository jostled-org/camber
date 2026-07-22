use crate::runtime_support as common;

use crate::deterministic::DeterministicGenerator;
use camber::http::{Request, Response, Router};
use camber::{RuntimeError, runtime};
use std::time::Duration;

const GENERATED_MULTIPART_CASES: u64 = 96;
const MAX_GENERATED_MULTIPART_BODY_BYTES: usize = 2 * 1024;

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

struct ExpectedGeneratedPart {
    name: Box<str>,
    filename: Option<Box<str>>,
    content_type: Option<Box<str>>,
    data: Box<[u8]>,
}

struct GeneratedWirePart {
    disposition: Box<str>,
    expected: ExpectedGeneratedPart,
}

enum GeneratedExpectation {
    Parts(Box<[ExpectedGeneratedPart]>),
    BadRequest,
}

struct GeneratedMultipartCase {
    content_type: Box<str>,
    body: Vec<u8>,
    expectation: GeneratedExpectation,
}

fn generated_boundary(case: &mut crate::deterministic::DeterministicCase) -> Box<str> {
    let fragment = case
        .select(&["alpha", "BETA7", "m1x_ed", "dash-9", "dot.4"])
        .expect("non-empty boundary fragment table");
    format!("camber-{fragment}-{}", case.index()).into_boxed_str()
}

fn embedded_boundary_payload(
    boundary: &str,
    case: &mut crate::deterministic::DeterministicCase,
) -> Box<[u8]> {
    let random_byte = *case
        .select(&[0x01_u8, 0x10, 0x7f, 0x80, 0xfe])
        .expect("non-empty payload byte table");
    let mut payload = vec![0x00, random_byte, 0xff];
    payload.extend_from_slice(b"prefix--");
    payload.extend_from_slice(boundary.as_bytes());
    payload.extend_from_slice(b"-suffix\r\n--");
    payload.extend_from_slice(boundary.as_bytes());
    payload.extend_from_slice(b"almost\x00tail");
    payload.into_boxed_slice()
}

fn build_generated_body(boundary: &str, parts: &[GeneratedWirePart]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(b"--");
        body.extend_from_slice(boundary.as_bytes());
        body.extend_from_slice(b"\r\nContent-Disposition: ");
        body.extend_from_slice(part.disposition.as_bytes());
        match part.expected.content_type.as_deref() {
            Some(content_type) => {
                body.extend_from_slice(b"\r\nCoNtEnT-TyPe: ");
                body.extend_from_slice(content_type.as_bytes());
            }
            None => {}
        }
        body.extend_from_slice(b"\r\n\r\n");
        body.extend_from_slice(&part.expected.data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"--\r\n");
    body
}

fn build_generated_single_field_body(boundary: &str, data: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"\r\nContent-Disposition: form-data; name=field\r\n\r\n");
    body.extend_from_slice(data);
    body.extend_from_slice(b"\r\n--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"--\r\n");
    body
}

fn generated_exact_case(
    scenario: u64,
    case: &mut crate::deterministic::DeterministicCase,
) -> GeneratedMultipartCase {
    let base_boundary = generated_boundary(case);
    let boundary: Box<str> = match scenario {
        1 => format!("{base_boundary}:=?").into_boxed_str(),
        _ => base_boundary,
    };
    let binary = embedded_boundary_payload(&boundary, case);
    let (first_name, first_filename, first_disposition): (Box<str>, Option<Box<str>>, Box<str>) =
        match scenario {
            0 => ("field".into(), None, "form-data; name=field".into()),
            1 => (
                "upload".into(),
                Some("blob.bin".into()),
                "FoRm-DaTa; NaMe=\"upload\"; FiLeNaMe=\"blob.bin\"".into(),
            ),
            2 => (
                "meta;field".into(),
                Some("part;two.bin".into()),
                "form-data; name=\"meta;field\"; filename=\"part;two.bin\"".into(),
            ),
            3 => (
                "field\"quoted".into(),
                Some("path\\file.bin".into()),
                "form-data; name=\"field\\\"quoted\"; filename=\"path\\\\file.bin\"".into(),
            ),
            _ => (
                "mixedCase".into(),
                Some("data.bin".into()),
                "FORM-DATA; ignored=value; NAME=\"mixedCase\"; FILENAME=data.bin".into(),
            ),
        };
    let parts = vec![
        GeneratedWirePart {
            disposition: first_disposition,
            expected: ExpectedGeneratedPart {
                name: first_name,
                filename: first_filename,
                content_type: Some("application/octet-stream".into()),
                data: binary,
            },
        },
        GeneratedWirePart {
            disposition: "form-data; name=note".into(),
            expected: ExpectedGeneratedPart {
                name: "note".into(),
                filename: None,
                content_type: None,
                data: format!("value-{}", case.index())
                    .into_bytes()
                    .into_boxed_slice(),
            },
        },
    ];
    let body = build_generated_body(&boundary, &parts);
    let expectation = GeneratedExpectation::Parts(
        parts
            .into_iter()
            .map(|part| part.expected)
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    let content_type = match scenario {
        0 => format!("MuLtIpArT/FoRm-DaTa; charset=utf-8; BoUnDaRy={boundary}"),
        1 => format!("multipart/form-data; boundary=\"{boundary}\""),
        2 => format!("MULTIPART/FORM-DATA; BOUNDARY=\"{boundary}\"; ignored=yes"),
        3 => format!("multipart/form-data; boundary={boundary}"),
        _ => format!("multipart/form-data; BOUNDARY=\"{boundary}\""),
    };
    GeneratedMultipartCase {
        content_type: content_type.into_boxed_str(),
        body,
        expectation,
    }
}

fn generated_bad_request_case(
    scenario: u64,
    case: &mut crate::deterministic::DeterministicCase,
) -> GeneratedMultipartCase {
    let boundary = generated_boundary(case);
    let payload = embedded_boundary_payload(&boundary, case);
    let mut body = build_generated_single_field_body(&boundary, &payload);
    let content_type = match scenario {
        5 => "multipart/form-data".into(),
        6 => "multipart/form-data; boundary=\"\"".into(),
        7 => format!("multipart/form-data; boundary={boundary}; BOUNDARY=other"),
        8 => format!("multipart/form-data; boundary=\"{boundary}"),
        9 => {
            let malformed = format!("{boundary} space");
            body = build_generated_single_field_body(&malformed, b"value");
            format!("multipart/form-data; boundary={malformed}")
        }
        10 => {
            let malformed = "x".repeat(71);
            body = build_generated_single_field_body(&malformed, b"value");
            format!("multipart/form-data; boundary=\"{malformed}\"")
        }
        11 => {
            let malformed = format!("{boundary} ");
            body = build_generated_single_field_body(&malformed, b"value");
            format!("multipart/form-data; boundary=\"{malformed}\"")
        }
        12 => {
            let malformed = format!("{boundary}@");
            body = build_generated_single_field_body(&malformed, b"value");
            format!("multipart/form-data; boundary=\"{malformed}\"")
        }
        13 => format!("multipart/form-data; boundary={boundary}-mismatch"),
        14 => {
            body.truncate(body.len().saturating_sub(4));
            format!("multipart/form-data; boundary={boundary}")
        }
        _ => "multipart/form-data; boundary=".into(),
    };
    GeneratedMultipartCase {
        content_type: content_type.into_boxed_str(),
        body,
        expectation: GeneratedExpectation::BadRequest,
    }
}

fn assert_generated_multipart_case(generated: GeneratedMultipartCase, context: &str) {
    assert!(
        generated.body.len() <= MAX_GENERATED_MULTIPART_BODY_BYTES,
        "{context}: generated body used {} bytes, limit is {MAX_GENERATED_MULTIPART_BODY_BYTES}",
        generated.body.len()
    );
    let request = multipart_request(&generated.content_type, generated.body);
    match (generated.expectation, request.multipart()) {
        (GeneratedExpectation::BadRequest, Err(RuntimeError::BadRequest(_))) => {}
        (GeneratedExpectation::BadRequest, Err(error)) => {
            panic!("{context}: expected concrete BadRequest, got {error:?}")
        }
        (GeneratedExpectation::BadRequest, Ok(reader)) => panic!(
            "{context}: malformed multipart parsed as {} parts",
            reader.parts().len()
        ),
        (GeneratedExpectation::Parts(_), Err(error)) => {
            panic!("{context}: legal multipart returned {error:?}")
        }
        (GeneratedExpectation::Parts(expected), Ok(reader)) => {
            assert_eq!(
                reader.parts().len(),
                expected.len(),
                "{context}: part count"
            );
            for (part_index, (actual, expected)) in
                reader.parts().iter().zip(expected.iter()).enumerate()
            {
                assert_eq!(
                    actual.name(),
                    expected.name.as_ref(),
                    "{context}: part {part_index} name"
                );
                assert_eq!(
                    actual.filename(),
                    expected.filename.as_deref(),
                    "{context}: part {part_index} filename"
                );
                assert_eq!(
                    actual.content_type(),
                    expected.content_type.as_deref(),
                    "{context}: part {part_index} content type"
                );
                assert_eq!(
                    actual.data(),
                    expected.data.as_ref(),
                    "{context}: part {part_index} payload"
                );
            }
        }
    }
}

#[test]
fn generated_multipart_boundary_and_parameter_matrix_is_bounded() {
    let generator = DeterministicGenerator::stable();
    for index in 0..GENERATED_MULTIPART_CASES {
        let mut case = generator.case(index);
        let scenario = index % 16;
        let generated = match scenario {
            0..=4 => generated_exact_case(scenario, &mut case),
            _ => generated_bad_request_case(scenario, &mut case),
        };
        assert_generated_multipart_case(generated, &case.to_string());
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
    common::test_runtime()
        .run(|| {
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

            let content_type = format!("multipart/form-data; boundary={boundary}");
            let response = crate::http::request(
                addr,
                "POST",
                "/upload",
                &[("Content-Type", &content_type)],
                &body,
                Duration::from_secs(5),
            )
            .unwrap();

            assert_eq!(response.status, 200);
            assert_eq!(response.body.as_ref(), b"field1,field2");

            runtime::request_shutdown();
        })
        .unwrap();
}
