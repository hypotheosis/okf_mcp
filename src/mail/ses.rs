//! The SES provider (issue #427): one SES v2 `SendEmail` per message through
//! the AWS SDK, never SES SMTP. Credentials come from the SDK's standard
//! chain (environment, profile, web identity, then the container provider
//! that EKS Pod Identity configures), so no key is ever configured here.
//!
//! The SDK does not retry: the mail queue owns retries ([`super::queue`]),
//! so each [`Mailer::send`] is one request, bounded by [`SES_TIMEOUT`].
//!
//! Classification:
//! - retryable: throttling and sending quotas, any 5xx, 408 or 429,
//!   timeouts, network errors, and every credential or authorization
//!   failure (no credentials found, any 401 or 403, an expired or invalid
//!   token, `AccessDenied`): like Postmark's
//!   401, an operator can fix them within the queue's retry window, and a
//!   Pod Identity agent that is briefly unreachable fixes itself;
//! - also retryable: account-level holds (`SendingPaused`,
//!   `AccountSuspended`), which say nothing about this message;
//! - permanent: `MessageRejected`, an unverified MAIL FROM domain, a bad
//!   request (an invalid address among them), a missing identity or
//!   configuration set, and any other 4xx with no retryable code.
//!
//! Error text names SES's error code and message, never a credential or a
//! message body.

use std::time::Duration;

use aws_config::meta::region::{ProvideRegion, RegionProviderChain};
use aws_config::retry::RetryConfig;
use aws_config::timeout::TimeoutConfig;
use aws_config::{BehaviorVersion, Region};
use aws_sdk_sesv2::config::http::HttpResponse;
use aws_sdk_sesv2::error::{DisplayErrorContext, ProvideErrorMetadata, SdkError};
use aws_sdk_sesv2::operation::send_email::SendEmailError;
use aws_sdk_sesv2::types::{Body, Content, Destination, EmailContent, Message};

use super::{config_error, Mail, MailError, Mailer};
use crate::config::SesConfig;
use crate::error::OkfError;

/// How long one send may take in all, credential lookup included.
const SES_TIMEOUT: Duration = Duration::from_secs(30);

/// How long connecting to SES may take.
const SES_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The charset SES is told every part is in (Rust strings are UTF-8).
const CHARSET: &str = "UTF-8";

/// Sends through SES. The SDK client holds a credential provider, not
/// credentials in its `Debug`, but there is no reason to print it.
pub(crate) struct SesMailer {
    client: aws_sdk_sesv2::Client,
    from: String,
}

/// Builds the SES mailer from the SDK's standard configuration chain.
/// Only `provider = "ses"` calls it, so nothing else ever looks for AWS
/// credentials. Credentials are not fetched here but on the first send
/// (and refreshed by the SDK as they expire).
///
/// # Arguments
/// * `from` - `mail.from` (a verified SES identity).
/// * `ses` - `[mail.ses]`, if present.
///
/// # Returns
/// The mailer.
///
/// # Errors
/// [`OkfError::Config`] when neither `mail.ses.region` nor the SDK's
/// region chain gives a region.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn from_config(
    from: &str,
    ses: Option<&SesConfig>,
) -> Result<SesMailer, OkfError> {
    let configured = ses.and_then(|ses| ses.region.as_deref());
    let region = resolve_region(
        configured,
        aws_config::default_provider::region::default_provider(),
    )
    .await?;
    // `defaults` reads the standard chains; the region is already settled.
    let sdk = aws_config::defaults(BehaviorVersion::latest())
        .region(region)
        .load()
        .await;
    Ok(SesMailer::new(
        aws_sdk_sesv2::config::Builder::from(&sdk),
        from,
    ))
}

