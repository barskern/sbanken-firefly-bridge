use anyhow::{anyhow, Context, Result};
use firefly_iii::apis::{
    client::APIClient as FireflyClient, configuration::Configuration as FireflyConfiguration,
};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use sbanken::apis::{
    client::APIClient as SbankenClient, configuration::Configuration as SbankenConfiguration,
};
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

    let sbanken_token = get_auth_token(
        &opt.sbanken_auth_url,
        &opt.sbanken_client_id,
        &opt.sbanken_client_secret,
    )
    .await
    .context("unable to get sbanken auth token")?;

    let sbanken_client = SbankenClient::new(SbankenConfiguration {
        base_path: opt.sbanken_base_url,
        oauth_access_token: Some(sbanken_token.expose_secret().into()),
        ..SbankenConfiguration::default()
    });

    let firefly_client = FireflyClient::new(FireflyConfiguration {
        base_path: opt.firefly_base_url,
        oauth_access_token: Some(opt.firefly_access_token.expose_secret().into()),
        ..FireflyConfiguration::default()
    });

    let sbanken_accounts = sbanken_client
        .accounts_api()
        .list_accounts(Some(opt.sbanken_customer_id.expose_secret()))
        .await
        .context("unable to fetch accounts from sbanken")?
        .items
        .unwrap();

    let firefly_accounts = firefly_client
        .accounts_api()
        .list_account(None, None, None)
        .await
        .context("unable to get existing accounts")?;

    for sbanken_account in sbanken_accounts.into_iter().filter(|acc| {
        !firefly_accounts.data.iter().any(|account_read| {
            account_read
                .attributes
                .notes
                .as_ref()
                .map(|notes| notes == acc.account_id.as_ref().unwrap())
                .unwrap_or(false)
        })
    }) {
        eprintln!(
            "Account '{}' does not already exist, creating...",
            sbanken_account.name.as_ref().unwrap()
        );
        firefly_client
            .accounts_api()
            .store_account(convert_account(sbanken_account).context("unable to convert account")?)
            .await
            .context("unable to store account")?;
    }

    Ok(())
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

fn convert_account(
    sbanken_account: sbanken::models::AccountV1,
) -> Result<firefly_iii::models::Account> {
    use firefly_iii::models::account::*;
    let (_type, role_opt) = match &**sbanken_account.account_type.as_ref().unwrap() {
        "High interest account" => (Type::Asset, Some(AccountRole::SavingAsset)),
        "Standard account" => (Type::Expense, None),
        "BSU account" => (Type::Asset, Some(AccountRole::SavingAsset)),
        _ => {
            return Err(anyhow!(
                "conversion not implemented for account type '{}'",
                sbanken_account.account_type.unwrap()
            ))
        }
    };
    let mut firefly_account = Account::new(sbanken_account.name.unwrap(), _type);
    firefly_account.account_role = role_opt;
    firefly_account.account_number = Some(sbanken_account.account_number.unwrap());
    firefly_account.notes = Some(sbanken_account.account_id.unwrap());
    Ok(firefly_account)
}
