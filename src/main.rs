use anyhow::{anyhow, Context, Result};
use chrono::Datelike;
use firefly_iii::apis::{
    client::APIClient as FireflyClient, configuration::Configuration as FireflyConfiguration,
};
use lazy_static::lazy_static;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use regex::Regex;
use sbanken::apis::{
    client::APIClient as SbankenClient, configuration::Configuration as SbankenConfiguration,
};
use secrecy::{ExposeSecret, Secret};
use serde::Deserialize;
use structopt::StructOpt;

const DATE_FORMAT: &str = "%Y-%m-%d";

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
    #[structopt(long, env, hide_env_values = true)]
    firefly_access_token: Secret<String>,
    #[structopt(long, default_value = "10")]
    delay_days: i64,
    #[structopt(long, default_value = "2019")]
    first_year: i32,
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

    let firefly_accounts = firefly_client
        .accounts_api()
        .list_account(
            None,
            None,
            Some(firefly_iii::models::AccountTypeFilter::Asset),
        )
        .await
        .context("unable to get existing accounts")?;

    let first_sync_day = std::fs::read("firefly_last_sync")
        .ok()
        .map(|s| {
            std::str::from_utf8(&s)
                .context("invalid encoding in firefly_last_sync")
                .and_then(|s| {
                    chrono::NaiveDate::parse_from_str(s, DATE_FORMAT)
                        .context("invalid date in firefly_last_sync")
                })
        })
        .transpose()?;

    let last_sync_day = (chrono::Utc::today() - chrono::Duration::days(opt.delay_days)).naive_local();

    if first_sync_day == Some(last_sync_day) {
        eprintln!("Already updated everything until {}", last_sync_day);
        return Ok(());
    }

    let actual_first_year = first_sync_day
        .map(|day| day.year())
        .unwrap_or(opt.first_year);
    let actual_last_year = last_sync_day.year();

    // Do one year at a time
    for year in actual_first_year..=actual_last_year {
        // Collect all transactions which need to be deduplicated, for each account in this vector
        let mut needs_deduplication = Vec::new();

        // Loop through all transactions for all accounts and add them to firefly
        for sbanken_account in sbanken_accounts.iter() {
            let account_id = sbanken_account.account_id.as_ref().unwrap();

            let sbanken_transactions = sbanken_client
                .transactions_api()
                .get_transactions(
                    &account_id,
                    Some(&opt.sbanken_customer_id.expose_secret()),
                    if year == actual_first_year {
                        Some(
                            first_sync_day
                                .map(|day| day.format(DATE_FORMAT).to_string())
                                .unwrap_or_else(|| format!("{}-01-01", year)),
                        )
                    } else {
                        Some(format!("{}-01-01", year))
                    },
                    if year == actual_last_year {
                        Some(last_sync_day.format(DATE_FORMAT).to_string())
                    } else {
                        Some(format!("{}-12-31", year))
                    },
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
                    if sbanken_transaction.transaction_type.as_deref() == Some("OVFNETTB")
                        || sbanken_transaction.transaction_type.as_deref() == Some("MOB.B.OVF")
                        || sbanken_transaction.transaction_type.as_deref() == Some("TILBAKEF.")
                    {
                        eprintln!(
                            "{} {}: {} -- {} -- {} **internal transaction for dedup**",
                            &sbanken_transaction.accounting_date.as_deref().unwrap()[..10],
                            sbanken_transaction.transaction_type.as_deref().unwrap(),
                            &firefly_account.attributes.name,
                            sbanken_transaction.amount.unwrap(),
                            sbanken_transaction.text.as_deref().unwrap(),
                        );

                        // Transaction is an internal bank transfer and has to be deduplicated.
                        needs_deduplication.push((account_id, sbanken_transaction));
                        continue;
                    }

                    let firefly_transaction =
                        convert_transaction(&firefly_account, &sbanken_transaction, None)
                            .context("unable to convert transaction")?;

                    let t = &firefly_transaction.transactions[0];
                    eprintln!(
                        "{} {}: {} -- {} --> {}",
                        t.date,
                        sbanken_transaction.transaction_type.as_deref().unwrap(),
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

        needs_deduplication.sort_by(|(_, a), (_, b)| {
            a.amount
                .unwrap()
                .abs()
                .partial_cmp(&b.amount.unwrap().abs())
                .expect("unreachable: amount was NaN")
                .then_with(|| a.accounting_date.cmp(&b.accounting_date))
                .then_with(|| a.text.cmp(&b.text))
                .then_with(|| a.amount.unwrap().partial_cmp(&b.amount.unwrap()).unwrap())
        });

        // Find and fix identical transfers which are sorted after eachother
        let flats: Vec<_> = needs_deduplication
            .windows(2)
            .map(|win| (win[0].1.amount.unwrap(), win[1].1.amount.unwrap()))
            .scan(0, |state, (prev, cur)| {
                let diff = cur - prev;

                if diff > 0.0 {
                    // rising "edge"
                    *state = 0;
                    Some(0)
                } else if diff < 0.0 {
                    // falling "edge"
                    let prev_state = *state;
                    *state = 0;
                    Some(prev_state)
                } else {
                    // flat
                    *state += 1;
                    Some(0)
                }
            })
            .enumerate()
            .filter(|&(_, flat_count)| flat_count > 0)
            .collect(); // We have to collect to be able modify needs_deduplication

        for (last_index, amount) in flats {
            let consecutive_duplicates = amount + 1;

            let first_index = (last_index + 1) - 2 * consecutive_duplicates;

            let shift_amount = if consecutive_duplicates % 2 == 1 {
                consecutive_duplicates
            } else {
                consecutive_duplicates - 1
            };

            let shifts = consecutive_duplicates / 2;

            for s in 0..shifts {
                let i = first_index + 1 + 2 * s;
                needs_deduplication.swap(i, i + shift_amount);
            }
        }

        // Run deduplication on this list, which is now exactly sorted so that sender and receiver are in the same pairs
        let mut dedup_chunks = needs_deduplication.chunks_exact(2);
        for pair in &mut dedup_chunks {
            let (from_ac, from_trans) = &pair[0];
            let (to_ac, to_trans) = &pair[1];

            let from_account = firefly_accounts
                .data
                .iter()
                .find(|account_read| {
                    account_read
                        .attributes
                        .notes
                        .as_ref()
                        .map(|notes| notes == *from_ac)
                        .unwrap_or(false)
                })
                .unwrap();

            let to_account = firefly_accounts
                .data
                .iter()
                .find(|account_read| {
                    account_read
                        .attributes
                        .notes
                        .as_ref()
                        .map(|notes| notes == *to_ac)
                        .unwrap_or(false)
                })
                .unwrap();

            eprintln!(
                "{} ({}) : {} -- {:6.2} ({:6.2}) --> {} : {} ({})",
                from_trans.accounting_date.as_ref().unwrap(),
                to_trans.accounting_date.as_ref().unwrap(),
                from_account.attributes.name,
                from_trans.amount.unwrap(),
                to_trans.amount.unwrap(),
                to_account.attributes.name,
                from_trans.text.as_ref().unwrap(),
                to_trans.text.as_ref().unwrap(),
            );

            if from_trans.amount == to_trans.amount.map(|f| -f)
                && from_trans.text == to_trans.text
                && from_trans.accounting_date == to_trans.accounting_date
            {
                let firefly_transaction =
                    convert_transaction(&from_account, &from_trans, Some(&to_account))
                        .context("unable to convert transaction")?;

                let _ = firefly_client
                    .transactions_api()
                    .store_transaction(firefly_transaction.clone())
                    .await
                    .map_err(|e| {
                        eprintln!("\tunable to store transaction, skipping: {}", e);
                    });
            } else {
                eprintln!("\twarn: got unbalanced transaction (not equal amount/date/text), skipping")
            }
        }

        if let Some((from_ac, from_trans)) = &dedup_chunks.remainder().first() {
            let from_account = firefly_accounts
                .data
                .iter()
                .find(|account_read| {
                    account_read
                        .attributes
                        .notes
                        .as_ref()
                        .map(|notes| notes == *from_ac)
                        .unwrap_or(false)
                })
                .unwrap();

            eprintln!(
                "GOT A LEFTOVER TRANSACTION: {} : {} -- {:6.2} -->  : {}",
                from_trans.accounting_date.as_ref().unwrap(),
                from_account.attributes.name,
                from_trans.amount.unwrap().abs(),
                from_trans.text.as_ref().unwrap(),
            );
        }
    }

    std::fs::write(
        "firefly_last_sync",
        &last_sync_day.format(DATE_FORMAT).to_string(),
    )?;

    Ok(())
}

fn cleanup_description(desc: &str) -> String {
    lazy_static! {
        static ref START_DATE: Regex = Regex::new(r"^\d{2}\.\d{2}\s").unwrap();
        static ref VISA_VARE_EXTRACT: Regex =
            Regex::new(r"(?i)^\*\d{4}\s\d{2}\.\d{2}\s\w{3}\s\d+.\d{2}\s(.+?)\sKurs:\s\d+.\d+$")
                .unwrap();
        static ref PAY_DATE: Regex = Regex::new(r"Betalt:\s\d{2}\.\d{2}\.\d{2}$").unwrap();
    }

    // Remove leading date (e.g. "12.02 KIWI ...")
    let desc = START_DATE.replace(desc, "");

    // Remove trailing pay date (e.g. "KIWI ... Betalt: 12.03.20")
    let desc = PAY_DATE.replace(&desc, "");

    // Remove leading "Fra: " and "Til: "
    let desc = desc.trim_start_matches("Til: ");
    let desc = desc.trim_start_matches("Fra: ");

    // Extract name of company from VISA_VARE description
    // (e.g. "*6227 26.02 NOK 30.00 COCA-COLA ENTERPRISES NOR Kurs: 1.0000")
    let desc = VISA_VARE_EXTRACT
        .captures(&desc)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
        .unwrap_or(&desc);

    let desc = if desc.to_lowercase().starts_with("skimore") { "Skimore" } else { desc };

    let desc = if desc.to_lowercase().starts_with("starbucks") { "Starbucks" } else { desc };

    let desc = if desc.to_lowercase().starts_with("steam") { "Steam" } else { desc };

    let desc = if desc.to_lowercase().starts_with("domeneshop") { "Domeneshop" } else { desc };

    let desc = if desc.to_lowercase().starts_with("hokksund sushi og thai") { "Hokksund Sushi og Thai" } else { desc };

    let desc = if desc.to_lowercase().starts_with("tekna") { "TEKNA" } else { desc };

    return desc.trim().to_string();
}

fn convert_transaction(
    main_account: &firefly_iii::models::AccountRead,
    sbanken_transaction: &sbanken::models::TransactionV1,
    other_account: Option<&firefly_iii::models::AccountRead>,
) -> Result<firefly_iii::models::Transaction> {
    use firefly_iii::models::{
        transaction_split::Type as TransactionType, Transaction, TransactionSplit,
    };

    let amount = sbanken_transaction.amount.unwrap();

    let mut split = TransactionSplit::new(
        // Extract date part of timestamp (YYYY-MM-DDTHH:MM:SS)
        sbanken_transaction.accounting_date.as_ref().unwrap()[0..10].into(),
        format!("{:.2}", amount.abs()),
        sbanken_transaction.text.as_ref().unwrap().clone(),
        None,
        None,
    );

    split.category_name = sbanken_transaction.transaction_type.clone();

    if amount < 0.0 {
        split.source_id = main_account.id.clone().parse().ok();
        if let Some(to_account) = other_account {
            split._type = Some(TransactionType::Transfer);
            split.destination_id = to_account.id.clone().parse().ok();
        } else {
            split._type = Some(TransactionType::Withdrawal);
            split.destination_name = sbanken_transaction.text.as_deref().map(cleanup_description);
        }
    } else {
        split.destination_id = main_account.id.clone().parse().ok();
        if let Some(to_account) = other_account {
            split._type = Some(TransactionType::Transfer);
            split.source_id = to_account.id.clone().parse().ok();
        } else {
            split._type = Some(TransactionType::Deposit);
            split.source_name = sbanken_transaction.text.as_deref().map(cleanup_description);
        }
    }

    Ok(Transaction::new(vec![split]))
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
