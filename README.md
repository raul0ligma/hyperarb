# ArbBot

Simple arbitrage bot for HYPE/USDC vs HYPE/USDH on Hyperliquid. Trades when there's a price difference between the two pairs.

## What it does

Watches both @232 (HYPE/USDH) and @107 (HYPE/USDC) order books via WebSocket. When one pair is trading higher than the other, it buys on the cheaper side and sells on the expensive side.

## Setup

```bash
PRIVATE_KEY=your_key
USER_ADDRESS=0x...
FIXED_ORDER_SIZE=10.0
RUST_LOG=info
```

```bash
cargo run
```

## How it works

## How it works

- Monitors real-time prices via WebSocket
- Checks if profit > taker fees (both sides) + minimum profit
- Executes both trades simultaneously when profitable
- accounts for fees when calculating sell size: `sell_size = buy_size * (1 - taker_fee_rate)`
- Handles partial fills and rebalances USDC/USDH as needed

## Order Flow

1. **Detect spread**: Price difference between @232 and @107 exceeds profit threshold
2. **Execute simultaneously**: Buy HYPE on cheaper book, sell on expensive book using IOC limit orders
3. **IOC Limits**: Ioc Bulk orders using fee adjusted sz
4. **Handle fills**:
   - Both fill → completed
   - Partial fill → offload remaining position with 2% slippage using emergency orders, may cause loss if multiple offloads due to fees
5. **Rebalance inventory**: Convert excess USDC/USDH back to 50/50 split to maintain inventory
