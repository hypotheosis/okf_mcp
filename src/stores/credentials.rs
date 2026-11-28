//! How a store's database password is found: the three credential kinds of
//! a `host` row (`env`, `secrets-manager`, `iam`) as three implementations of
//! [`CredentialSource`].
//!
//! **Where the database user is.** There is no `user` column. Each kind
//! says it in its own way, so no migration is needed:
//!
//! * `env`: the reference names a variable holding the password; the user is
//!   the account database's own user (the user, TLS and other settings of a
//!   host store come from the account pool's options).
//! * `secrets-manager`: the reference is a secret ARN whose JSON value has
//!   `username` and `password` (the format RDS-managed secrets use), so the
//!   secret carries the user.
//! * `iam`: the reference *is* the database user the token is signed for.
//!
//! **Privacy.** A [`Credential`] prints as `[redacted]`, and every
//! [`CredentialError`] holds only text that is not secret by construction (a
//! variable's name, an AWS error *code*, a fixed phrase). An AWS error's
//! message is never kept: it can name the secret's ARN. So nothing here can
//! put a password, a token or an ARN in a log or a reply.
//!
//! **The AWS clients sit behind two small traits** ([`SecretReader`],
//! [`TokenSigner`]), so tests mock them. The real ones share one lazily
//! loaded `aws-config` chain, the chain the SES mailer uses.
//!
//! **Refreshing.** A pool's connect options carry the password, and every
//! new physical connection uses them. [`spawn_keeper`] starts one task per
//! pool that replaces them (`Pool::set_connect_options`, sqlx-core 0.9
//! `src/pool/mod.rs:555`) before an IAM token expires, and when a connection
//! attempt fails authentication. The task ends when the pool's registry
//! entry is dropped, so forgetting a store leaves no task behind.

use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use aws_config::{BehaviorVersion, SdkConfig};
use aws_credential_types::provider::ProvideCredentials;
use aws_sigv4::http_request::{
    sign, SignableBody, SignableRequest, SignatureLocation, SigningSettings,
};
use aws_sigv4::sign::v4;
use aws_smithy_runtime_api::client::identity::Identity;
use serde::Deserialize;
use sqlx::postgres::{PgConnectOptions, PgPool};
use tokio::sync::{broadcast, oneshot, watch, OnceCell};

use crate::store::stores::CredentialKind;
use crate::telemetry::subscriber::split_event;

/// How long an RDS IAM token is valid: AWS's fixed 15 minutes.
const IAM_TOKEN_LIFETIME: Duration = Duration::from_secs(15 * 60);

/// How often an IAM store's token is replaced: well inside its lifetime, so
/// one failed refresh still leaves time for the next.
pub(super) const IAM_REFRESH_EVERY: Duration = Duration::from_secs(10 * 60);

/// The shortest time between two refreshes of one pool, however many
/// connection attempts fail authentication meanwhile: a wrong password must
/// not turn every request into a call to AWS.
const MIN_REFRESH_GAP: Duration = Duration::from_secs(10);

/// How soon a failed refresh is tried again.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(30);

/// The longest an AWS call may take.
const AWS_CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// A database login, as a pool's connect options carry it.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Credential {
    /// The user, when the source decides it; `None` keeps the base
    /// options' user.
    user: Option<String>,
    /// The password or token.
    password: String,
}

impl std::fmt::Debug for Credential {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "a Debug impl")
    )]
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Credential([redacted])")
    }
}

impl Credential {
    /// A login.
    ///
    /// # Arguments
    /// * `user` - the database user, or `None` to keep the base options'.
    /// * `password` - the password or token.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "one-line constructor")
    )]
    pub(super) fn new(user: Option<String>, password: String) -> Self {
        Self { user, password }
    }

    /// The connect options with this login set.
    ///
    /// # Arguments
    /// * `base` - the options to build on (host, port, database, TLS).
    ///
    /// # Returns
    /// `base` with the password, and the user if the source gave one.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "sets two fields, no I/O")
    )]
    pub(super) fn apply(&self, base: PgConnectOptions) -> PgConnectOptions {
        let with_password = base.password(&self.password);
        match &self.user {
            Some(user) => with_password.username(user),
            None => with_password,
        }
    }
}

/// Why a credential could not be found. Every field is non-secret by
/// construction, so the error's `Display` and `Debug` are safe to log.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(super) enum CredentialError {
    /// An `env` credential's variable is unset.
    #[error("the variable {variable} is not set")]
    EnvUnset {
        /// The variable's name (the row's reference; not a secret).
        variable: String,
    },
    /// An AWS call failed. Only the error *code* is kept: the message can
    /// name the secret's ARN.
    #[error("{service} answered {code}")]
    Service {
        /// The AWS service.
        service: &'static str,
        /// The error code (`AccessDeniedException`, ...), or a fixed word
        /// for a failure that has none (`timeout`, `no-code`).
        code: String,
    },
    /// What was found has the wrong shape. Never carries the content.
    #[error("{what} is malformed")]
    Malformed {
        /// What is malformed, as a fixed phrase.
        what: &'static str,
    },
    /// A token could not be signed.
    #[error("an IAM token could not be signed: {reason}")]
    Signing {
        /// A fixed phrase.
        reason: &'static str,
    },
}

