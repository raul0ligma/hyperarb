use alloy::primitives::Address;
use anyhow::Result;
use envconfig::Envconfig;
use futures_util::{SinkExt, StreamExt};
use hyperqit::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

// WebSocket URL
const WS_URL: &str = "wss://api.hyperliquid.xyz/ws";

// Constants
const TAKER_FEE_RATE: f64 = 0.000672; // 0.0672%
const HYPE_USDH_ASSET_ID: u32 = 232; // @232
const HYPE_USDC_ASSET_ID: u32 = 107; // @107
const TRADE_SIZE_USDH: f64 = 20.0; // Constant trade size in USDH

// Asset metadata from Hyperliquid
const HYPE_SZ_DECIMALS: i32 = 2;
const USDH_SZ_DECIMALS: i32 = 2;

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

#[derive(Envconfig)]
pub struct Config {
    #[envconfig(from = "PRIVATE_KEY")]
    pub private_key: String,
    #[envconfig(from = "RUST_LOG")]
    pub log_level: String,
    #[envconfig(from = "USER_ADDRESS")]
    pub user_address: String,
}

#[derive(Debug, Clone)]
struct TradeParams {
    // Buy side (cheaper exchange)
    buy_asset_id: u32,
    buy_price: f64,
    buy_size: f64, // Exact HYPE amount to buy

    // Sell side (expensive exchange)
    sell_asset_id: u32,
    sell_price: f64,
    sell_size: f64, // Exact HYPE amount to sell (after fees)

    // Expected results
    expected_profit_usd: f64,
}

#[derive(Debug)]
struct BalanceState {
    usdc: f64,
    usdh: f64,
    hype: f64,
}

fn format_price_for_spot(price: f64, sz_decimals: i32) -> String {
    let max_price_decimals = MAX_DECIMALS_SPOT - sz_decimals;
    let formatted_price = format_significant_digits_and_decimals(price, max_price_decimals);
    formatted_price.to_string()
}

fn format_size_for_asset(size: f64, sz_decimals: i32) -> String {
    let formatted_size = format_decimals(size, sz_decimals);
    formatted_size.to_string()
}

pub struct HyperliquidWsClient {
    coins: Vec<String>,
    balance_coins: Vec<String>,
    user_address: String,
    store: [f64; 8],
    arbitrage_executor: ArbitrageExecutor,
}

impl HyperliquidWsClient {
    pub fn new(
        coins: Vec<String>,
        balance_coins: Vec<String>,
        user_address: String,
        arbitrage_executor: ArbitrageExecutor,
    ) -> Self {
        Self {
            coins,
            balance_coins,
            user_address,
            store: [0.0; 8],
            arbitrage_executor,
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
                // Always update prices first (never blocks)
                self.handle_bbo_update(response.data).await?;

                // Update arbitrage executor with latest prices
                self.arbitrage_executor.update_prices(self.store);

                // Try to execute arbitrage (skips if already running)
                let _ = self.arbitrage_executor.try_execute_arbitrage().await;
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

        let offset: usize = if bbo.coin == "@232" { 0 } else { 4 };

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
        &mut self,
        data: Value,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Ok(web_data) = serde_json::from_value::<WebData2>(data) {
            let mut usdc = 0.0;
            let mut usdh = 0.0;
            let mut hype = 0.0;

            for balance in &web_data.spot_state.balances {
                if self.balance_coins.contains(&balance.coin) {
                    let available = balance.total.parse::<f64>().unwrap_or(0.0)
                        - balance.hold.parse::<f64>().unwrap_or(0.0);

                    match balance.coin.as_str() {
                        "USDC" => usdc = available,
                        "USDH" => usdh = available,
                        "HYPE" => hype = available,
                        _ => {}
                    }
                }
            }

            // Update arbitrage executor balances
            self.arbitrage_executor.update_balances(usdc, usdh, hype);
        }

        Ok(())
    }
}

pub struct ArbitrageExecutor {
    client: HyperliquidClient,
    balances: BalanceState,
    store: [f64; 8],          // Price store from WebSocket
    is_executing: AtomicBool, // Execution guard
}

impl ArbitrageExecutor {
    pub fn new(client: HyperliquidClient) -> Self {
        Self {
            client,
            balances: BalanceState {
                usdc: 0.0,
                usdh: 0.0,
                hype: 0.0,
            },
            store: [0.0; 8],
            is_executing: AtomicBool::new(false),
        }
    }

