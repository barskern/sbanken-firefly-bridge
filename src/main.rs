use anyhow::{anyhow, Context, Result};
use firefly_iii::apis::{client::APIClient, configuration::Configuration};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use secrecy::{ExposeSecret, Secret};
use serde::Deserialize;
use structopt::StructOpt;

#[derive(StructOpt, Debug)]
#[structopt(about, author)]
struct Opts {
    #[structopt(long, env, hide_env_values = true)]
    sbanken_client_id: Secret<String>,
    #[structopt(long, env, hide_env_values = true)]
    sbanken_client_secret: Secret<String>,
    #[structopt(long, env, hide_env_values = true)]
    sbanken_customer_id: Secret<String>,
    #[structopt(long, env)]
    sbanken_auth_url: String,
    #[structopt(long, env)]
    sbanken_base_url: String,
    #[structopt(long, env)]
    firefly_base_url: String,
    #[structopt(long, env)]
    firefly_access_token: Secret<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opts::from_args();

    let sbanken_client = authorized_client(
        &opt.sbanken_auth_url,
        &opt.sbanken_client_id,
        &opt.sbanken_client_secret,
        &opt.sbanken_customer_id,
    )
    .await
    .context("unable to create authorized client")?;

    let firefly_client = APIClient::new(Configuration {
        base_path: opt.firefly_base_url,
        oauth_access_token: Some(opt.firefly_access_token.expose_secret().into()),
        ..Configuration::default()
    });

    // Make a "random" call to an API to test authorization
    let accounts_response: Items<Account> = sbanken_client
        .get(&format!(
            "{}/exec.bank/api/v1/Accounts",
            opt.sbanken_base_url
        ))
        .send()
        .await
        .context("unable to get accounts from sbanken")?
        .json()
        .await
        .context("unable to decode accounts response")?;

    let existing_accounts_response = firefly_client
        .accounts_api()
        .list_account(None, None, None)
        .await
        .map_err(|e| anyhow!("{:?}", e))
        .context("unable to get existing accounts")?;

    for raw_account in accounts_response.items.into_iter().filter(|acc| {
        existing_accounts_response
            .data
            .as_ref()
            .map(|existing_accounts| {
                !existing_accounts.iter().any(|account_read| {
                    account_read
                        .attributes
                        .as_ref()
                        .map(|account| {
                            account
                                .notes
                                .as_ref()
                                .map(|notes| notes == &acc.account_id)
                                .unwrap_or(false)
                        })
                        .unwrap_or(false)
                })
            })
            .unwrap_or(true)
    }) {
        eprintln!("Account '{}' does not already exist, creating...", raw_account.name);
        firefly_client
            .accounts_api()
            .store_account(raw_account.into())
            .await
            .map_err(|e| anyhow!("{:?}", e))
            .context("unable to store account")?;
    }

    Ok(())
}

async fn authorized_client(
    auth_url: &str,
    client_id: &Secret<String>,
    client_secret: &Secret<String>,
    customer_id: &Secret<String>,
) -> Result<reqwest::Client> {
    let auth_token = get_auth_token(&auth_url, &client_id, &client_secret)
        .await
        .context("unable to get auth token")?;

    // Create a client with pre-configured headers needed for all API calls
    let headers = {
        use reqwest::header::*;
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            format!("Bearer {}", auth_token.expose_secret())
                .parse()
                .context("fetched auth_token contains invalid characters")?,
        );
        headers.insert(
            "customerId",
            customer_id
                .expose_secret()
                .parse()
                .expect("unreachable: customer_id should not contain non-visible ascii characters"),
        );
        headers
    };

    Ok(reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .context("unable to build http client")?)
}

async fn get_auth_token(
    auth_url: &str,
    client_id: &Secret<String>,
    client_secret: &Secret<String>,
) -> Result<Secret<String>> {
    #[derive(Debug, Deserialize)]
    struct AuthSuccess {
        access_token: Secret<String>,
    }
    #[derive(Debug, Deserialize)]
    struct AuthError {
        error: String,
    }
    #[derive(Debug, Deserialize)]
    #[serde(untagged)]
    enum AuthResponse {
        Success(AuthSuccess),
        Error(AuthError),
    }

    let auth_response: AuthResponse = reqwest::Client::new()
        .post(auth_url)
        .header(reqwest::header::ACCEPT, "application/json")
        .basic_auth(
            utf8_percent_encode(client_id.expose_secret(), NON_ALPHANUMERIC),
            Some(utf8_percent_encode(
                client_secret.expose_secret(),
                NON_ALPHANUMERIC,
            )),
        )
        .form(&[("grant_type", "client_credentials")])
        .send()
        .await?
        .json()
        .await?;

    match auth_response {
        AuthResponse::Success(AuthSuccess { access_token }) => Ok(access_token),
        AuthResponse::Error(AuthError { error }) => {
            Err(anyhow!("received error from api: {}", error))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Items<T> {
    #[serde(rename = "availableItems")]
    available_items: Option<i64>,
    #[serde(rename = "errorCode")]
    error_code: Option<serde_json::Value>,
    #[serde(rename = "errorMessage")]
    error_message: Option<serde_json::Value>,
    #[serde(rename = "errorType")]
    error_type: Option<serde_json::Value>,
    #[serde(rename = "isError")]
    is_error: Option<bool>,
    items: Vec<T>,
    #[serde(rename = "traceId")]
    trace_id: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct Account {
    #[serde(rename = "accountId")]
    account_id: String,
    #[serde(rename = "accountNumber")]
    account_number: String,
    #[serde(rename = "accountType")]
    account_type: String,
    #[serde(deserialize_with = "deserialize_money")]
    available: f64,
    #[serde(deserialize_with = "deserialize_money")]
    balance: f64,
    #[serde(rename = "creditLimit", deserialize_with = "deserialize_money")]
    credit_limit: f64,
    name: String,
    #[serde(rename = "ownerCustomerId")]
    owner_customer_id: String,
}

impl Into<firefly_iii::models::Account> for Account {
    fn into(self) -> firefly_iii::models::Account {
        use firefly_iii::models::account::*;
        let (_type, role_opt) = match &*self.account_type {
            "High interest account" => (Type::Asset, Some(AccountRole::SavingAsset)),
            "Standard account" => (Type::Expense, None),
            _ => todo!("account type: {}", self.account_type),
        };
        let mut acc = Account::new(self.name, _type);
        acc.account_role = role_opt;
        acc.account_number = Some(self.account_number);
        acc.notes = Some(self.account_id);
        acc
    }
}

fn deserialize_money<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::de::Deserializer<'de>,
{
    #[derive(Debug, Deserialize)]
    #[serde(untagged)]
    enum Number {
        F64(f64),
        I64(i64),
    }

    Number::deserialize(deserializer).map(|v| match v {
        Number::F64(x) => x,
        Number::I64(x) => x as f64,
    })
}