/// When a source's credential must be fetched again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refresh {
    /// Never: the value does not change while the server runs (`env`).
    Never,
    /// Only when a connection attempt fails authentication (a rotated
    /// secret).
    OnAuthFailure,
    /// On a timer, and also when a connection attempt fails authentication
    /// (an IAM token that expires).
    Every(Duration),
}

/// Something that yields a store's database login.
#[async_trait]
pub(super) trait CredentialSource: Send + Sync + std::fmt::Debug {
    /// Fetches the login now.
    ///
    /// # Errors
    /// [`CredentialError`], safe to log.
    async fn fetch(&self) -> Result<Credential, CredentialError>;

    /// When [`CredentialSource::fetch`] must be called again.
    fn refresh(&self) -> Refresh;
}

/// Reads one environment variable: `None` when it is unset.
pub(super) type Environment = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The `env` kind: the password is in a variable.
pub(super) struct EnvCredential {
    variable: String,
    environment: Environment,
}

impl std::fmt::Debug for EnvCredential {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "a Debug impl")
    )]
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EnvCredential")
            .field("variable", &self.variable)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl CredentialSource for EnvCredential {
    #[tracing::instrument(level = "debug", skip_all)]
    async fn fetch(&self) -> Result<Credential, CredentialError> {
        // The password comes from the named variable only: the account
        // database's password is never sent to another host.
        (self.environment)(&self.variable)
            .map(|password| Credential::new(None, password))
            .ok_or_else(|| CredentialError::EnvUnset {
                variable: self.variable.clone(),
            })
    }

    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "returns a constant")
    )]
    fn refresh(&self) -> Refresh {
        Refresh::Never
    }
}

/// Reads a secret's string value (the AWS client, mocked in tests).
#[async_trait]
pub(super) trait SecretReader: Send + Sync {
    /// The secret's string.
    ///
    /// # Arguments
    /// * `arn` - the secret's ARN.
    ///
    /// # Errors
    /// [`CredentialError`]; never holding the ARN or the value.
    async fn read(&self, arn: &str) -> Result<String, CredentialError>;
}

/// The `secrets-manager` kind: the reference is a secret's ARN.
pub(super) struct SecretsManagerCredential {
    arn: String,
    reader: Arc<dyn SecretReader>,
}

impl std::fmt::Debug for SecretsManagerCredential {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "a Debug impl")
    )]
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The ARN is withheld.
        formatter.write_str("SecretsManagerCredential")
    }
}

/// The JSON an RDS-managed secret holds. Other keys (`engine`, `host`, ...)
/// are ignored.
#[derive(Deserialize)]
struct SecretJson {
    username: String,
    password: String,
}

#[async_trait]
impl CredentialSource for SecretsManagerCredential {
    #[tracing::instrument(level = "debug", skip_all)]
    async fn fetch(&self) -> Result<Credential, CredentialError> {
        let text = self.reader.read(&self.arn).await?;
        // serde's error text can quote the value, so it is dropped here.
        let secret: SecretJson =
            serde_json::from_str(&text).map_err(|_| CredentialError::Malformed {
                what: "the secret's value",
            })?;
        Ok(Credential::new(Some(secret.username), secret.password))
    }

    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "returns a constant")
    )]
    fn refresh(&self) -> Refresh {
        Refresh::OnAuthFailure
    }
}

/// Signs an RDS IAM authentication token (the AWS signing, mocked in tests).
#[async_trait]
pub(super) trait TokenSigner: Send + Sync {
    /// A token for `user` on `host:port`, valid for 15 minutes.
    ///
    /// # Errors
    /// [`CredentialError`]; never holding the token.
    async fn token(&self, host: &str, port: u16, user: &str) -> Result<String, CredentialError>;
}

/// The `iam` kind: the reference is the database user.
pub(super) struct IamCredential {
    host: String,
    port: u16,
    user: String,
    refresh_every: Duration,
    signer: Arc<dyn TokenSigner>,
}

impl std::fmt::Debug for IamCredential {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "a Debug impl")
    )]
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("IamCredential")
    }
}

#[async_trait]
impl CredentialSource for IamCredential {
    #[tracing::instrument(level = "debug", skip_all)]
    async fn fetch(&self) -> Result<Credential, CredentialError> {
        let token = self.signer.token(&self.host, self.port, &self.user).await?;
        Ok(Credential::new(Some(self.user.clone()), token))
    }

    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "returns a field")
    )]
    fn refresh(&self) -> Refresh {
        Refresh::Every(self.refresh_every)
    }
}

/// Everything needed to turn a row's credential into a
/// [`CredentialSource`]: the environment and the two AWS clients.
#[derive(Clone)]
pub(super) struct Providers {
    environment: Environment,
    secrets: Arc<dyn SecretReader>,
    signer: Arc<dyn TokenSigner>,
    iam_refresh_every: Duration,
}

