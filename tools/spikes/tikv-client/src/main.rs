use tikv_client::TransactionClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let endpoints = std::env::var("PD_ENDPOINTS").unwrap_or_else(|_| "127.0.0.1:2379".to_string());
    let addrs: Vec<String> = endpoints
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let client = TransactionClient::new(addrs).await?;
    let ts = client.current_timestamp().await?;
    println!("timestamp: {:?}", ts);

    let mut txn = client.begin_optimistic().await?;
    let key = "spike:key".to_string();
    txn.put(key.clone(), b"value".to_vec()).await?;
    let value = txn.get(key.clone()).await?;
    println!("read back: {:?}", value);
    txn.commit().await?;
    println!("commit: ok");

    Ok(())
}
