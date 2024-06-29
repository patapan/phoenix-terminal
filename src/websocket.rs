use anyhow::{anyhow, Result};
use base64::engine::general_purpose;
use base64::Engine;
use chrono::{TimeZone, Utc};
use ellipsis_client::tokio;
use ellipsis_client::tokio::net::TcpStream;
use futures_util::sink::SinkExt;
use futures_util::stream::{SplitSink, SplitStream, StreamExt};
use phoenix::program::accounts::MarketHeader;
use phoenix::program::dispatch_market::load_with_dispatch;
use phoenix::state::markets::fifo::FIFOOrderId;
use phoenix_sdk::orderbook::Orderbook;
use phoenix_sdk::sdk_client::{
    MarketEventDetails, MarketMetadata, PhoenixEvent, PhoenixOrder, SDKClient,
};
use serde::Deserialize;
use serde_json::json;
use solana_sdk::signature::{Keypair, Signature};
use std::collections::{BTreeMap, VecDeque};
use std::mem::size_of;
use std::process::Command;
use tokio::io::{self, AsyncBufReadExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use url::Url;

#[derive(Deserialize, Debug, Clone)]
pub struct Market {
    pub account: String,
    pub base_ticker: String,
    pub quote_ticker: String,
}

pub struct Terminal {
    pub rpc: String,
    pub market: Market,
    pub websocket: WebSocket,
}
pub struct WebSocket {
    write: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    read: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
}

const NUM_RECENT_ORDERS: usize = 10;

impl WebSocket {
    // Initialise and return websocket
    pub async fn init(url: Url) -> Self {
        let (ws_stream, _) = connect_async(url).await.expect("Failed to connect");
        let (write, read) = ws_stream.split();
        println!("WebSocket handshake has been successfully completed");

        WebSocket { write, read }
    }

    pub async fn subscribe_market_until_exit(
        &mut self,
        market: &Market,
        http_rpc: &str,
    ) -> Result<()> {
        let subscribe_messages = vec![
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "accountSubscribe",
                "params": [
                    market.account,
                    {
                        "encoding": "jsonParsed",
                        "commitment": "finalized"
                    }
                ]
            })
            .to_string(),
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "logsSubscribe",
                "params": [
                    {
                        "mentions": [ market.account ]
                      },
                      {
                        "commitment": "finalized"
                      }
                ]
            })
            .to_string(),
        ];

        for sub in subscribe_messages {
            // Send the subscribe message
            self.write
                .send(Message::Text(sub))
                .await
                .expect("Failed to send subscribe message");
        }

        let mut subscription_ids: Vec<i64> = vec![];
        let mut stdin = io::BufReader::new(io::stdin()).lines();

        // Init phoenix sdk to deserialise transaction data
        let payer = Keypair::new();
        let phoenix_sdk = SDKClient::new(&payer, &http_rpc).await?;

        let mut recent_orders = VecDeque::new();
        loop {
            tokio::select! {
                Some(message) = self.read.next() => {
                    if let Ok(Message::Text(text)) = message {
                        if let Ok(parsed_json) = serde_json::from_str::<serde_json::Value>(&text) {
                            // Check if the message is a response to the subscribe request
                            if parsed_json.get("id") == Some(&json!(1)) {
                                // Capture the subscription ID from the response
                                if let Some(sub_id) = parsed_json.get("result").and_then(|r| r.as_i64()) {
                                    subscription_ids.push(sub_id);
                                }
                            } else if let Some(serialized_market_data) =
                                parsed_json["params"]["result"]["value"]["data"][0].as_str()
                            {
                                let decoded_data = general_purpose::STANDARD.decode(serialized_market_data)
                                    .expect("Failed to decode base64 string");
                                let (orderbook, metadata) = deserialize_market_data(decoded_data)
                                    .expect("Expected orderbook");
                                refresh_terminal(&recent_orders, &market, &metadata);
                                orderbook.print_ladder(5, 4);
                                println!("\nPress Enter to exit stream");
                            } else {
                                // We know it's a transaction message
                                let signature = parsed_json["params"]["result"]["value"]["signature"].as_str();
                                if signature.is_some() {
                                    let decoded = bs58::decode(signature.unwrap())
                                        .into_vec()
                                        .expect("Failed to decode transaction signature");
                                    let signature = Signature::new(&decoded);
                                    let events = phoenix_sdk
                                        .parse_events_from_transaction(&signature)
                                        .await
                                        .unwrap_or_default();

                                    let fills = events
                                        .iter()
                                        .filter_map(|&event| match event.details {
                                            MarketEventDetails::Fill(..) => Some(event),
                                            _ => None,
                                        })
                                        .collect::<Vec<PhoenixEvent>>();

                                    for fill in fills {
                                        recent_orders.push_back(fill);
                                        if recent_orders.len() > NUM_RECENT_ORDERS {
                                            recent_orders.pop_front();
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                line = stdin.next_line() => {
                    if let Ok(Some(_line)) = line {
                        self.unsubscribe(&subscription_ids).await;
                        recent_orders.clear();
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    async fn unsubscribe(&mut self, subscription_ids: &Vec<i64>) {
        // Here we assume the IDs are returned in the order we sent,
        // as we cannot distinguish the rpc responses for account / transaction sub
        let unsubscribe_messages = vec![
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "accountUnsubscribe",
                "params": [
                    subscription_ids.get(0)
                ]
            })
            .to_string(),
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "logsUnsubscribe",
                "params": [
                    subscription_ids.get(1)
                ]
            })
            .to_string(),
        ];
        for unsub in unsubscribe_messages {
            self.write
                .send(Message::Text(unsub))
                .await
                .expect("Failed to send unsubscribe message");
        }
    }
}

fn process_phoenix_event(event: PhoenixEvent, metadata: &MarketMetadata) -> String {
    let timestamp = Utc.timestamp_opt(event.timestamp, 0).unwrap();

    match event.details {
        MarketEventDetails::Fill(fill_details) => {
            format!(
                "{} {:?} - Price: {} Size: {} Maker: {} Taker: {}",
                timestamp.format("%Y-%m-%d %H:%M:%S").to_string(),
                fill_details.side_filled,
                metadata.ticks_to_float_price(fill_details.price_in_ticks),
                (metadata.base_lots_to_base_atoms(fill_details.base_lots_filled)
                    / 10_u64.pow(metadata.base_decimals.into())),
                fill_details
                    .maker
                    .to_string()
                    .chars()
                    .take(6)
                    .collect::<String>(),
                fill_details
                    .taker
                    .to_string()
                    .chars()
                    .take(6)
                    .collect::<String>()
            )
        }
        MarketEventDetails::Place(_) => todo!(),
        MarketEventDetails::Evict(_) => todo!(),
        MarketEventDetails::Reduce(_) => todo!(),
        MarketEventDetails::FillSummary(_) => todo!(),
        MarketEventDetails::Fee(_) => todo!(),
        MarketEventDetails::TimeInForce(_) => todo!(),
    }
}

fn get_market_metadata_from_header_bytes(header_bytes: &[u8]) -> Result<MarketMetadata> {
    bytemuck::try_from_bytes(header_bytes)
        .map_err(|_| anyhow!("Failed to deserialize market header"))
        .and_then(MarketMetadata::from_header)
}

fn deserialize_market_data(
    market_account_data: Vec<u8>,
) -> Result<(Orderbook<FIFOOrderId, PhoenixOrder>, MarketMetadata)> {
    let default_orderbook = Orderbook::<FIFOOrderId, PhoenixOrder> {
        raw_base_units_per_base_lot: 0.0,
        quote_units_per_raw_base_unit_per_tick: 0.0,
        bids: BTreeMap::new(),
        asks: BTreeMap::new(),
    };

    let (header_bytes, bytes) = market_account_data.split_at(size_of::<MarketHeader>());
    let meta = get_market_metadata_from_header_bytes(header_bytes)?;
    let raw_base_units_per_base_lot = meta.raw_base_units_per_base_lot();
    let quote_units_per_raw_base_unit_per_tick = meta.quote_units_per_raw_base_unit_per_tick();
    Ok((
        load_with_dispatch(&meta.market_size_params, bytes)
            .map(|market| {
                Orderbook::from_market(
                    market.inner,
                    raw_base_units_per_base_lot,
                    quote_units_per_raw_base_unit_per_tick,
                )
            })
            .unwrap_or_else(|_| default_orderbook.clone()),
        meta,
    ))
}

pub fn refresh_terminal(
    recent_orders: &VecDeque<PhoenixEvent>,
    market: &Market,
    metadata: &MarketMetadata,
) {
    let clear_command = if cfg!(target_os = "windows") {
        "cls"
    } else {
        "clear"
    };

    Command::new(clear_command).status().unwrap();

    if recent_orders.len() == 0 {
        print!("No ");
    }
    println!(
        "Recent orders for {}/{}\n",
        market.base_ticker, market.quote_ticker
    );

    let mut elements_printed = 0;
    for fill in recent_orders.iter().take(NUM_RECENT_ORDERS) {
        println!("{}", process_phoenix_event(*fill, &metadata));
        elements_printed += 1;
    }

    while elements_printed < NUM_RECENT_ORDERS {
        println!("");
        elements_printed += 1;
    }

    println!("\nLive orderbook on Phoenix:\n");
}