impl Providers {
    /// The real providers: the process's environment aside, the AWS clients
    /// load the SDK's configuration on first use, so a server with no
    /// `secrets-manager` or `iam` store never touches AWS.
    ///
    /// # Arguments
    /// * `environment` - reads a variable by name.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "builds a struct, no I/O")
    )]
    pub(super) fn aws(environment: Environment) -> Self {
        let sdk = Arc::new(LazySdk::default());
        Self::new(
            environment,
            Arc::new(AwsSecretReader {
                sdk: Arc::clone(&sdk),
            }),
            Arc::new(AwsTokenSigner { sdk }),
        )
    }

    /// Providers with the given AWS clients (tests pass mocks).
    ///
    /// # Arguments
    /// * `environment` - reads a variable by name.
    /// * `secrets` - reads a secret.
    /// * `signer` - signs an IAM token.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "builds a struct, no I/O")
    )]
    pub(super) fn new(
        environment: Environment,
        secrets: Arc<dyn SecretReader>,
        signer: Arc<dyn TokenSigner>,
    ) -> Self {
        Self {
            environment,
            secrets,
            signer,
            iam_refresh_every: IAM_REFRESH_EVERY,
        }
    }

    /// Tests: how often an IAM token is replaced.
    #[cfg(test)]
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "a test hook")
    )]
    pub(super) fn with_iam_refresh_every(mut self, every: Duration) -> Self {
        self.iam_refresh_every = every;
        self
    }

    /// The source for a `host` row's credential.
    ///
    /// # Arguments
    /// * `kind` - the row's `credential_kind`.
    /// * `reference` - the row's `credential_ref`.
    /// * `host`, `port` - the row's server (an IAM token is signed for it).
    ///
    /// # Returns
    /// The source.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "builds a struct, no I/O")
    )]
    pub(super) fn source(
        &self,
        kind: CredentialKind,
        reference: &str,
        host: &str,
        port: u16,
    ) -> Arc<dyn CredentialSource> {
        match kind {
            CredentialKind::Env => Arc::new(EnvCredential {
                variable: reference.to_string(),
                environment: Arc::clone(&self.environment),
            }),
            CredentialKind::SecretsManager => Arc::new(SecretsManagerCredential {
                arn: reference.to_string(),
                reader: Arc::clone(&self.secrets),
            }),
            CredentialKind::Iam => Arc::new(IamCredential {
                host: host.to_string(),
                port,
                user: reference.to_string(),
                refresh_every: self.iam_refresh_every,
                signer: Arc::clone(&self.signer),
            }),
        }
    }
}

/// Whether a driver error is the server refusing the login (SQLSTATE class
/// 28, "invalid authorization specification": a wrong or expired password
/// or token).
///
/// # Arguments
/// * `error` - what a connection attempt returned.
///
/// # Returns
/// `true` for a refused login.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "inspects a value, no I/O")
)]
pub(crate) fn is_auth_failure(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|database| database.code())
        .is_some_and(|code| code.starts_with("28"))
}

/// Carries each refused login, anywhere in the process, to the credential
/// tasks: the role the server named, if its message names one.
///
/// sqlx gives no hook on a pool's connect path, and most store reads reach
/// the pool through `CatalogDb::executor()` and propagate its error with `?`,
/// so the refusal is seen where every driver error passes: `CatalogDb::classify`
/// and `OkfError::from(sqlx::Error)` call [`signal_auth_refused`]. An error
/// does not say which pool it came from, so every task hears it and answers
/// only if the role is its pool's current login (or the message named none).
static LOGINS_REFUSED: LazyLock<broadcast::Sender<Option<String>>> =
    LazyLock::new(|| broadcast::channel(64).0);

/// The role a refusal message names, when it can only be that one: the text
/// between the message's only two `"` (Postgres writes `password
/// authentication failed for user "NAME"`). `None` otherwise, which every
/// credential task answers.
///
/// The server writes the name raw, not as an escaped identifier, so a name
/// holding a `"` cannot be told apart from the text around it. The rule is
/// therefore one-sided: a task stays asleep only when the message names a
/// single, quote-free, non-empty role that is not its own. Any other shape
/// (no quoted name, an unterminated or empty one, several, or a name with
/// quotation marks in it) wakes every task: a wasted fetch is cheap, while a
/// missed one leaves a `secrets-manager` pool, which has no timer, refused
/// for good.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "parses text, no I/O")
)]
fn role_in(message: &str) -> Option<&str> {
    let mut pieces = message.split('"');
    // `split` yields one piece more than there are quotation marks, so
    // exactly two marks means exactly three pieces.
    match (pieces.next(), pieces.next(), pieces.next(), pieces.next()) {
        (Some(_), Some(role), Some(_), None) if !role.is_empty() => Some(role),
        _ => None,
    }
}

/// Tells the credential tasks that a login was refused.
///
/// # Arguments
/// * `error` - the refusal (see [`is_auth_failure`]).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "sends a message, no I/O")
)]
pub(crate) fn signal_auth_refused(error: &sqlx::Error) {
    let role = error
        .as_database_error()
        .and_then(|database| role_in(database.message()))
        .map(str::to_owned);
    // No task listening (a pool on `env`) is not an error.
    let _ = LOGINS_REFUSED.send(role);
}

