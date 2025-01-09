use crate::config::{Config, JiraConfig};
use crate::jira::error::{
    NoOnCallPersonSnafu, NoPhoneNumberSnafu, RequestOnCallPersonSnafu,
    RequestPhoneNumberForPersonSnafu, RequestScheduleSnafu, ScheduleNotFoundSnafu,
    TooManySchedulesFoundSnafu,
};
use crate::jira::Error::{ScheduleNotFound, TooManySchedulesFound};
use crate::util::send_json_request;
use crate::{http_error, AlertInfo, ScheduleIdentifier, ScheduleRequestById};
use axum::http::{HeaderMap, StatusCode};
use hyper::header::AUTHORIZATION;
use reqwest::{Client, Url};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use snafu::{ensure, OptionExt, ResultExt, Snafu};

#[derive(Snafu, Debug)]
#[snafu(module)]
pub(crate) enum Error {
    #[snafu(display("requesting on call person failed: \n{source}"))]
    RequestOnCallPerson { source: crate::util::Error },
    #[snafu(display("requesting schedule by name failed: \n{source}"))]
    RequestSchedule { source: crate::util::Error },
    #[snafu(display("requesting phone number failed for [{username}]: \n{source}"))]
    RequestPhoneNumberForPerson {
        source: crate::util::Error,
        username: String,
    },
    #[snafu(display("Jira says no one is currently on call!"))]
    NoOnCallPerson {},
    #[snafu(display("Jira doesn't have a schedule with the name [{schedule_name}]!"))]
    ScheduleNotFound { schedule_name: String },
    #[snafu(display("Expected to find exactly one schedule for the name [{schedule_name}] in Jira, but got [{schedules_found}] instead!"))]
    TooManySchedulesFound {
        schedule_name: String,
        schedules_found: usize,
    },
    #[snafu(display("User [{username}] has no phone number configured!"))]
    NoPhoneNumber { username: String },
}

impl http_error::Error for Error {
    fn status_code(&self) -> StatusCode {
        match self {
            Error::RequestOnCallPerson { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            Error::NoOnCallPerson { .. } => StatusCode::IM_A_TEAPOT,
            Error::NoPhoneNumber { .. } => StatusCode::IM_A_TEAPOT,
            Error::RequestPhoneNumberForPerson { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            Error::RequestSchedule { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            ScheduleNotFound { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            TooManySchedulesFound { .. } => StatusCode::UNPROCESSABLE_ENTITY,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    id: String,
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub struct UserPhoneNumber {
    pub name: String,
    pub phone: Vec<String>,
}

#[derive(Clone, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct OnCallResult {
    data: OnCallResultData,
}

#[derive(Clone, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct OnCallResultData {
    on_call_recipients: Vec<String>,
}

pub(crate) async fn get_schedule_id_by_name(
    schedule_name: &String,
    http: &Client,
    jira_config: &JiraConfig,
) -> Result<String, Error> {
    let mut url_builder = jira_config.base_url.clone();
    url_builder = url_builder.join(&format!("schedules")).unwrap();

    let schedules = send_json_request::<Vec<Schedule>>(
        http.get(url_builder.clone())
            .query(&[("query", schedule_name)]),
    )
    .await
    .context(RequestScheduleSnafu)?;

    // Stop early, if we get more than one schedule back for our query
    ensure!(
        schedules.len() == 1,
        TooManySchedulesFoundSnafu {
            schedule_name,
            schedules_found: schedules.len()
        }
    );

    // Attempt to retrieve the first element from the result list, if this fails the list
    // was empty, so we found no schedule and return with an error
    let schedule = schedules.get(0).context(ScheduleNotFoundSnafu {
        schedule_name: schedule_name.clone(),
    })?;

    Ok(schedule.id.clone())
}

pub(crate) async fn get_oncall_number(
    schedule: &ScheduleIdentifier,
    http: &Client,
    config: &Config,
) -> Result<AlertInfo, Error> {
    let Config {
        opsgenie_config,
        slack_config,
        ..
    } = config;
    let mut url_builder = opsgenie_config.base_url.clone();

    let schedule_id = match schedule {
        ScheduleIdentifier::ScheduleById(id) => id.id.clone(),
        ScheduleIdentifier::ScheduleByName(name) => {
            get_schedule_id_by_name(&name.name, &http, opsgenie_config).await?
        }
    };

    url_builder = url_builder
        .join(&format!("schedules/{schedule_id}/on-calls"))
        .unwrap();

    tracing::debug!(
        "Retrieving on call person from [{}]",
        url_builder.to_string()
    );

    let persons_on_call =
        send_json_request::<OnCallResult>(http.get(url_builder.clone()).query(&[("flat", "true")]))
            .await
            .context(RequestOnCallPersonSnafu)?;

    // We don't need this value, this is just to check the response wasn't empty and no one is
    // on call
    persons_on_call
        .data
        .on_call_recipients
        .get(0)
        .context(NoOnCallPersonSnafu)?;

    let mut result_list: Vec<UserPhoneNumber> = Vec::new();

    for user in persons_on_call.data.on_call_recipients {
        tracing::debug!(user, "Looking up phone number");
        let phone_number = get_phone_number(http.clone(), opsgenie_config.base_url.clone(), &user)
            .await
            .context(RequestPhoneNumberForPersonSnafu { username: &user })?;
        result_list.push(UserPhoneNumber {
            name: user.to_string(),
            phone: phone_number,
        })
    }

    let user = result_list.get(0).context(NoOnCallPersonSnafu)?;
    let username = &user.name;
    let phone_number = user
        .phone
        .get(0)
        .context(NoPhoneNumberSnafu { username: username })?;

    Ok(AlertInfo {
        username: username.clone(),
        phone_number: phone_number.clone(),
        full_information: result_list,
    })
}

#[derive(Clone, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ContactInformationResult {
    data: ContactInformationResultData,
}

#[derive(Clone, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ContactInformationResultData {
    id: String,
    username: String,
    full_name: String,
    user_contacts: Vec<UserContact>,
}

#[derive(Clone, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct UserContact {
    to: String,
    id: String,
    contact_method: String,
    enabled: bool,
}

async fn get_phone_number(
    http: Client,
    base_url: Url,
    username: &str,
) -> Result<Vec<String>, crate::util::Error> {
    let url_builder = base_url.clone();
    let url_builder = url_builder.join(&format!("users/{username}")).unwrap();
    tracing::debug!(
        "Retrieving contact information for [{}] information from [{}]",
        username,
        url_builder.to_string()
    );
    let contact_information = send_json_request::<ContactInformationResult>(
        http.get(url_builder.clone())
            .query(&[("expand", "contact")]),
    )
    .await?;
    tracing::trace!("Got data from jira: [{:?}]", contact_information);

    let mut numbers = contact_information
        .data
        .user_contacts
        .iter()
        .filter(|user_contact| {
            user_contact.contact_method.eq("voice") || user_contact.contact_method.eq("sms")
        })
        .map(|user_contact| format_phone_number(user_contact.to.clone()))
        .collect::<Vec<String>>();

    // Sort to enable easier deduplication and remove duplicate numbers
    numbers.sort();
    numbers.dedup();

    Ok(numbers)
}

fn format_phone_number(number: String) -> String {
    let number = number.replace("-", "");
    format!("+{}", number)
}
