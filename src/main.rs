use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use tokio_tungstenite::{connect_async, tungstenite::Message};

const WS_URL: &str = "wss://api.hyperliquid.xyz/ws";

#[derive(Debug, Deserialize)]
struct WsResponse {
    channel: String,
    data: Value,
}

#[derive(Debug, Deserialize)]
struct WsLevel {
    px: String,
    sz: String,
    n: u32,
}

#[derive(Debug, Deserialize)]
struct WsBbo {
    coin: String,
    time: u64,
    bbo: [Option<WsLevel>; 2],
}

#[derive(Debug, Deserialize)]
struct SpotBalance {
    coin: String,
    token: u32,
    total: String,
    hold: String,
    #[serde(rename = "entryNtl")]
    entry_ntl: String,
}

#[derive(Debug, Deserialize)]
struct SpotState {
    balances: Vec<SpotBalance>,
}

#[derive(Debug, Deserialize)]
struct WebData2 {
    #[serde(rename = "spotState")]
    spot_state: SpotState,
}

pub struct HyperliquidWsClient {
    coins: Vec<String>,
    balance_coins: Vec<String>,
    user_address: String,
    store: [f64; 8],
    coin_a: String,
    coin_b: String,
    sz: f64,
}

impl HyperliquidWsClient {
    pub fn new(
        coins: Vec<String>,
        balance_coins: Vec<String>,
        user_address: String,
        sz: f64,
    ) -> Self {
        Self {
            coin_a: coins[0].clone(),
            coin_b: coins[1].clone(),
            coins,
            balance_coins,
            user_address,
            store: [0.0; 8],
            sz,
        }
    }

    pub async fn connect_and_subscribe(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let (ws_stream, _) = connect_async(WS_URL).await?;
        println!("Connected to Hyperliquid WebSocket");

        let (mut ws_sender, mut ws_receiver) = ws_stream.split();

        // Subscribe to BBO for price feeds
        for coin in &self.coins {
            let bbo_subscription = json!({
                "method": "subscribe",
                "subscription": {
                    "type": "bbo",
                    "coin": coin
                }
            });
            ws_sender
                .send(Message::Text(bbo_subscription.to_string().into()))
                .await?;
            println!("Subscribed to BBO for coin: {}", coin);
        }

        // Subscribe to clearing house state for balances
        let clearing_house_subscription = json!({
            "method": "subscribe",
            "subscription": {
                "type": "webData2",
                "user": self.user_address
            }
        });
        ws_sender
            .send(Message::Text(
                clearing_house_subscription.to_string().into(),
            ))
            .await?;
        println!("Subscribed to webData2 for user: {}", self.user_address);

        while let Some(msg) = ws_receiver.next().await {
            match msg? {
                Message::Text(text) => {
                    self.handle_message(&text).await?;
                }
                Message::Close(_) => {
                    println!("WebSocket connection closed");
                    break;
                }
                _ => {}
            }
        }

        Ok(())
    }

    async fn handle_message(&mut self, text: &str) -> Result<(), Box<dyn std::error::Error>> {
        let response: WsResponse = serde_json::from_str(text)?;

        match response.channel.as_str() {
            "subscriptionResponse" => {
                println!("Subscription confirmed");
            }
            "bbo" => {
                self.handle_bbo_update(response.data).await?;

                let usdh_bid = self.store[1]; // @232 bid (HYPE/USDH)
                let usdh_ask = self.store[3]; // @232 ask 
                let usdc_bid = self.store[5]; // @107 bid (HYPE/USDC)  
                let usdc_ask = self.store[7]; // @107 ask

                // Check both prices are available
                if usdh_bid == 0.0 || usdh_ask == 0.0 || usdc_bid == 0.0 || usdc_ask == 0.0 {
                    return Ok(());
                }

                let fee = 0.000672; // 0.0672% as decimal

                // Arbitrage opportunity 1: Buy USDC pair, sell USDH pair
                let profit_1 = ((usdh_bid - usdc_ask) * self.sz) - (self.sz * fee * 2.0);

                // Arbitrage opportunity 2: Buy USDH pair, sell USDC pair
                let profit_2 = ((usdc_bid - usdh_ask) * self.sz) - (self.sz * fee * 2.0);

                if profit_1 > 0.0 {
                    println!(
                        "Profitable: Buy @107 at {}, Sell @232 at {}, Profit: {}",
                        usdc_ask, usdh_bid, profit_1
                    );
                }
                if profit_2 > 0.0 {
                    println!(
                        "Profitable: Buy @232 at {}, Sell @107 at {}, Profit: {}",
                        usdh_ask, usdc_bid, profit_2
                    );
                }
            }
            "webData2" => {
                self.handle_clearing_house_state(response.data).await?;
            }
            _ => {}
        }

        Ok(())
    }

    async fn handle_bbo_update(&mut self, data: Value) -> Result<(), Box<dyn std::error::Error>> {
        let bbo: WsBbo = serde_json::from_value(data)?;

        let offset: usize = if bbo.coin == self.coin_a { 0 } else { 4 };

        if let (Some(best_bid), Some(best_ask)) = (&bbo.bbo[0], &bbo.bbo[1]) {
            println!(
                "BBO {} - Bid: {} @ {} | Ask: {} @ {}",
                bbo.coin, best_bid.sz, best_bid.px, best_ask.sz, best_ask.px
            );

            self.store[offset + 0] = best_bid.sz.parse().unwrap();
            self.store[offset + 1] = best_bid.px.parse().unwrap();
            self.store[offset + 2] = best_ask.sz.parse().unwrap();
            self.store[offset + 3] = best_ask.px.parse().unwrap();
        }

        Ok(())
    }

    async fn handle_clearing_house_state(
        &self,
        data: Value,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Ok(web_data) = serde_json::from_value::<WebData2>(data) {
            println!("\nBalances:");
            for balance in &web_data.spot_state.balances {
                if self.balance_coins.contains(&balance.coin) {
                    let available = balance.total.parse::<f64>().unwrap_or(0.0)
                        - balance.hold.parse::<f64>().unwrap_or(0.0);
                    println!(
                        "{}: total={}, available={:.6}",
                        balance.coin, balance.total, available
                    );
                }
            }
        }

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let hype_usdh = 232;
    let hype_usdc = 107;

    let coins = vec![format!("@{}", hype_usdh), format!("@{}", hype_usdc)];

    let balance_coins = vec!["USDC".to_string(), "USDH".to_string(), "HYPE".to_string()];

    let user_address = "".to_string();

    let mut client = HyperliquidWsClient::new(coins, balance_coins, user_address, 20.0);
    client.connect_and_subscribe().await?;

    Ok(())
}