/// Waits until a login of `login` was refused (or the message named no
/// role, or messages were missed).
#[tracing::instrument(level = "trace", skip_all)]
async fn next_refusal(
    refused: &mut broadcast::Receiver<Option<String>>,
    login: &impl Fn() -> String,
) {
    loop {
        match refused.recv().await {
            Ok(Some(role)) if role != login() => {}
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => return,
            Err(broadcast::error::RecvError::Closed) => std::future::pending().await,
        }
    }
}

/// What keeps a pool's connect options current. Dropping it stops the
/// task, which releases the pool.
#[derive(Debug)]
pub(super) struct Keeper {
    // Dropped with the keeper: the task sees the channel close and ends.
    _stop: oneshot::Sender<()>,
    /// Counts the refreshes that replaced the options; closes when the task
    /// ends.
    refreshed: watch::Receiver<u64>,
}

impl Keeper {
    /// Watches the refresh count; the channel closes when the task ends. A
    /// store handle holds one so an operation refused its login can wait
    /// for the next refresh (`CatalogDb::retrying`).
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "one-line accessor")
    )]
    pub(super) fn watch(&self) -> watch::Receiver<u64> {
        self.refreshed.clone()
    }
}

/// Starts the task that keeps `pool`'s connect options current, if the
/// source ever changes.
///
/// # Arguments
/// * `source` - where a fresh login comes from.
/// * `base` - the options a login is applied to.
/// * `pool` - the pool whose options are replaced.
///
/// # Returns
/// The keeper, or `None` for a source that never changes (`env`).
#[tracing::instrument(level = "debug", skip_all)]
pub(super) fn spawn_keeper(
    source: Arc<dyn CredentialSource>,
    base: PgConnectOptions,
    pool: PgPool,
) -> Option<Keeper> {
    let every = match source.refresh() {
        Refresh::Never => return None,
        Refresh::OnAuthFailure => None,
        Refresh::Every(every) => Some(every),
    };
    let (stop, stopped) = oneshot::channel();
    let (count, refreshed) = watch::channel(0_u64);
    let current = pool.clone();
    let login = move || current.connect_options().get_username().to_string();
    let apply = move |credential: Credential| {
        pool.set_connect_options(credential.apply(base.clone()));
    };
    tokio::spawn(keep_current(
        source,
        every,
        LOGINS_REFUSED.subscribe(),
        login,
        stopped,
        count,
        apply,
    ));
    Some(Keeper {
        _stop: stop,
        refreshed,
    })
}

/// The keeper's loop: wait for the timer or an authentication failure,
/// fetch, apply, then leave a gap, until `stopped` closes.
///
/// # Arguments
/// * `source` - where a fresh login comes from.
/// * `every` - the timer, if any.
/// * `refused` - the refused logins ([`LOGINS_REFUSED`]).
/// * `login` - the role the pool connects as now.
/// * `stopped` - closes when the pool's registry entry is dropped.
/// * `count` - bumped after each login is applied.
/// * `apply` - installs a login.
#[tracing::instrument(level = "debug", skip_all)]
async fn keep_current(
    source: Arc<dyn CredentialSource>,
    every: Option<Duration>,
    mut refused: broadcast::Receiver<Option<String>>,
    login: impl Fn() -> String + Send,
    mut stopped: oneshot::Receiver<()>,
    count: watch::Sender<u64>,
    apply: impl Fn(Credential) + Send,
) {
    let mut wait = every;
    loop {
        let timer = async {
            match wait {
                Some(after) => tokio::time::sleep(after).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = &mut stopped => return,
            () = next_refusal(&mut refused, &login) => {}
            () = timer => {}
        }
        // Whatever was refused so far is answered by this fetch; a refusal
        // that arrives during it or the gap that follows wakes the next one.
        while refused.try_recv().is_ok() {}
        let fetched = tokio::select! {
            _ = &mut stopped => return,
            fetched = source.fetch() => fetched,
        };
        match fetched {
            Ok(credential) => {
                apply(credential);
                count.send_modify(|refreshes| *refreshes += 1);
                wait = every;
            }
            Err(error) => {
                split_event!(
                    warn,
                    {},
                    { %error, },
                    "a store's credential could not be refreshed"
                );
                wait =
                    Some(every.map_or(RETRY_AFTER_FAILURE, |every| every.min(RETRY_AFTER_FAILURE)));
            }
        }
        // However many attempts failed meanwhile, they are one refresh.
        tokio::select! {
            _ = &mut stopped => return,
            () = tokio::time::sleep(MIN_REFRESH_GAP) => {}
        }
    }
}

/// The SDK's configuration, loaded once, on first use.
#[derive(Default)]
struct LazySdk(OnceCell<SdkConfig>);

impl LazySdk {
    /// The configuration, from the SDK's standard chains (environment,
    /// profile, web identity, the container provider EKS Pod Identity
    /// uses): the chain the SES mailer reads.
    #[tracing::instrument(level = "debug", skip_all)]
    async fn get(&self) -> &SdkConfig {
        self.0
            .get_or_init(|| aws_config::defaults(BehaviorVersion::latest()).load())
            .await
    }
}

/// The AWS code of a failed call, or a fixed word when it has none.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "reads a field, no I/O")
)]
fn code_of(error: &impl aws_sdk_secretsmanager::error::ProvideErrorMetadata) -> String {
    error.code().unwrap_or("no-code").to_string()
}

