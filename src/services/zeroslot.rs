use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use reqwest::Client;
use serde_json::json;
use solana_sdk::transaction::Transaction;

/// URL base de 0slot (puedes cambiar a otro datacenter: de, ams, jp, la)
const ZEROSLOT_URL: &str = "https://ny.0slot.trade";

/// Envía una transacción firmada a 0slot
pub async fn send_transaction_zeroslot(tx: &Transaction) -> Result<String> {
    // API key desde variable de entorno
    let api_key = std::env::var("ZEROSLOT_API_KEY")
        .map_err(|_| anyhow!("Falta la variable de entorno ZEROSLOT_API_KEY"))?;

    // Serializar transacción a base64
    let serialized = bincode::serialize(tx)?;
    let encoded_tx = BASE64.encode(&serialized);

    // Construir cliente HTTP
    let client = Client::new();
    let url = format!("{}?api-key={}", ZEROSLOT_URL, api_key);

    // Payload JSON-RPC
    let payload = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "sendTransaction",
        "params": [
            encoded_tx,
            { "encoding": "base64" }
        ]
    });

    // Enviar request
    let resp = client
        .post(&url)
        .json(&payload)
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;

    // Parsear respuesta → firma de la transacción
    if let Some(sig) = resp.get("result") {
        Ok(sig.as_str().unwrap_or_default().to_string())
    } else if let Some(err) = resp.get("error") {
        Err(anyhow!("0slot error: {:?}", err))
    } else {
        Err(anyhow!("Respuesta inesperada de 0slot: {:?}", resp))
    }
}
