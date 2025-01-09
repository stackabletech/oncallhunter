use crate::config::ConfigError::{ParseBindAddress, ParseBool, ParsePort};
use crate::{jira, twilio};
use hyper::header::{HeaderValue, InvalidHeaderValue};
use secrecy::{CloneableSecret, DebugSecret, Secret, SecretString, Zeroize};
use snafu::{OptionExt, ResultExt, Snafu};
use std::env;
use std::env::VarError;
use std::ffi::OsString;
use std::fmt::Debug;
use std::net::{AddrParseError, IpAddr, Ipv4Addr};
use std::num::ParseIntError;
use std::str::{FromStr, ParseBoolError};
use tracing::instrument;
use url::Url;

static TRACE_EXPORTER_ENVNAME: &str = "WYGC_ENABLE_TRACE_EXPORT";
static TRACE_EXPORTER_DEFAULT: bool = false;

static LOG_EXPORTER_ENVNAME: &str = "WYGC_ENABLE_LOG_EXPORT";
static LOG_EXPORTER_DEFAULT: bool = false;

static BIND_ADDRESS_ENVNAME: &str = "WYGC_BIND_ADDRESS";
static BIND_ADDRESS_DEFAULT: &str = "0.0.0.0";

static BIND_PORT_ENVNAME: &str = "WYGC_BIND_PORT";
static BIND_PORT_DEFAULT: &str = "2368";

static TWILIO_TOKEN_ENVNAME: &str = "WYGC_TWILIO_TOKEN";
static TWILIO_BASEURL_ENVNAME: &str = "WYGC_TWILIO_BASEURL";
static TWILIO_BASEURL_DEFAULT: &str = "https://studio.twilio.com/v2/Flows/";
static TWILIO_WORKFLOW_ENVNAME: &str = "WYGC_TWILIO_WORKFLOW";
static TWILIO_OUTGOING_NUMBER_ENVNAME: &str = "WYGC_TWILIO_OUTNUMBER";

static JIRA_USER_ENVNAME: &str = "WYGC_JIRA_USER";
static JIRA_PASSWORD_ENVNAME: &str = "WYGC_JIRA_PASSWORD";
static JIRA_BASEURL_ENVNAME: &str = "WYGC_JIRA_BASEURL";
static JIRA_BASEURL_DEFAULT: &str =
    "https://api.atlassian.com/jsm/ops/api/efb357c6-07f6-478f-99b7-fd594c010350/v1/";

static SLACK_TOKEN_ENVNAME: &str = "WYGC_SLACK_TOKEN";
static SLACK_BASEURL_ENVNAME: &str = "WYGC_SLACK_BASEURL";

// Create our own secrecy wrapper around HeaderValue in order to avoid logging any
// confidential values in tracing spans
// The Benefit of doing it here instead of storing as a string here and parsing later is that we
// can fail early on startup if illegal values are configured instead of starting up and having
// to log an error for every request.
#[derive(Clone)]
pub struct AuthHeader(pub HeaderValue);

impl Zeroize for AuthHeader {
    fn zeroize(&mut self) {
        // TODO: not sure how to handle this, currently we don't securely overwrite in memory
        //  but the trait needs to be implemented to be able to implement CloneableSecret
    }
}

impl CloneableSecret for AuthHeader {}

/// Provides a `Debug` impl (by default `[[REDACTED]]`)
impl DebugSecret for AuthHeader {}

/// Use this alias when storing secret values
pub type SecretAuthHeader = Secret<AuthHeader>;

#[derive(Snafu, Debug)]
pub enum ConfigError {
    #[snafu(display("failed to parse a valid ipv4 address from [{envname}]: \n{source}"))]
    ParseBindAddress {
        source: AddrParseError,
        envname: String,
    },

    #[snafu(display("failed to read value of [{envname}] env var as string"))]
    ConvertOsString { envname: String },

    #[snafu(display("missing mandatory configuration [{envname}]"))]
    MissingRequiredValue { envname: String },

