use near_account_id::AccountId;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = near_fetch::Client::new("https://rpc.mainnet.near.org");

    let my_id: &str = "chaotictempest.near";
    let sk = "Insert your secret key here. Please do not commit your secret key to git! This is just an example";

    // let account = Account::from_secret_key(my_id.parse()?, SecretKey::from_str(sk)?, &worker);
    let social: AccountId = "social.near".parse()?;

    // Get and show our current profile data:
    let res = client
        .view(&social, "get")
        .args_json(serde_json::json!({
            "keys": [format!("{my_id}/profile/**")]
        }))
        .await?;
    println!("{:#?}", res.json::<serde_json::Value>());

    /// Set some test data into social.near. Default gas of 10Tgas is used.
    let result = client
        .call(&social, "set")
        .args_json(serde_json::json!({
            "data": {
                me: {
                    "test": {
                        "input": "Hello from near_fetch client",
                    }
                }
            },
        }))
        // `.deposit` is required if our account on near social hasn't kept a storage
        // deposit in NEAR social contract:
        // .deposit(...)
        .transact()
        .await?
        .json::<serde_json::Value>();
    println!("transaction result from setting data into social.near: {result:#?}");

    Ok(())
}
