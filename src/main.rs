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
        .list_account(
            None,
            None,
            Some(firefly_iii::models::AccountTypeFilter::Asset),
        )
        .await
        .context("unable to get existing accounts")?;

    for sbanken_account in sbanken_accounts.iter().filter(|acc| {
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
            .store_account(convert_account(&sbanken_account).context("unable to convert account")?)
            .await
            .context("unable to store account")?;
    }

    // Update accounts after possibly creating new accounts
    let firefly_accounts = firefly_client
        .accounts_api()
        .list_account(
            None,
            None,
            Some(firefly_iii::models::AccountTypeFilter::Asset),
        )
        .await
        .context("unable to get existing accounts")?;

    // Loop through all transactions for all accounts and add them to firefly
    for sbanken_account in sbanken_accounts.iter() {
        let account_id = if let Some(account_id) = sbanken_account.account_id.as_ref() {
            account_id
        } else {
            eprintln!(
                "Unable to find matching firefly account for '{}', skipping",
                sbanken_account.name.as_ref().unwrap()
            );
            continue;
        };

        let sbanken_transactions = sbanken_client
            .transactions_api()
            .get_transactions(
                &account_id,
                Some(&opt.sbanken_customer_id.expose_secret()),
                Some("2020-01-01".into()),
                None,
                None,
                Some(1000),
            )
            .await
            .context("unable to get transactions for account")?;

        if sbanken_transactions.is_error.unwrap_or(true) {
            eprintln!(
                "Error when accessing transaction, skipping: {}",
                sbanken_transactions.error_message.as_ref().unwrap()
            );
            continue;
        }

        eprintln!(
            "Found {} transaction(s) for account {}",
            sbanken_transactions.available_items.unwrap(),
            sbanken_account.name.as_ref().unwrap()
        );

        if let Some(firefly_account) = firefly_accounts.data.iter().find(|account_read| {
            account_read
                .attributes
                .notes
                .as_ref()
                .map(|notes| notes == account_id)
                .unwrap_or(false)
        }) {
            eprintln!("Updating transactions...");

            for sbanken_transaction in sbanken_transactions.items.unwrap() {
                let firefly_transaction =
                    convert_transaction(&firefly_account, &sbanken_transaction)
                        .context("unable to convert transaction")?;

                let t = &firefly_transaction.transactions[0];
                eprintln!(
                    "{}: {} -- {} --> {}",
                    t.date,
                    t.source_id
                        .map(|id| format!("<account {}>", id))
                        .or(t.source_name.clone())
                        .unwrap_or("<missing>".into()),
                    t.amount,
                    t.destination_id
                        .map(|id| format!("<account {}>", id))
                        .or(t.destination_name.clone())
                        .unwrap_or("<missing>".into()),
                );

                let _ = firefly_client
                    .transactions_api()
                    .store_transaction(firefly_transaction.clone())
                    .await
                    .map_err(|e| {
                        eprintln!("\tunable to store transaction, skipping: {}", e);
                    });
            }
        }
    }

    Ok(())
}

fn convert_transaction(
    firefly_account: &firefly_iii::models::AccountRead,
    sbanken_transaction: &sbanken::models::TransactionV1,
) -> Result<firefly_iii::models::Transaction> {
    use firefly_iii::models::{
        transaction_split::Type as TransactionType, Transaction, TransactionSplit,
    };

    let amount = sbanken_transaction.amount.unwrap();

    let mut split = TransactionSplit::new(
        // Extract date part of timestamp (YYYY-MM-DDTHH:MM:SS)
        sbanken_transaction.accounting_date.as_ref().unwrap()[0..10].into(),
        format!("{:.2}", amount.abs()),
        sbanken_transaction
            .text
            .as_ref()
            .unwrap()
            .chars()
            .filter(|c| c.is_ascii())
            .collect(),
        None,
        None,
    );

    split.category_name = sbanken_transaction.transaction_type.clone();

    if amount < 0.0 {
        split._type = Some(TransactionType::Withdrawal);
        split.source_id = firefly_account.id.clone().parse().ok();
        split.destination_name = sbanken_transaction.text.clone(); // TODO filter out dates
    } else {
        split._type = Some(TransactionType::Deposit);
        split.destination_id = firefly_account.id.clone().parse().ok();
        split.source_name = sbanken_transaction.text.clone(); // TODO filter out dates
    }

    let mut transaction = Transaction::new(vec![split]);
    transaction.error_if_duplicate_hash = Some(true);

    Ok(transaction)
}

fn convert_account(
    sbanken_account: &sbanken::models::AccountV1,
) -> Result<firefly_iii::models::Account> {
    use firefly_iii::models::account::*;
    let account_role = match &**sbanken_account.account_type.as_ref().unwrap() {
        "High interest account" => AccountRole::SavingAsset,
        "Standard account" => AccountRole::DefaultAsset,
        "BSU account" => AccountRole::SavingAsset,
        _ => {
            return Err(anyhow!(
                "conversion not implemented for account type '{}'",
                sbanken_account.account_type.as_ref().unwrap()
            ))
        }
    };
    let mut firefly_account = Account::new(sbanken_account.name.clone().unwrap(), Type::Asset);
    firefly_account.account_role = Some(account_role);
    firefly_account.account_number = Some(sbanken_account.account_number.clone().unwrap());
    firefly_account.notes = Some(sbanken_account.account_id.clone().unwrap());
    Ok(firefly_account)
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
