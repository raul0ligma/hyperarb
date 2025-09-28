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
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

// WebSocket URL
const WS_URL: &str = "wss://api.hyperliquid.xyz/ws";

// Constants
const TAKER_FEE_RATE: f64 = 0.000672; // 0.0672%
const HYPE_USDH_ASSET_ID: u32 = 232; // @232
const HYPE_USDC_ASSET_ID: u32 = 107; // @107

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
    #[envconfig(from = "FIXED_ORDER_SIZE")]
    pub order_sz: f64,
    #[envconfig(from = "MIN_PROFIT_USD")]
    pub min_profit: f64,
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

#[derive(Clone)]
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

#[derive(Clone)]
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

                // CHANGED: Now async execution but still non-blocking WebSocket
                let executor = self.arbitrage_executor.clone();
                tokio::spawn(async move {
                    let _ = executor.try_execute_arbitrage().await;
                });
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

#[derive(Clone)]
pub struct ArbitrageExecutor {
    client: Arc<HyperliquidClient>, // CHANGED: Wrapped in Arc for cloning
    balances: BalanceState,
    store: [f64; 8],                // Price store from WebSocket
    is_executing: Arc<Mutex<bool>>, // Change to Mutex
    // CHANGED: Wrapped in Arc for cloning
    order_sz: f64,
    min_profit: f64,
}

impl ArbitrageExecutor {
    pub fn new(client: HyperliquidClient, sz: f64, min_profit: f64) -> Self {
        Self {
            client: Arc::new(client), // CHANGED: Wrap client in Arc
            balances: BalanceState {
                usdc: 0.0,
                usdh: 0.0,
                hype: 0.0,
            },
            store: [0.0; 8],
            min_profit,
            order_sz: sz,
            is_executing: Arc::new(Mutex::new(false)), // CHANGED: Wrapped in Arc
        }
    }