    #[snafu(display("baseurl parse error for service [{service}]: \n{source}"))]
    ConstructBaseUrl {
        source: url::ParseError,
        service: String,
    },
    #[snafu(display("unable to parse authz header value from [{envname}]: \n{source}"))]
    ConstructAuthHeader {
        source: InvalidHeaderValue,
        envname: String,
    },
    #[snafu(display("failed to parse port number for [{envname}]: \n{source}"))]
    ParsePort {
        source: ParseIntError,
        envname: String,
    },
    #[snafu(display("failed to parse boolean value for [{envname}]: \n{source}"))]
    ParseBool {
        source: ParseBoolError,
        envname: String,
    },
    #[snafu(display("failed to parse boolean value for [{envname}]: \n{source}"))]
    ConvertEnvString { source: VarError, envname: String },
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_address: IpAddr,
    pub bind_port: u16,

    pub opsgenie_config: JiraConfig,
    pub twilio_config: TwilioConfig,

    pub slack_config: Option<SlackConfig>,
}

#[derive(Debug, Clone)]
pub struct SlackConfig {
    pub url: Url,
    pub token: SecretAuthHeader,
}

#[derive(Debug, Clone)]
pub struct JiraConfig {
    pub base_url: Url,
    pub username: String,
    pub password: SecretString,
}

#[derive(Debug, Clone)]
pub struct TwilioConfig {
    pub base_url: Url,
    pub credentials: SecretAuthHeader,
    pub workflow_id: String,
    pub outgoing_number: String,
}

impl Config {
    #[instrument(name = "parse_config")]
    pub fn new() -> Result<Self, ConfigError> {
        // Determine address and port to listen on from env vars, use default values if not set
        // TODO: Pretty sure this is a garbage way of doing this, need to look at it some more
        //  `into_string()` probably is the way to go here ..
        let bind_address = env::var_os(BIND_ADDRESS_ENVNAME)
            .unwrap_or(OsString::from(BIND_ADDRESS_DEFAULT))
            .to_str()
            .context(ConvertOsStringSnafu {
                envname: BIND_ADDRESS_DEFAULT,
            })?
            .parse::<Ipv4Addr>()
            .context(ParseBindAddressSnafu {
                envname: BIND_ADDRESS_ENVNAME,
            })?;
        tracing::trace!(?bind_address, "Bind address set");

        let bind_port = u16::from_str(
            env::var_os(BIND_PORT_ENVNAME)
                .unwrap_or(OsString::from(BIND_PORT_DEFAULT))
                .to_str()
                .context(ConvertOsStringSnafu {
                    envname: BIND_PORT_DEFAULT,
                })?,
        )
        .context(ParsePortSnafu {
            envname: BIND_PORT_ENVNAME,
        })?;
        tracing::debug!(bind_port, "Bind port set");

        let twilio_config = TwilioConfig::new()?;
        let opsgenie_config = JiraConfig::new()?;

        // Attempt to parse SlackConfig, if no webhook is configured log a warning and continue,
        // if we encounter an actual error, abort startup
        let slack_config = SlackConfig::new()?;

        // Put it all together into a filled config object
        Ok(Config {
            bind_address: bind_address.into(),
            bind_port,
            opsgenie_config,
            twilio_config,
            slack_config,
        })
    }
}

impl JiraConfig {
    pub fn new() -> Result<Self, ConfigError> {
        // Parse OpsGenie specific configuration values from environment
        // TODO: the default should be in this module I guess..
        let base_url = Url::parse(
            env::var_os(JIRA_BASEURL_ENVNAME)
                .unwrap_or(OsString::from(JIRA_BASEURL_DEFAULT))
                .to_str()
                .context(ConvertOsStringSnafu {
                    envname: JIRA_BASEURL_ENVNAME,
                })?,
        )
        .context(ConstructBaseUrlSnafu {
            service: "OpsGenie",
        })?;

        tracing::debug!("OpsGenie base url parsed as : [{}]", base_url.to_string());

        let username = env::var_os(JIRA_USER_ENVNAME)
            .context(MissingRequiredValueSnafu {
                envname: JIRA_USER_ENVNAME,
            })?
            .to_str()
            .context(ConvertOsStringSnafu {
                envname: JIRA_USER_ENVNAME,
            })?
            .to_string();

        let password = SecretString::new(
            env::var_os(JIRA_PASSWORD_ENVNAME)
                .context(MissingRequiredValueSnafu {
                    envname: JIRA_PASSWORD_ENVNAME,
                })?
                .to_str()
                .context(ConvertOsStringSnafu {
                    envname: JIRA_PASSWORD_ENVNAME,
                })?
                .to_string(),
        );

        Ok(JiraConfig {
            base_url,
            username,
            password,
        })
    }
}