/// The region to send in: `mail.ses.region` when set, else the fallback
/// chain's answer.
///
/// # Arguments
/// * `configured` - `mail.ses.region` (already checked not blank).
/// * `fallback` - the SDK's region chain (tests pass a fixed answer).
///
/// # Returns
/// The region.
///
/// # Errors
/// [`OkfError::Config`] when neither gives one.
// `impl ProvideRegion + 'static`: any region source the chain can own, like
// a Python argument typed with a Protocol.
#[tracing::instrument(level = "debug", skip_all)]
async fn resolve_region(
    configured: Option<&str>,
    fallback: impl ProvideRegion + 'static,
) -> Result<Region, OkfError> {
    // `Option<Region>` is itself a region source: `None` defers to the next.
    let chain = RegionProviderChain::first_try(
        configured.map(|region| Region::new(region.trim().to_string())),
    )
    .or_else(fallback);
    chain.region().await.ok_or_else(|| {
        config_error(
            "provider = \"ses\" found no AWS region: set mail.ses.region, or AWS_REGION \
             in the server's environment"
                .to_string(),
        )
    })
}

impl SesMailer {
    /// An SES mailer from a client configuration, with this module's
    /// timeouts and no SDK retries.
    ///
    /// # Arguments
    /// * `config` - the SES client configuration (region and credentials;
    ///   tests also set an endpoint).
    /// * `from` - `mail.from`.
    ///
    /// # Returns
    /// The mailer.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn new(config: aws_sdk_sesv2::config::Builder, from: &str) -> Self {
        let config = config
            // One attempt: the queue retries with its own backoff, so an SDK
            // retry loop inside it would only multiply the attempts.
            .retry_config(RetryConfig::disabled())
            .timeout_config(
                TimeoutConfig::builder()
                    .connect_timeout(SES_CONNECT_TIMEOUT)
                    .operation_timeout(SES_TIMEOUT)
                    .build(),
            )
            .build();
        Self {
            client: aws_sdk_sesv2::Client::from_conf(config),
            from: from.to_string(),
        }
    }
}

/// One UTF-8 part of a message.
#[tracing::instrument(level = "trace", skip_all)]
fn utf8(data: &str) -> Result<Content, MailError> {
    Content::builder()
        .data(data)
        .charset(CHARSET)
        .build()
        // Only a missing `data` fails, and it is always set.
        .map_err(|error| MailError::Permanent(format!("ses: building the message: {error}")))
}

/// Sorts a failed `SendEmail` (see the module docs for the policy).
// `SdkError<E, R>`: the SDK's error, generic over the operation's modeled
// error `E` and the raw response type `R` (here the SDK's HTTP response).
#[tracing::instrument(level = "trace", skip_all)]
fn classify(error: &SdkError<SendEmailError, HttpResponse>) -> MailError {
    let SdkError::ServiceError(service) = error else {
        // `DisplayErrorContext` prints the whole source chain (the
        // connector's or credential provider's reason), which has neither
        // a credential nor the body.
        let detail = format!("ses: {}", DisplayErrorContext(error));
        // A request that could not even be built fails the same way every
        // time; everything else (timeouts, network and credential failures,
        // an unreadable answer) may pass.
        return match error {
            SdkError::ConstructionFailure(_) => MailError::Permanent(detail),
            _ => MailError::Retryable(detail),
        };
    };
    let refusal = service.err();
    let code = refusal.code().unwrap_or("unknown");
    let detail = format!(
        "ses refused the message ({code}): {}",
        refusal.message().unwrap_or("no message")
    );
    match refusal {
        SendEmailError::MessageRejected(_)
        | SendEmailError::MailFromDomainNotVerifiedException(_)
        | SendEmailError::BadRequestException(_)
        | SendEmailError::NotFoundException(_) => MailError::Permanent(detail),
        SendEmailError::TooManyRequestsException(_)
        | SendEmailError::LimitExceededException(_)
        | SendEmailError::SendingPausedException(_)
        | SendEmailError::AccountSuspendedException(_) => MailError::Retryable(detail),
        // Errors SES does not model for `SendEmail` (or an answer with no
        // error type at all, such as a gateway's 502): throttling, auth and
        // server errors by code or by status, everything else is a refusal.
        _ if retryable_code(code) || retryable_status(service.raw().status().as_u16()) => {
            MailError::Retryable(detail)
        }
        _ => MailError::Permanent(detail),
    }
}