    pub fn update_balances(&mut self, usdc: f64, usdh: f64, hype: f64) {
        self.balances.usdc = usdc;
        self.balances.usdh = usdh;
        self.balances.hype = hype;
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

    // CHANGED: Added dynamic sizing function
    fn has_sufficient_liquidity(&self, sz: f64) -> bool {
        let usdh_bid_size = self.store[0]; // @232 bid size
        let usdh_ask_size = self.store[2]; // @232 ask size  
        let usdc_bid_size = self.store[4]; // @107 bid size
        let usdc_ask_size = self.store[6]; // @107 ask size

        let (usdh_bid_price, usdh_ask_price, usdc_bid_price, usdc_ask_price) =
            match self.get_current_prices() {
                Ok(prices) => prices,
                Err(_) => return false,
            };

        // Calculate required HYPE amounts for 2x our trade size
        let required_usdh_bid_hype = (sz * 2.0) / usdh_bid_price;
        let required_usdh_ask_hype = (sz * 2.0) / usdh_ask_price;
        let required_usdc_bid_hype = (sz * 2.0) / usdc_bid_price;
        let required_usdc_ask_hype = (sz * 2.0) / usdc_ask_price;

        // All 4 levels must have at least 2x our trade amount
        usdh_bid_size >= required_usdh_bid_hype
            && usdh_ask_size >= required_usdh_ask_hype
            && usdc_bid_size >= required_usdc_bid_hype
            && usdc_ask_size >= required_usdc_ask_hype
    }

    // CHANGED: Keep original formula, just add dynamic sizing
    pub fn check_arbitrage_opportunity(&self) -> Result<Option<TradeParams>> {
        // First check if all levels have sufficient liquidity
        if !self.has_sufficient_liquidity(self.order_sz) {
            return Ok(None);
        }

        let (usdh_bid, usdh_ask, usdc_bid, usdc_ask) = self.get_current_prices()?;

        // Direction 1: Buy USDC, Sell USDH
        let sz_usdc = self.order_sz / usdc_ask;
        let profit_1 = ((usdh_bid - usdc_ask) * sz_usdc) - (self.order_sz * TAKER_FEE_RATE * 2.0);

        // Direction 2: Buy USDH, Sell USDC
        let sz_usdh = self.order_sz / usdh_ask;
        let profit_2 = ((usdc_bid - usdh_ask) * sz_usdh) - (self.order_sz * TAKER_FEE_RATE * 2.0);

        // Check minimum profit threshold
        if profit_1 >= self.min_profit && profit_1 > profit_2 {
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

        if profit_2 >= self.min_profit {
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
        let required_usd = trade_params.buy_size * trade_params.buy_price;

        let available_balance = if trade_params.buy_asset_id == HYPE_USDH_ASSET_ID {
            self.balances.usdh
        } else {
            self.balances.usdc
        };

        if available_balance < required_usd {
            return Err(anyhow::anyhow!(
                "Insufficient balance: need {:.2}, have {:.2}",
                required_usd,
                available_balance
            ));
        }

        Ok(())
    }

    // CHANGED: Completely rewritten for non-blocking execution
    pub async fn try_execute_arbitrage(&self) -> Result<()> {
        let _guard = match self.is_executing.try_lock() {
            Ok(guard) => guard,
            Err(_) => return Ok(()), // Already executing
        };

        // All validation and execution while holding lock
        let opportunity = match self.check_arbitrage_opportunity() {
            Ok(Some(opp)) => opp,
            _ => return Ok(()),
        };

        if let Err(_) = self.validate_sufficient_balance(&opportunity) {
            return Ok(());
        }

        // Execute synchronously while holding lock
        Self::execute_trade_async(self.client.clone(), opportunity).await?;

        Ok(())
    }

    // CHANGED: New static method for async execution
    async fn execute_trade_async(
        client: Arc<HyperliquidClient>,
        trade_params: TradeParams,
    ) -> Result<()> {
        info!(
            "Executing: Buy {:.4} HYPE @ ${:.3}, Sell {:.4} HYPE @ ${:.3}, Expected: ${:.3}",
            trade_params.buy_size,
            trade_params.buy_price,
            trade_params.sell_size,
            trade_params.sell_price,
            trade_params.expected_profit_usd
        );

        let bulk_order = Self::create_bulk_order_static(&trade_params)?;
        let result = client.create_position_raw(bulk_order).await?;
        Self::handle_execution_result(client, result, &trade_params).await
    }

    // CHANGED: Static version of create_bulk_order
    fn create_bulk_order_static(trade_params: &TradeParams) -> Result<BulkOrder> {
        let orders = vec![
            OrderRequest {
                asset: 10000 + trade_params.buy_asset_id,
                is_buy: true,
                limit_px: format_price_for_spot(trade_params.buy_price, HYPE_SZ_DECIMALS),
                sz: format_size_for_asset(trade_params.buy_size, HYPE_SZ_DECIMALS),
                reduce_only: false,
                order_type: OrderType::Limit(Limit { tif: "Ioc".into() }),
                cloid: None,
            },
            OrderRequest {
                asset: 10000 + trade_params.sell_asset_id,
                is_buy: false,
                limit_px: format_price_for_spot(trade_params.sell_price, HYPE_SZ_DECIMALS),
                sz: format_size_for_asset(trade_params.sell_size, HYPE_SZ_DECIMALS),
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

    // CHANGED: Comprehensive exit handling with floating point precision fixes
    async fn handle_execution_result(
        client: Arc<HyperliquidClient>,
        result: ExchangeOrderResponse,
        original_params: &TradeParams,
    ) -> Result<()> {
        info!("exchange order {:?}", result);
        match result {
            ExchangeOrderResponse::Order(resp) => {
                let mut buy_filled: Option<f64> = None;
                let mut sell_filled: Option<f64> = None;

                for (i, status) in resp.statuses.iter().enumerate() {
                    match status {
                        OrderStatus::Filled(filled) => {
                            let size: f64 = filled.total_sz.parse()?;
                            if i == 0 {
                                buy_filled = Some(size);
                            } else {
                                sell_filled = Some(size);
                            }
                            info!("Leg {} filled: {:.4} HYPE", i, size);
                        }
                        OrderStatus::Error(err) => {
                            error!("Leg {} failed: {}", i, err);
                        }
                        _ => {}
                    }
                }

                match (buy_filled, sell_filled) {
                    (Some(buy_size), Some(sell_size)) => {
                        // Both filled, check if complete or partial with floating point tolerance
                        let buy_complete = (buy_size - original_params.buy_size).abs() < 0.0001;
                        let sell_complete = (sell_size - original_params.sell_size).abs() < 0.0001;

                        if buy_complete && sell_complete {
                            info!("Complete arbitrage success");
                        } else {
                            // Calculate expected sell amount from actual buy
                            let expected_sell_size = buy_size * (1.0 - TAKER_FEE_RATE);
                            let remaining: f64 = expected_sell_size - sell_size;

                            // Only offload if remaining is significant (avoid dust)
                            // size should be greater than $10 to offload
                            if remaining * original_params.sell_price > 10.5 {
                                warn!("Partial sell, offloading remaining: {:.4} HYPE", remaining);
                                Self::offload_position(
                                    client,
                                    remaining,
                                    original_params.buy_asset_id,
                                    false,
                                    original_params.buy_price,
                                )
                                .await?;
                            } else {
                                warn!("Partial sell, offloading remaining: {:.4} HYPE", remaining);
                                info!("Arbitrage complete with minor rounding difference");
                                let latest_spot = client.get_user_spot_info(None).await?;
                                let mut usdc_bal = 0.0;
                                let mut usdh_bal = 0.0;
                                for balance in &latest_spot.balances {
                                    let available = balance.total.parse::<f64>().unwrap_or(0.0)
                                        - balance.hold.parse::<f64>().unwrap_or(0.0);

                                    match balance.coin.as_str() {
                                        "USDC" => usdc_bal = available,
                                        "USDH" => usdh_bal = available,
                                        _ => {}
                                    }
                                }

                                // if we bought using USDH and sold USDC, we will get have to split usdc
                                let total_stable = usdc_bal + usdh_bal;
                                let target_each = total_stable / 2.0;

                                let (is_buy_usdh, amount_to_trade) = if usdc_bal > usdh_bal {
                                    // Have excess USDC, need to buy USDH
                                    (true, (usdc_bal - target_each))
                                } else {
                                    // Have excess USDH, need to sell USDH for USDC
                                    (false, (usdh_bal - target_each))
                                };

                                if amount_to_trade < 11.0 {
                                    // do not trade if it's too little
                                    return Ok(());
                                }

                                let orders = vec![OrderRequest {
                                    asset: 10230, // USDH/USDC pair asset ID
                                    is_buy: is_buy_usdh,
                                    limit_px: if is_buy_usdh { "1.0005" } else { "0.99954" }
                                        .to_string(),
                                    sz: format_size_for_asset(amount_to_trade, USDH_SZ_DECIMALS),
                                    reduce_only: false,
                                    order_type: OrderType::Limit(Limit { tif: "Ioc".into() }),
                                    cloid: None,
                                }];

                                let result = client
                                    .create_position_raw(BulkOrder {
                                        orders,
                                        grouping: "na".to_string(),
                                    })
                                    .await?;

                                info!("rebalance result {:?}", result)
                            }
                        }
                    }
                    (Some(buy_size), None) => {
                        // Buy filled, sell failed - offload full received amount
                        let actual_received = buy_size * (1.0 - TAKER_FEE_RATE);
                        warn!(
                            "Buy filled, sell failed - offloading: {:.4} HYPE",
                            actual_received
                        );
                        Self::offload_position(
                            client,
                            actual_received,
                            original_params.buy_asset_id,
                            false,
                            original_params.sell_price,
                        )
                        .await?;
                    }
                    (None, Some(sell_sz)) => {
                        error!("invalid case, found sell_sz {}", sell_sz);
                    }
                    (None, None) => {
                        info!("No fills, no action needed");
                    }
                }
            }
            _ => {
                error!("Unexpected response type");
            }
        }

        Ok(())
    }

    // CHANGED: Renamed from emergency_order to offload_position with precision handling
    async fn offload_position(
        client: Arc<HyperliquidClient>,
        size: f64,
        asset_id: u32,
        is_buy: bool,
        original_price: f64,
    ) -> Result<()> {
        // Apply slippage: buy higher, sell lower for guaranteed execution
        let slippage_price = if is_buy {
            original_price * 1.02 // 1% higher for offload buy
        } else {
            original_price * 0.98 // 1% lower for offload sell
        };

        let offload_order = BulkOrder {
            orders: vec![OrderRequest {
                asset: 10000 + asset_id,
                is_buy,
                limit_px: format_price_for_spot(slippage_price, HYPE_SZ_DECIMALS),
                sz: format_size_for_asset(size, HYPE_SZ_DECIMALS),
                reduce_only: false,
                order_type: OrderType::Limit(Limit { tif: "Ioc".into() }),
                cloid: None,
            }],
            grouping: "na".to_string(),
        };

        let action = if is_buy { "buy" } else { "sell" };
        info!(
            "Offload {}: {:.4} HYPE @ ${:.3}",
            action, size, slippage_price
        );
        let _result = client.create_position_raw(offload_order).await?;
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
    let arbitrage_executor = ArbitrageExecutor::new(executor, config.order_sz, config.min_profit);

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
