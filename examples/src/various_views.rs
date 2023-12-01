#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = near_fetch::Client::new("https://rpc.testnet.near.org");
    let block = client.view_block().await?;
    println!("lastest block: {block:#?}");

    let testnet_id = "testnet".parse()?;
    let access_keys = client.view_access_keys(&testnet_id).await?;
    println!("lastest block: {access_keys:#?}");

    Ok(())
}