/// Whether an HTTP status may pass whatever its error code: a request
/// timeout, throttling, a credential or authorization refusal (401, 403),
/// or any server error.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn retryable_status(status: u16) -> bool {
    matches!(status, 401 | 403 | 408 | 429 | 500..=599)
}

/// Whether an unmodeled SES error code may pass: throttling, a credential
/// or authorization problem an operator can fix, or a server error.
#[tracing::instrument(level = "trace", skip_all)]
fn retryable_code(code: &str) -> bool {
    matches!(
        code,
        "Throttling"
            | "ThrottlingException"
            | "TooManyRequestsException"
            | "RequestTimeout"
            | "RequestTimeoutException"
            | "ServiceUnavailable"
            | "ServiceUnavailableException"
            | "InternalFailure"
            | "InternalServerError"
            | "InternalServiceErrorException"
            | "AccessDenied"
            | "AccessDeniedException"
            | "UnrecognizedClientException"
            | "InvalidClientTokenId"
            | "InvalidSignatureException"
            | "SignatureDoesNotMatch"
            | "ExpiredToken"
            | "ExpiredTokenException"
            | "MissingAuthenticationToken"
            | "IncompleteSignature"
    )
}

#[async_trait::async_trait]
impl Mailer for SesMailer {
    #[tracing::instrument(level = "debug", skip_all)]
    async fn send(&self, mail: &Mail) -> Result<(), MailError> {
        let message = Message::builder()
            .subject(utf8(&mail.subject)?)
            .body(
                Body::builder()
                    .text(utf8(&mail.text)?)
                    .html(utf8(&mail.html)?)
                    .build(),
            )
            .build();
        self.client
            .send_email()
            .from_email_address(&self.from)
            .destination(Destination::builder().to_addresses(&mail.to).build())
            .content(EmailContent::builder().simple(message).build())
            .send()
            .await
            .map(|_| ())
            .map_err(|error| classify(&error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use aws_sdk_sesv2::config::{Credentials, SharedCredentialsProvider};
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::{HeaderMap, Method, StatusCode, Uri};
    use axum::response::{IntoResponse, Response};
    use axum::Router;
    use serde_json::{json, Value};

    /// A recorded request: method, path, headers and JSON body.
    type Seen = (Method, String, HeaderMap, Value);

    /// What the mock saw, and what it answers.
    #[derive(Clone)]
    struct Mock {
        seen: Arc<Mutex<Vec<Seen>>>,
        status: StatusCode,
        // SES v2 is REST-JSON: an error's type is the `x-amzn-errortype`
        // header, its text the body's `message`.
        error_type: Option<&'static str>,
        delay: Duration,
    }

    async fn handle(
        State(mock): State<Mock>,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
        mock.seen
            .lock()
            .unwrap()
            .push((method, uri.path().to_string(), headers, body));
        tokio::time::sleep(mock.delay).await;
        let mut response = match mock.error_type {
            None => (mock.status, axum::Json(json!({"MessageId": "m-1"}))).into_response(),
            Some(kind) => (
                mock.status,
                axum::Json(json!({"message": format!("mock {kind}")})),
            )
                .into_response(),
        };
        if let Some(kind) = mock.error_type {
            response
                .headers_mut()
                .insert("x-amzn-errortype", kind.parse().unwrap());
        }
        response
    }

    /// Serves a mock SES endpoint on a free port.
    async fn mock_ses(status: u16, error_type: Option<&'static str>) -> (String, Mock) {
        mock_ses_slow(status, error_type, Duration::ZERO).await
    }

    async fn mock_ses_slow(
        status: u16,
        error_type: Option<&'static str>,
        delay: Duration,
    ) -> (String, Mock) {
        let mock = Mock {
            seen: Arc::new(Mutex::new(Vec::new())),
            status: StatusCode::from_u16(status).unwrap(),
            error_type,
            delay,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new().fallback(handle).with_state(mock.clone());
        tokio::spawn(async move { axum::serve(listener, router).await });
        (endpoint, mock)
    }

    const SECRET_KEY: &str = "test-secret-key-s3cr3t";

    fn client_config(endpoint: &str) -> aws_sdk_sesv2::config::Builder {
        aws_sdk_sesv2::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .endpoint_url(endpoint)
            .credentials_provider(Credentials::new(
                "AKIDEXAMPLE",
                SECRET_KEY,
                Some("session-token-s3cr3t".to_string()),
                None,
                "test",
            ))
    }

    fn mailer(endpoint: &str) -> SesMailer {
        SesMailer::new(client_config(endpoint), "okf <okf@example.com>")
    }

    fn mail() -> Mail {
        Mail {
            to: "alice@example.com".to_string(),
            subject: "Réinitialiser — パスワード".to_string(),
            text: "https://okf.example.com/account/reset?token=link-s3cr3t".to_string(),
            html: "<a href=\"https://okf.example.com/account/reset?token=link-s3cr3t\">ünïcode</a>"
                .to_string(),
        }
    }

    /// Asserts a failure's text holds no credential, body or link.
    fn assert_clean(error: &MailError) {
        let shown = format!("{error} {error:?}");
        for secret in [SECRET_KEY, "session-token-s3cr3t", "link-s3cr3t", "ünïcode"] {
            assert!(!shown.contains(secret), "{secret} in {shown}");
        }
    }

    #[tokio::test]
    async fn the_request_is_a_signed_utf8_send_email() {
        let (endpoint, mock) = mock_ses(200, None).await;

        mailer(&endpoint).send(&mail()).await.unwrap();

        let seen = mock.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let (method, path, headers, body) = &seen[0];
        assert_eq!(method, Method::POST);
        assert_eq!(path, "/v2/email/outbound-emails");
        // SigV4, with the access key id but never the secret.
        let authorization = headers["authorization"].to_str().unwrap();
        assert!(
            authorization.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"),
            "{authorization}"
        );
        assert!(authorization.contains("/us-east-1/ses/aws4_request"));
        assert_eq!(
            body,
            &json!({
                "FromEmailAddress": "okf <okf@example.com>",
                "Destination": {"ToAddresses": ["alice@example.com"]},
                "Content": {"Simple": {
                    "Subject": {"Data": "Réinitialiser — パスワード", "Charset": "UTF-8"},
                    "Body": {
                        "Text": {
                            "Data": "https://okf.example.com/account/reset?token=link-s3cr3t",
                            "Charset": "UTF-8"
                        },
                        "Html": {
                            "Data": "<a href=\"https://okf.example.com/account/reset?token=link-s3cr3t\">ünïcode</a>",
                            "Charset": "UTF-8"
                        }
                    }
                }}
            })
        );
    }

    #[tokio::test]
    async fn rejections_and_bad_requests_are_permanent() {
        for (status, kind) in [
            (400, "MessageRejected"),
            (400, "MailFromDomainNotVerifiedException"),
            (400, "BadRequestException"),
            (404, "NotFoundException"),
            (400, "SomeFutureValidationException"),
        ] {
            let (endpoint, _mock) = mock_ses(status, Some(kind)).await;

            let refused = mailer(&endpoint).send(&mail()).await.unwrap_err();

            assert!(
                matches!(&refused, MailError::Permanent(detail) if detail.contains(kind)),
                "{kind}: {refused:?}"
            );
            assert_clean(&refused);
        }
    }

    #[tokio::test]
    async fn throttling_quotas_holds_and_outages_are_retryable_in_one_attempt() {
        for (status, kind) in [
            (429, "TooManyRequestsException"),
            (400, "LimitExceededException"),
            (400, "SendingPausedException"),
            (400, "AccountSuspendedException"),
            (400, "ThrottlingException"),
            (500, "InternalFailure"),
            (503, "ServiceUnavailable"),
            // Unlisted codes on a retryable status.
            (502, "SomeGatewayException"),
            (504, "SomeFutureServerException"),
            (408, "SomeTimeoutException"),
        ] {
            let (endpoint, mock) = mock_ses(status, Some(kind)).await;

            let refused = mailer(&endpoint).send(&mail()).await.unwrap_err();

            assert!(
                matches!(&refused, MailError::Retryable(detail) if detail.contains(kind)),
                "{kind}: {refused:?}"
            );
            assert_clean(&refused);
            // No SDK retry inside the queue's.
            assert_eq!(mock.seen.lock().unwrap().len(), 1, "{kind}");
        }
    }

    #[tokio::test]
    async fn a_server_error_without_an_error_type_is_retryable() {
        for status in [500, 502, 503] {
            let (endpoint, _mock) = mock_ses(status, None).await;

            let refused = mailer(&endpoint).send(&mail()).await.unwrap_err();

            assert!(
                matches!(refused, MailError::Retryable(_)),
                "{status}: {refused:?}"
            );
            assert_clean(&refused);
        }
    }

    #[tokio::test]
    async fn authorization_failures_are_retryable() {
        for (status, kind) in [
            (403, "AccessDeniedException"),
            (403, "UnrecognizedClientException"),
            (403, "InvalidSignatureException"),
            (403, "ExpiredTokenException"),
            // Unlisted codes on 401 and 403.
            (403, "OptInRequired"),
            (401, "SomeFutureAuthException"),
        ] {
            let (endpoint, _mock) = mock_ses(status, Some(kind)).await;

            let refused = mailer(&endpoint).send(&mail()).await.unwrap_err();

            assert!(
                matches!(&refused, MailError::Retryable(detail) if detail.contains(kind)),
                "{kind}: {refused:?}"
            );
            assert_clean(&refused);
        }
    }

    /// A credential source that never has credentials, like a pod whose
    /// identity association is missing.
    #[derive(Debug)]
    struct NoCredentials;

    impl aws_sdk_sesv2::config::ProvideCredentials for NoCredentials {
        fn provide_credentials<'a>(
            &'a self,
        ) -> aws_credential_types::provider::future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            aws_credential_types::provider::future::ProvideCredentials::ready(Err(
                aws_credential_types::provider::error::CredentialsError::not_loaded(
                    "no pod identity",
                ),
            ))
        }
    }

    #[tokio::test]
    async fn missing_credentials_are_retryable_and_never_reach_ses() {
        let (endpoint, mock) = mock_ses(200, None).await;
        let config = client_config(&endpoint)
            .credentials_provider(SharedCredentialsProvider::new(NoCredentials));

        let refused = SesMailer::new(config, "okf <okf@example.com>")
            .send(&mail())
            .await
            .unwrap_err();

        assert!(
            matches!(&refused, MailError::Retryable(detail) if detail.contains("no pod identity")),
            "{refused:?}"
        );
        assert_clean(&refused);
        assert!(mock.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unreachable_endpoints_are_retryable() {
        // A port held but never listened on: every connect is refused.
        let unlistened = crate::test_net::Refused::new();
        let closed = format!("http://{}", unlistened.address());

        let refused = crate::test_net::bounded(
            "a send to a refusing address",
            mailer(&closed).send(&mail()),
        )
        .await
        .unwrap_err();

        assert!(matches!(refused, MailError::Retryable(_)), "{refused:?}");
        assert_clean(&refused);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_send_times_out_as_retryable() {
        let (endpoint, _mock) = mock_ses_slow(200, None, SES_TIMEOUT * 2).await;

        let refused = mailer(&endpoint).send(&mail()).await.unwrap_err();

        assert!(matches!(refused, MailError::Retryable(_)), "{refused:?}");
    }

    #[tokio::test]
    async fn the_configured_region_wins_over_the_chain() {
        let region = resolve_region(Some("eu-west-1"), Region::new("us-east-1"))
            .await
            .unwrap();

        assert_eq!(region.as_ref(), "eu-west-1");
    }

    #[tokio::test]
    async fn without_a_configured_region_the_chain_decides() {
        let region = resolve_region(None, Region::new("us-west-2"))
            .await
            .unwrap();

        assert_eq!(region.as_ref(), "us-west-2");
    }

    #[tokio::test]
    async fn no_region_anywhere_stops_start_up_with_advice() {
        let refused = resolve_region(None, None::<Region>).await.unwrap_err();

        assert!(
            matches!(&refused, OkfError::Config { message, .. }
                if message.contains("mail.ses.region") && message.contains("AWS_REGION")),
            "{refused:?}"
        );
    }
}
