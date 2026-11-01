#![allow(clippy::expect_used)]

use std::time::Duration;

use autumn_web::reexports::http::StatusCode;

use super::*;

fn all() -> Vec<Error> {
    let json = serde_json::from_str::<serde_json::Value>("{").err();
    let mut errors = vec![
        Error::Config("c".into()),
        Error::Signature("s".into()),
        Error::Replay("r".into()),
        Error::InvalidRequest("i".into()),
        Error::PayloadTooLarge { limit: 1 },
        Error::LinearApi {
            status: 500,
            body: "b".into(),
        },
        Error::GraphQl(vec!["a".into(), "b".into()]),
        Error::RateLimited {
            retries: 2,
            retry_after: Some(Duration::from_secs(1)),
        },
        Error::UnexpectedResponse("u".into()),
        Error::InProgress("d".into()),
        Error::StateMismatch,
        Error::AuthorizationDenied("access_denied".into()),
        Error::NotInstalled("org".into()),
        Error::Reauthorize("org".into()),
        Error::OAuthDisabled,
        Error::WebhooksDisabled,
        Error::Store("x".into()),
        Error::MissingRuntime,
    ];
    if let Some(json) = json {
        errors.push(Error::Json(json));
    }
    errors
}

#[test]
fn each_kind_maps_to_its_documented_status() {
    let expected = |kind| match kind {
        ErrorKind::Signature
        | ErrorKind::Replay
        | ErrorKind::NotInstalled
        | ErrorKind::Reauthorize => 401,
        ErrorKind::InvalidRequest | ErrorKind::Json => 400,
        ErrorKind::PayloadTooLarge => 413,
        ErrorKind::StateMismatch | ErrorKind::AuthorizationDenied => 403,
        ErrorKind::OAuthDisabled | ErrorKind::WebhooksDisabled => 404,
        ErrorKind::InProgress => 409,
        ErrorKind::Http
        | ErrorKind::LinearApi
        | ErrorKind::GraphQl
        | ErrorKind::UnexpectedResponse => 502,
        ErrorKind::RateLimited | ErrorKind::MissingRuntime => 503,
        _ => 500,
    };
    for err in all() {
        assert_eq!(err.status().as_u16(), expected(err.kind()), "{err:?}");
    }
}

#[test]
fn to_autumn_error_keeps_the_status_and_message() {
    let err = Error::InProgress("d-1".into());
    let autumn = err.to_autumn_error();
    assert_eq!(autumn.status(), StatusCode::CONFLICT);
    assert!(autumn.to_string().contains("d-1"), "{autumn}");
}

#[test]
fn messages_join_graphql_errors() {
    let err = Error::GraphQl(vec!["one".into(), "two".into()]);
    assert_eq!(err.to_string(), "linear graphql error: one; two");
}

#[test]
fn transient_errors_are_rate_limits_5xx_and_in_flight() {
    let transient: Vec<bool> = all().iter().map(Error::is_transient).collect();
    let names: Vec<ErrorKind> = all().iter().map(Error::kind).collect();
    for (kind, transient) in names.iter().zip(transient) {
        let want = matches!(
            kind,
            ErrorKind::RateLimited | ErrorKind::LinearApi | ErrorKind::InProgress
        );
        assert_eq!(transient, want, "{kind:?}");
    }
    let client_error = Error::LinearApi {
        status: 400,
        body: String::new(),
    };
    assert!(!client_error.is_transient());
}
