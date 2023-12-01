#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = near_fetch::Client::new("https://rpc.testnet.near.org");

    // View the latest block to be generated on the network.
    let block = client.view_block().await?;
    println!("lastest block: {block:#?}");

    // Reference the chunk via the block hash we queried for earlier:
    let shard_id = 0;
    let chunk = client
        .view_chunk()
        .block_hash_and_shard(block.header.hash, shard_id)
        .await?;
    println!("Latest Chunk: {chunk:#?}");

    // Check current gas price on the network:
    let price = client.gas_price().await?;
    println!("Current gas price: {price:#?}");

    let testnet_id = "testnet".parse()?;
    let access_keys = client.view_access_keys(&testnet_id).await?;
    println!("lastest testnet account access keys: {access_keys:#?}");

    Ok(())
}