/// [`SecretReader`] over AWS Secrets Manager.
struct AwsSecretReader {
    sdk: Arc<LazySdk>,
}

/// The region in a secret's ARN (`arn:aws:secretsmanager:REGION:...`).
///
/// # Arguments
/// * `arn` - the secret's ARN.
///
/// # Returns
/// The region, or `None` when `arn` is not a Secrets Manager ARN.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "parses a string, no I/O")
)]
fn region_of_arn(arn: &str) -> Option<&str> {
    let mut parts = arn.split(':');
    let region = (parts.next() == Some("arn"))
        .then(|| parts.nth(1))
        .flatten()
        .filter(|service| *service == "secretsmanager")
        .and_then(|_| parts.next())?;
    (!region.is_empty()).then_some(region)
}

#[async_trait]
impl SecretReader for AwsSecretReader {
    #[tracing::instrument(level = "debug", skip_all)]
    async fn read(&self, arn: &str) -> Result<String, CredentialError> {
        let region = region_of_arn(arn).ok_or(CredentialError::Malformed {
            what: "the secret reference",
        })?;
        let sdk = self.sdk.get().await;
        // The ARN names the secret's region, which need not be the
        // server's own.
        let config = aws_sdk_secretsmanager::config::Builder::from(sdk)
            .region(aws_config::Region::new(region.to_string()))
            .timeout_config(
                aws_config::timeout::TimeoutConfig::builder()
                    .operation_timeout(AWS_CALL_TIMEOUT)
                    .build(),
            )
            .build();
        let client = aws_sdk_secretsmanager::Client::from_conf(config);
        let answer = client
            .get_secret_value()
            .secret_id(arn)
            .send()
            .await
            .map_err(|error| CredentialError::Service {
                service: "secrets manager",
                code: code_of(&error),
            })?;
        answer
            .secret_string()
            .map(str::to_string)
            .ok_or(CredentialError::Malformed {
                what: "the secret's value",
            })
    }
}

/// [`TokenSigner`] over the SDK's credentials.
struct AwsTokenSigner {
    sdk: Arc<LazySdk>,
}

#[async_trait]
impl TokenSigner for AwsTokenSigner {
    #[tracing::instrument(level = "debug", skip_all)]
    async fn token(&self, host: &str, port: u16, user: &str) -> Result<String, CredentialError> {
        let sdk = self.sdk.get().await;
        let provider = sdk.credentials_provider().ok_or(CredentialError::Signing {
            reason: "the server has no AWS credentials",
        })?;
        let region = signing_region(host, sdk.region().map(AsRef::as_ref)).ok_or(
            CredentialError::Signing {
                reason: "no AWS region is configured",
            },
        )?;
        let credentials = tokio::time::timeout(AWS_CALL_TIMEOUT, provider.provide_credentials())
            .await
            .map_err(|_| CredentialError::Service {
                service: "the credential chain",
                code: "timeout".to_string(),
            })?
            .map_err(|_| CredentialError::Service {
                service: "the credential chain",
                code: "no-credentials".to_string(),
            })?;
        presign_token(
            &Identity::from(credentials),
            region,
            host,
            port,
            user,
            SystemTime::now(),
        )
    }
}

/// The region a token for `host` is signed for: the database's, read from a
/// recognised RDS host name, because RDS rejects a token signed for another
/// region; the server's own region only for a host name that does not say
/// (a proxy, a CNAME).
///
/// # Arguments
/// * `host` - the row's host.
/// * `server_region` - the region of the server's AWS configuration.
///
/// # Returns
/// The region, or `None` when neither source has one.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "picks a string, no I/O")
)]
fn signing_region<'a>(host: &'a str, server_region: Option<&'a str>) -> Option<&'a str> {
    region_of_rds_host(host).or(server_region)
}

/// The region in an RDS host name (`name.id.REGION.rds.amazonaws.com`).
///
/// # Arguments
/// * `host` - the row's host.
///
/// # Returns
/// The region, if the name has the RDS shape.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "parses a string, no I/O")
)]
fn region_of_rds_host(host: &str) -> Option<&str> {
    let before_rds = host.strip_suffix(".rds.amazonaws.com")?;
    before_rds
        .rsplit('.')
        .next()
        .filter(|part| !part.is_empty())
}

/// Percent-encodes everything outside the unreserved characters, as a
/// SigV4 query value needs.
///
/// # Arguments
/// * `text` - the value.
///
/// # Returns
/// The encoded value.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "encodes a string, no I/O")
)]
fn encode_query_value(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// The error for any failure of the signing library, whose own errors are
/// not kept (they can quote the request).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(uninstrumented_fn, reason = "builds a fixed error")
)]
fn unsigned<E>(_: E) -> CredentialError {
    CredentialError::Signing {
        reason: "the request could not be signed",
    }
}