    pub fn update_balances(&mut self, usdc: f64, usdh: f64, hype: f64) {
        self.balances.usdc = usdc;
        self.balances.usdh = usdh;
        self.balances.hype = hype;
        info!(
            "Updated balances: USDC={:.6}, USDH={:.6}, HYPE={:.6}",
            usdc, usdh, hype
        );
    }

    pub fn update_prices(&mut self, store: [f64; 8]) {
        self.store = store;
    }

    fn get_current_prices(&self) -> Result<(f64, f64, f64, f64)> {
        let usdh_bid = self.store[1]; // @232 bid (HYPE/USDH)
        let usdh_ask = self.store[3]; // @232 ask 
        let usdc_bid = self.store[5]; // @107 bid (HYPE/USDC)  
        let usdc_ask = self.store[7]; // @107 ask

        if usdh_bid == 0.0 || usdh_ask == 0.0 || usdc_bid == 0.0 || usdc_ask == 0.0 {
            return Err(anyhow::anyhow!("Prices not initialized"));
        }

        Ok((usdh_bid, usdh_ask, usdc_bid, usdc_ask))
    }

    pub fn check_arbitrage_opportunity(&self) -> Result<Option<TradeParams>> {
        let (usdh_bid, usdh_ask, usdc_bid, usdc_ask) = self.get_current_prices()?;

        // Calculate HYPE size for each direction
        let sz_usdc = TRADE_SIZE_USDH / usdc_ask; // HYPE from buying with USDC
        let sz_usdh = TRADE_SIZE_USDH / usdh_ask; // HYPE from buying with USDH

        // Your original simple formula - fees on USD amounts
        let profit_1 = ((usdh_bid - usdc_ask) * sz_usdc) - (TRADE_SIZE_USDH * TAKER_FEE_RATE * 2.0);
        let profit_2 = ((usdc_bid - usdh_ask) * sz_usdh) - (TRADE_SIZE_USDH * TAKER_FEE_RATE * 2.0);

        if profit_1 > 0.0 && profit_1 > profit_2 {
            // Adjust sell size for what you actually receive after buy fee
            let sell_sz = sz_usdc * (1.0 - TAKER_FEE_RATE);
            return Ok(Some(TradeParams {
                buy_asset_id: HYPE_USDC_ASSET_ID,
                buy_price: usdc_ask,
                buy_size: sz_usdc,
                sell_asset_id: HYPE_USDH_ASSET_ID,
                sell_price: usdh_bid,
                sell_size: sell_sz,
                expected_profit_usd: profit_1,
            }));
        }

        if profit_2 > 0.0 {
            // Adjust sell size for what you actually receive after buy fee
            let sell_sz = sz_usdh * (1.0 - TAKER_FEE_RATE);
            return Ok(Some(TradeParams {
                buy_asset_id: HYPE_USDH_ASSET_ID,
                buy_price: usdh_ask,
                buy_size: sz_usdh,
                sell_asset_id: HYPE_USDC_ASSET_ID,
                sell_price: usdc_bid,
                sell_size: sell_sz,
                expected_profit_usd: profit_2,
            }));
        }

        Ok(None)
    }

    fn validate_sufficient_balance(&self, trade_params: &TradeParams) -> Result<()> {
        // Check if we have enough USDH or USDC for the buy side
        if trade_params.buy_asset_id == HYPE_USDH_ASSET_ID {
            if self.balances.usdh < TRADE_SIZE_USDH {
                return Err(anyhow::anyhow!(
                    "Insufficient USDH balance: need {:.6}, have {:.6}",
                    TRADE_SIZE_USDH,
                    self.balances.usdh
                ));
            }
        } else {
            if self.balances.usdc < TRADE_SIZE_USDH {
                return Err(anyhow::anyhow!(
                    "Insufficient USDC balance: need {:.6}, have {:.6}",
                    TRADE_SIZE_USDH,
                    self.balances.usdc
                ));
            }
        }

        Ok(())
    }

    fn calculate_trade_params(&self, trade_params: TradeParams) -> Result<TradeParams> {
        let hype_amount = TRADE_SIZE_USDH / trade_params.buy_price;
        info!(
            "Trade params: Buy {:.6} HYPE for ${} at ${:.3} on asset {}, Sell {:.6} HYPE at ${:.3} on asset {}",
            hype_amount,
            TRADE_SIZE_USDH,
            trade_params.buy_price,
            trade_params.buy_asset_id,
            hype_amount,
            trade_params.sell_price,
            trade_params.sell_asset_id
        );
        info!("Expected profit: ${:.6}", trade_params.expected_profit_usd);

        Ok(trade_params)
    }