impl TwilioConfig {
    pub fn new() -> Result<Self, ConfigError> {
        // Parse Twilio specific configuration values from environment
        // TODO: the default should be in this module I guess..
        let base_url = Url::parse(
            env::var_os(TWILIO_BASEURL_ENVNAME)
                .unwrap_or(OsString::from(TWILIO_BASEURL_DEFAULT))
                .to_str()
                .context(ConvertOsStringSnafu {
                    envname: TWILIO_BASEURL_ENVNAME,
                })?,
        )
        .context(ConstructBaseUrlSnafu { service: "Twilio" })?;

        tracing::debug!("Twilio base url parsed as : [{}]", base_url.to_string());

        let credentials = get_secret_header_from_env(TWILIO_TOKEN_ENVNAME)?;

        let workflow_id = env::var_os(TWILIO_WORKFLOW_ENVNAME)
            .context(MissingRequiredValueSnafu {
                envname: TWILIO_WORKFLOW_ENVNAME,
            })?
            .to_str()
            .context(ConvertOsStringSnafu {
                envname: TWILIO_WORKFLOW_ENVNAME,
            })?
            .to_string();

        let outgoing_number = env::var_os(TWILIO_OUTGOING_NUMBER_ENVNAME)
            .context(MissingRequiredValueSnafu {
                envname: TWILIO_OUTGOING_NUMBER_ENVNAME,
            })?
            .to_str()
            .context(ConvertOsStringSnafu {
                envname: TWILIO_OUTGOING_NUMBER_ENVNAME,
            })?
            .to_string();

        Ok(TwilioConfig {
            base_url,
            credentials,
            workflow_id,
            outgoing_number,
        })
    }
}

impl SlackConfig {
    pub fn new() -> Result<Option<Self>, ConfigError> {
        // We try to parse the Slack Webhook url first, if that is not present, no harm done - we log
        // that we won't alert on Slack and go on our merry way
        // If the webhook is present but the token is missing that is not good, and we'll error out
        // with a "missing mandatory value" error, as we cannot call the webhook without a token

        if let Some(var_value) = env::var_os(SLACK_BASEURL_ENVNAME) {
            let url = Url::parse(var_value.to_str().context(ConvertOsStringSnafu {
                envname: SLACK_BASEURL_ENVNAME,
            })?)
            .context(ConstructBaseUrlSnafu { service: "slack" })?;

            let token = get_secret_header_from_env(SLACK_TOKEN_ENVNAME)?;

            Ok(Some(SlackConfig { url, token }))
        } else {
            // Variable is not set, we'll continue without Slack notifications
            tracing::warn!(
                "[{SLACK_BASEURL_ENVNAME}] not set, Slack notifications will be disabled!"
            );
            Ok(None)
        }
    }
}

fn get_secret_header_from_env(envname: &str) -> Result<SecretAuthHeader, ConfigError> {
    Ok(SecretAuthHeader::new(AuthHeader(
        HeaderValue::from_str(
            env::var_os(envname)
                .context(MissingRequiredValueSnafu { envname })?
                .to_str()
                .context(ConvertOsStringSnafu { envname })?,
        )
        .context(ConstructAuthHeaderSnafu { envname })?,
    )))
}

pub fn enable_trace_exporter() -> Result<bool, ConfigError> {
    extract_env_as_bool(TRACE_EXPORTER_ENVNAME, TRACE_EXPORTER_DEFAULT)
}

pub fn enable_log_exporter() -> Result<bool, ConfigError> {
    extract_env_as_bool(LOG_EXPORTER_ENVNAME, LOG_EXPORTER_DEFAULT)
}

fn extract_env_as_bool(envname: impl AsRef<str>, default: bool) -> Result<bool, ConfigError> {
    match env::var(envname.as_ref()) {
        Ok(value) => Ok(bool::from_str(&value).context(ParseBoolSnafu {
            envname: envname.as_ref(),
        })?),
        Err(e) if e == VarError::NotPresent => Ok(default),
        Err(e) => Err(e).context(ConvertEnvStringSnafu {
            envname: envname.as_ref(),
        }),
    }
}
