use anyhow::{anyhow, Context, Result};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use secrecy::{ExposeSecret, Secret};
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opts::from_args();

    let client = authorized_client(
        &opt.sbanken_auth_url,
        &opt.sbanken_client_id,
        &opt.sbanken_client_secret,
        &opt.sbanken_customer_id,
    )
    .await
    .context("unable to create authorized client")?;

    // Make a "random" call to an API to test authorization
    let x: serde_json::Value = client
        .get(&format!(
            "{}/exec.customers/api/v1/Customers",
            opt.sbanken_base_url
        ))
        .send()
        .await?
        .json()
        .await?;

    dbg!(x);

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
            customer_id.expose_secret().parse().expect(
                "unreachable: customer_id should not contain non-visible ascii characters",
            ),
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
    #[derive(Debug, serde::Deserialize)]
    struct AuthSuccess {
        access_token: Secret<String>,
    }
    #[derive(Debug, serde::Deserialize)]
    struct AuthError {
        error: String,
    }
    #[derive(Debug, serde::Deserialize)]
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