    fn create_bulk_order(&self, trade_params: &TradeParams) -> Result<BulkOrder> {
        // Format the already calculated sizes and prices
        let buy_size_str = format_size_for_asset(trade_params.buy_size, HYPE_SZ_DECIMALS);
        let sell_size_str = format_size_for_asset(trade_params.sell_size, HYPE_SZ_DECIMALS);
        let buy_price_str = format_price_for_spot(trade_params.buy_price, HYPE_SZ_DECIMALS);
        let sell_price_str = format_price_for_spot(trade_params.sell_price, HYPE_SZ_DECIMALS);

        let orders = vec![
            OrderRequest {
                asset: 10000 + trade_params.buy_asset_id,
                is_buy: true,
                limit_px: buy_price_str,
                sz: buy_size_str,
                reduce_only: false,
                order_type: OrderType::Limit(Limit { tif: "Ioc".into() }),
                cloid: None,
            },
            OrderRequest {
                asset: 10000 + trade_params.sell_asset_id,
                is_buy: false,
                limit_px: sell_price_str,
                sz: sell_size_str,
                reduce_only: false,
                order_type: OrderType::Limit(Limit { tif: "Ioc".into() }),
                cloid: None,
            },
        ];

        Ok(BulkOrder {
            orders,
            grouping: "na".to_string(),
        })
    }

    pub async fn try_execute_arbitrage(&self) -> Result<()> {
        // Check if already executing
        if self.is_executing.load(Ordering::Relaxed) {
            info!("Arbitrage already running, skipping this opportunity");
            return Ok(());
        }

        // Set execution flag
        self.is_executing.store(true, Ordering::Relaxed);

        let result = self.execute_arbitrage_internal().await;

        println!("{:?}", result);
        // Clear execution flag
        self.is_executing.store(false, Ordering::Relaxed);

        result
    }

    async fn execute_arbitrage_internal(&self) -> Result<()> {
        info!(
            "Checking for arbitrage opportunity with constant size: ${} USDH",
            TRADE_SIZE_USDH
        );

        // Use latest WebSocket state
        let opportunity = self
            .check_arbitrage_opportunity()?
            .ok_or_else(|| anyhow::anyhow!("No profitable arbitrage opportunity found"))?;
        println!("here=======");
        self.validate_sufficient_balance(&opportunity)?;
        let trade_params = self.calculate_trade_params(opportunity)?;
        let bulk_order = self.create_bulk_order(&trade_params)?;

        print!("perparewd");
        info!("Bulk order prepared: {:?}", bulk_order);
        info!("Expected profit: ${:.6}", trade_params.expected_profit_usd);

        // Execute order (commented out for now)
        let result = self.client.create_position_raw(bulk_order).await.unwrap();
        // info!("Arbitrage executed successfully: {:?}", result);

        match result {
            ExchangeOrderResponse::Order(resp) => {
                for (i, status) in resp.statuses.iter().enumerate() {
                    match status {
                        OrderStatus::Error(er) => {
                            error!("arbitrage failed {} {}", i, er)
                        }
                        OrderStatus::Filled(out) => {
                            error!("leg filled {:?}", out)
                        }
                        _ => {
                            error!("invalid order status {:?}", status)
                        }
                    }
                }
            }
            _ => {
                info!("unknown result {:?}", result)
            }
        }

        panic!("trade done");

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::init_from_env().unwrap();
    let signer = Box::new(hyperqit::LocalWallet::signer(config.private_key));
    let user_address: Address = config.user_address.parse().unwrap();
    let executor = HyperliquidClient::new(Network::Mainnet, signer, user_address);

    // Create arbitrage executor
    let arbitrage_executor = ArbitrageExecutor::new(executor);

    // Create WebSocket client with arbitrage executor
    let hype_usdh = 232;
    let hype_usdc = 107;
    let coins = vec![format!("@{}", hype_usdh), format!("@{}", hype_usdc)];
    let balance_coins = vec!["USDC".to_string(), "USDH".to_string(), "HYPE".to_string()];

    let mut ws_client = HyperliquidWsClient::new(
        coins,
        balance_coins,
        config.user_address,
        arbitrage_executor,
    );

    println!("Starting Hyperliquid arbitrage bot...");
    ws_client.connect_and_subscribe().await.unwrap();

    Ok(())
}