/// Signs an RDS IAM authentication token: the SigV4-presigned URL
/// `https://HOST:PORT/?Action=connect&DBUser=USER` for service `rds-db`,
/// without its scheme, valid 15 minutes.
///
/// # Arguments
/// * `identity` - the AWS credentials to sign with.
/// * `region` - the database's region.
/// * `host`, `port`, `user` - the database server and its IAM-enabled user.
/// * `time` - the signing time (a parameter so a test can fix it).
///
/// # Returns
/// The token.
///
/// # Errors
/// [`CredentialError::Signing`] if the request cannot be signed.
#[tracing::instrument(level = "debug", skip_all)]
fn presign_token(
    identity: &Identity,
    region: &str,
    host: &str,
    port: u16,
    user: &str,
    time: SystemTime,
) -> Result<String, CredentialError> {
    let mut settings = SigningSettings::default();
    settings.signature_location = SignatureLocation::QueryParams;
    settings.expires_in = Some(IAM_TOKEN_LIFETIME);
    let params = v4::SigningParams::builder()
        .identity(identity)
        .region(region)
        .name("rds-db")
        .time(time)
        .settings(settings)
        .build()
        .map_err(unsigned)?;
    let url = format!(
        "https://{host}:{port}/?Action=connect&DBUser={}",
        encode_query_value(user)
    );
    let request = SignableRequest::new(
        "GET",
        url.as_str(),
        std::iter::empty(),
        SignableBody::Bytes(&[]),
    )
    .map_err(unsigned)?;
    let (instructions, _signature) = sign(request, &params.into())
        .map_err(unsigned)?
        .into_parts();
    let mut signed = http::Request::builder()
        .uri(url.as_str())
        .body(())
        .map_err(|_| CredentialError::Signing {
            reason: "the request could not be built",
        })?;
    instructions.apply_to_request_http1x(&mut signed);
    let signed_url = signed.uri().to_string();
    signed_url
        .strip_prefix("https://")
        .map(str::to_string)
        .ok_or(CredentialError::Signing {
            reason: "the request could not be built",
        })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;

    /// A secret reader that answers with a fixed string and counts calls.
    struct FixedSecret {
        answer: Result<String, CredentialError>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl SecretReader for FixedSecret {
        async fn read(&self, _arn: &str) -> Result<String, CredentialError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.answer {
                Ok(text) => Ok(text.clone()),
                Err(CredentialError::Service { service, code }) => Err(CredentialError::Service {
                    service,
                    code: code.clone(),
                }),
                Err(_) => Err(CredentialError::Malformed { what: "the mock" }),
            }
        }
    }

    fn no_environment() -> Environment {
        Arc::new(|_| None)
    }

    struct NeverSigns;

    #[async_trait]
    impl TokenSigner for NeverSigns {
        async fn token(&self, _: &str, _: u16, _: &str) -> Result<String, CredentialError> {
            Err(CredentialError::Signing { reason: "a mock" })
        }
    }

    fn providers_with_secret(answer: Result<String, CredentialError>) -> Providers {
        Providers::new(
            no_environment(),
            Arc::new(FixedSecret {
                answer,
                calls: AtomicUsize::new(0),
            }),
            Arc::new(NeverSigns),
        )
    }

    #[tokio::test]
    async fn secrets_manager_credential_is_read_from_the_secret_json() {
        let secret = r#"{"username":"okf_app","password":"pw-1","engine":"postgres","port":5432}"#;
        let providers = providers_with_secret(Ok(secret.to_string()));
        let source = providers.source(
            CredentialKind::SecretsManager,
            "arn:aws:secretsmanager:us-east-1:123456789012:secret:example",
            "db.example.internal",
            5432,
        );

        let credential = source.fetch().await.unwrap();

        assert_eq!(
            credential,
            Credential::new(Some("okf_app".to_string()), "pw-1".to_string())
        );
        let options = credential.apply("postgres://base@localhost/db".parse().unwrap());
        assert_eq!(options.get_username(), "okf_app");
        assert_eq!(source.refresh(), Refresh::OnAuthFailure);
        // The credential prints no value.
        assert!(!format!("{credential:?}").contains("pw-1"));
    }

    #[tokio::test]
    async fn a_malformed_secret_is_an_error_that_quotes_nothing() {
        for body in [
            "not json at all sentinel-1",
            r#"{"username":"u"}"#,
            r#"{"username":"u","password":12345}"#,
            r#"["sentinel-2"]"#,
        ] {
            let providers = providers_with_secret(Ok(body.to_string()));
            let source = providers.source(CredentialKind::SecretsManager, "arn", "h", 1);

            let error = source.fetch().await.unwrap_err();

            assert_eq!(
                error,
                CredentialError::Malformed {
                    what: "the secret's value"
                }
            );
            let shown = format!("{error} {error:?}");
            assert!(
                !shown.contains("sentinel") && !shown.contains("12345"),
                "{shown}"
            );
        }
    }

    #[tokio::test]
    async fn env_credential_reads_the_variable_and_keeps_the_base_user() {
        let providers = Providers::new(
            Arc::new(|name| (name == "PW").then(|| "secret-pw".to_string())),
            Arc::new(FixedSecret {
                answer: Ok(String::new()),
                calls: AtomicUsize::new(0),
            }),
            Arc::new(NeverSigns),
        );

        let found = providers
            .source(CredentialKind::Env, "PW", "h", 1)
            .fetch()
            .await
            .unwrap();
        let unset = providers
            .source(CredentialKind::Env, "OTHER", "h", 1)
            .fetch()
            .await
            .unwrap_err();

        assert_eq!(found, Credential::new(None, "secret-pw".to_string()));
        let options = found.apply("postgres://base@localhost/db".parse().unwrap());
        assert_eq!(options.get_username(), "base");
        assert_eq!(
            unset,
            CredentialError::EnvUnset {
                variable: "OTHER".to_string()
            }
        );
        assert_eq!(
            providers
                .source(CredentialKind::Env, "PW", "h", 1)
                .refresh(),
            Refresh::Never
        );
    }

    #[test]
    fn a_role_is_read_from_a_refusal_message_or_left_unknown() {
        let said = |name: &str| format!("password authentication failed for user \"{name}\"");
        assert_eq!(role_in(&said("app")), Some("app"));
        // Postgres writes the name raw, so a name holding quotation marks,
        // one or two adjacent, cannot be read back: unknown.
        assert_eq!(role_in(&said("app\"reader")), None);
        assert_eq!(role_in(&said("app\"\"reader")), None);
        assert_eq!(role_in(&said("")), None);
        assert_eq!(role_in("no quoted name here"), None);
        assert_eq!(role_in("an unterminated \"name"), None);
        // Several quoted names (pg_hba.conf's refusal): unknown.
        assert_eq!(
            role_in("no pg_hba.conf entry for host \"h\", user \"app\", database \"d\""),
            None
        );
    }

    #[test]
    fn a_refusal_wakes_every_task_unless_it_names_another_role() {
        let wakes = |message: &str, login: &str| {
            let (sender, mut refused) = broadcast::channel(4);
            sender.send(role_in(message).map(str::to_owned)).unwrap();
            drop(sender);
            let login = || login.to_string();
            // A refusal that does not wake the task leaves it waiting on a
            // closed channel, which never resolves.
            futures_util::FutureExt::now_or_never(next_refusal(&mut refused, &login)).is_some()
        };
        let said = |name: &str| format!("password authentication failed for user \"{name}\"");
        assert!(wakes(&said("app"), "app"));
        assert!(!wakes(&said("other"), "app"));
        assert!(wakes(&said("app\"\"reader"), "app\"\"reader"));
        assert!(wakes(&said("app\"reader"), "app\"reader"));
        assert!(wakes("an unterminated \"app", "app"));
        assert!(wakes("no quoted name here", "app"));
    }

    #[test]
    fn region_is_read_from_a_secret_arn_and_an_rds_host() {
        let arn = "arn:aws:secretsmanager:eu-west-2:123456789012:secret:example";
        assert_eq!(region_of_arn(arn), Some("eu-west-2"));
        assert_eq!(region_of_arn("arn:aws:s3:::example"), None);
        assert_eq!(region_of_arn("arn:aws:secretsmanager::1:secret:x"), None);
        assert_eq!(region_of_arn("not-an-arn"), None);
        assert_eq!(
            region_of_rds_host("example.abcdefgh.us-east-1.rds.amazonaws.com"),
            Some("us-east-1")
        );
        assert_eq!(region_of_rds_host("db.example.internal"), None);
    }

    #[test]
    fn a_token_is_signed_for_the_databases_region_not_the_servers() {
        let server = Some("us-east-1");
        // The host names another region: it wins.
        assert_eq!(
            signing_region("example.abcdefgh.eu-west-2.rds.amazonaws.com", server),
            Some("eu-west-2")
        );
        // A name that does not say falls back to the server's region.
        assert_eq!(
            signing_region("db.example.internal", server),
            Some("us-east-1")
        );
        assert_eq!(signing_region("db.example.internal", None), None);
    }

    #[test]
    fn an_iam_token_is_a_presigned_connect_url_for_the_user() {
        let credentials = aws_credential_types::Credentials::new(
            "AKIDEXAMPLE",
            "secret-key-never-in-the-token",
            None,
            None,
            "test",
        );
        let identity = Identity::from(credentials);
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);

        let token = presign_token(
            &identity,
            "us-east-1",
            "db.example.internal",
            5432,
            "okf app",
            at,
        )
        .unwrap();

        assert!(
            token.starts_with("db.example.internal:5432/?Action=connect&DBUser=okf%20app&"),
            "{token}"
        );
        assert!(token.contains("X-Amz-Expires=900"), "{token}");
        assert!(
            token.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"),
            "{token}"
        );
        assert!(token.contains("rds-db%2Faws4_request"), "{token}");
        assert!(token.contains("X-Amz-Signature="), "{token}");
        assert!(!token.contains("secret-key-never-in-the-token"));
        // Signing is deterministic for a fixed time.
        assert_eq!(
            token,
            presign_token(
                &identity,
                "us-east-1",
                "db.example.internal",
                5432,
                "okf app",
                at
            )
            .unwrap()
        );
    }

    /// A source whose login changes each fetch, and that can fail.
    struct Counting {
        fetches: AtomicU64,
        fail: Mutex<bool>,
        refresh: Refresh,
    }

    impl std::fmt::Debug for Counting {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("Counting")
        }
    }

    #[async_trait]
    impl CredentialSource for Counting {
        async fn fetch(&self) -> Result<Credential, CredentialError> {
            let nth = self.fetches.fetch_add(1, Ordering::SeqCst) + 1;
            if *self.fail.lock().unwrap() {
                return Err(CredentialError::Malformed { what: "a mock" });
            }
            Ok(Credential::new(None, format!("pw-{nth}")))
        }

        fn refresh(&self) -> Refresh {
            self.refresh
        }
    }

    /// Runs `keep_current` on the paused clock, collecting each applied
    /// password. Returns what the test needs to drive it.
    struct Harness {
        source: Arc<Counting>,
        applied: Arc<Mutex<Vec<String>>>,
        refused: broadcast::Sender<Option<String>>,
        stop: Option<oneshot::Sender<()>>,
        refreshed: watch::Receiver<u64>,
        task: tokio::task::JoinHandle<()>,
    }

    fn start(refresh: Refresh) -> Harness {
        let source = Arc::new(Counting {
            fetches: AtomicU64::new(0),
            fail: Mutex::new(false),
            refresh,
        });
        let applied = Arc::new(Mutex::new(Vec::new()));
        let (refused, refusals) = broadcast::channel(8);
        let (stop, stopped) = oneshot::channel();
        let (count, refreshed) = watch::channel(0_u64);
        let sink = Arc::clone(&applied);
        let every = match refresh {
            Refresh::Every(every) => Some(every),
            _ => None,
        };
        let task = tokio::spawn(keep_current(
            Arc::clone(&source) as Arc<dyn CredentialSource>,
            every,
            refusals,
            || "example_role".to_string(),
            stopped,
            count,
            move |credential| sink.lock().unwrap().push(credential.password),
        ));
        Harness {
            source,
            applied,
            refused,
            stop: Some(stop),
            refreshed,
            task,
        }
    }

    impl Harness {
        /// Waits for the refresh count to reach `target`.
        async fn refreshes(&mut self, target: u64) {
            self.refreshed
                .wait_for(|count| *count >= target)
                .await
                .unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_keeper_replaces_the_login_on_its_timer_and_stops_when_dropped() {
        let mut keeper = start(Refresh::Every(Duration::from_secs(600)));

        // The paused clock moves to the timer when everything is idle.
        keeper.refreshes(1).await;
        keeper.refreshes(2).await;

        assert_eq!(*keeper.applied.lock().unwrap(), ["pw-1", "pw-2"]);
        drop(keeper.stop.take());
        keeper.task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn an_auth_failure_refreshes_once_however_many_attempts_failed() {
        let mut keeper = start(Refresh::OnAuthFailure);

        // Three failed attempts before the keeper runs are one signal.
        keeper.refused.send(None).unwrap();
        keeper.refused.send(None).unwrap();
        keeper.refused.send(None).unwrap();
        keeper.refreshes(1).await;
        // Further failures wait out the minimum gap, then refresh once more.
        keeper.refused.send(None).unwrap();
        keeper.refreshes(2).await;

        assert_eq!(keeper.source.fetches.load(Ordering::SeqCst), 2);
        assert_eq!(*keeper.applied.lock().unwrap(), ["pw-1", "pw-2"]);
        drop(keeper.stop.take());
        keeper.task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_refresh_keeps_the_old_login_and_is_retried() {
        let mut keeper = start(Refresh::Every(Duration::from_secs(600)));
        keeper.refreshes(1).await;
        *keeper.source.fail.lock().unwrap() = true;
        // Fails on the next timer; nothing is applied.
        tokio::time::sleep(Duration::from_secs(601)).await;
        assert_eq!(keeper.applied.lock().unwrap().len(), 1);

        *keeper.source.fail.lock().unwrap() = false;
        keeper.refreshes(2).await;

        assert_eq!(keeper.applied.lock().unwrap().len(), 2);
        drop(keeper.stop.take());
        keeper.task.await.unwrap();
    }

    #[tokio::test]
    async fn a_source_that_never_changes_has_no_keeper() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://u:p@127.0.0.1:1/x")
            .unwrap();
        let source = Arc::new(Counting {
            fetches: AtomicU64::new(0),
            fail: Mutex::new(false),
            refresh: Refresh::Never,
        });

        let keeper = spawn_keeper(source, "postgres://u@127.0.0.1:1/x".parse().unwrap(), pool);

        assert!(keeper.is_none());
    }

    #[test]
    fn auth_failures_are_recognised_by_sqlstate_class() {
        assert!(!is_auth_failure(&sqlx::Error::PoolTimedOut));
        assert!(!is_auth_failure(&sqlx::Error::RowNotFound));
    }
}
