//! FTX adapter implementation
use std::borrow::Borrow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

use metered::{time_source::StdInstant, *};
use serde_json::json;
use surf::Url;

use crate::market_data::{adapter::*, error::*, Market};
use crate::prelude::*;

#[derive(Default, Debug)]
pub struct Ftx {
    pub metrics: FtxMetrics,
}

#[derive(Default, Debug)]
pub struct FtxMetrics {
    throughput: Throughput<StdInstant, RefCell<metered::common::TxPerSec>>,
}

#[async_trait(?Send)]
impl RestMarketDataAdapter for Ftx {
    const NAME: &'static str = "ftx-rest";

    /// Fetches available markets on FTX
    async fn fetch_markets(&self) -> Result<Box<[Market]>, MarketDataError> {
        let client: surf::Client = surf::Config::new()
            .set_base_url(Url::parse("https://ftx.com").map_err(MarketDataError::with_source)?)
            .set_timeout(Some(Duration::from_secs(5)))
            .try_into()
            .map_err(MarketDataError::with_source)?;

        let mut res = client.get("/api/markets").await.unwrap();
        let body = res.body_string().await.unwrap();

        let root = serde_json::from_slice::<rest::ResponseRoot>(body.as_bytes())
            .map_err(MarketDataError::with_source)?;
        let result: &rest::ResponseResult = root.result.borrow();

        let rest::ResponseResult::Markets(markets) = result;

        Ok(markets
            .iter()
            .filter_map(|m| {
                <Market as TryFrom<&'_ rest::MarketInfo<'_>>>::try_from(m.borrow()).ok()
            })
            .collect())
    }

    async fn fetch_orderbook_snapshot(
        &self,
        symbol: &str,
    ) -> Result<PlainOrderbook<f64>, MarketDataError> {
        Ok(PlainOrderbook::<f64>::new())
    }
}

impl WsMarketDataAdapter for Ftx {
    fn throughput_metrics(&self) -> &Throughput<StdInstant, RefCell<metered::common::TxPerSec>> {
        &self.metrics.throughput
    }

    fn ws_url(&self) -> Box<str> {
        Box::from("wss://ftx.com/ws")
    }

    fn subscribe_msgs(&mut self, markets: &[&str]) -> Box<[String]> {
        markets
            .iter()
            .map(|market| {
                info!("Subscribing for {}", market);

                [
                    json!({"op": "subscribe", "channel": "orderbook", "market": market})
                        .to_string(),
                    json!({"op": "subscribe", "channel": "trades", "market": market}).to_string(),
                ]
            })
            .flatten()
            .collect()
    }

    /// Processes Websocket text message
    fn process_ws_msg(
        &self,
        msg: &str,
        markets: &mut HashMap<Box<str>, PlainOrderbook<f64>>,
    ) -> Result<Option<MarketEvent>, MarketDataError> {
        let start = std::time::Instant::now();
        let ws_msg = serde_json::from_slice::<ws::WsMsg>(msg.as_bytes());

        match ws_msg {
            Err(e) => {
                error!("ws_msg {}", msg);

                Err(MarketDataError {
                    source: Box::new(e),
                })
            }
            Ok(mut ws_msg) => {
                let data = ws_msg.data.to_mut();
                match data {
                    ws::Data::Trades(trades) => {
                        trace!("got trades = {:?}", trades);

                        let trades: Vec<_> = trades
                            .iter()
                            .filter_map(|trade| botvana::market::trade::Trade::try_from(trade).ok())
                            .collect();

                        trace!("parsing took = {:?}", start.elapsed());

                        Ok(Some(MarketEvent {
                            r#type: MarketEventType::Trades(
                                Box::from(ws_msg.market.unwrap()),
                                trades.into_boxed_slice(),
                            ),
                            timestamp: Utc::now(),
                        }))
                    }
                    ws::Data::Orderbook(ref mut orderbook_msg) => {
                        let orderbook = match orderbook_msg.action {
                            "partial" => {
                                let orderbook = PlainOrderbook {
                                    bids: PriceLevelsVec::from_tuples_vec_unsorted(
                                        &mut orderbook_msg.bids,
                                    ),
                                    asks: PriceLevelsVec::from_tuples_vec_unsorted(
                                        &mut orderbook_msg.asks,
                                    ),
                                    time: orderbook_msg.time,
                                };
                                info!("orderbook = {:?}", orderbook);
                                markets
                                    .insert(Box::from(ws_msg.market.unwrap()), orderbook.clone());
                                orderbook
                            }
                            "update" => {
                                let orderbook = markets.get_mut(ws_msg.market.unwrap()).unwrap();
                                orderbook.update_with_timestamp(
                                    &PriceLevelsVec::from_tuples_vec(&orderbook_msg.bids),
                                    &PriceLevelsVec::from_tuples_vec(&orderbook_msg.asks),
                                    orderbook_msg.time,
                                );
                                orderbook.clone()
                            }
                            action => {
                                return Err(MarketDataError::with_source(UnknownVariantError {
                                    variant: action.to_string(),
                                }))
                            }
                        };

                        Ok(Some(MarketEvent {
                            r#type: MarketEventType::OrderbookUpdate(
                                Box::from(ws_msg.market.unwrap()),
                                Box::new(orderbook),
                            ),
                            timestamp: Utc::now(),
                        }))
                        //info!("got orderbook = {:?}", orderbook);
                    }
                }
            }
        }
    }
}

pub mod rest {
    use std::borrow::Cow;

    use serde::Deserialize;

    #[derive(Debug, Clone, PartialEq, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ResponseRoot<'a> {
        pub success: bool,
        #[serde(borrow)]
        pub result: Cow<'a, ResponseResult<'a>>,
    }

    #[derive(Debug, Clone, PartialEq, Deserialize)]
    #[serde(untagged)]
    pub enum ResponseResult<'a> {
        #[serde(borrow)]
        Markets(Vec<Cow<'a, MarketInfo<'a>>>),
    }

    #[derive(Debug, Clone, PartialEq, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct MarketInfo<'a> {
        #[serde(borrow)]
        pub name: &'a str,
        #[serde(borrow)]
        pub base_currency: Option<&'a str>,
        #[serde(borrow)]
        pub quote_currency: Option<&'a str>,
        // pub quote_volume24h: Option<f64>,
        // pub change1h: Option<f64>,
        // pub change24h: Option<f64>,
        // pub change_bod: Option<f64>,
        // pub high_leverage_fee_exempt: bool,
        pub min_provide_size: f64,
        pub r#type: &'a str,
        #[serde(borrow)]
        pub underlying: Option<&'a str>,
        pub enabled: bool,
        // pub ask: Option<f64>,
        // pub bid: Option<f64>,
        // pub last: Option<f64>,
        pub post_only: Option<bool>,
        // pub price: Option<f64>,
        pub price_increment: f64,
        pub size_increment: f64,
        pub restricted: Option<bool>,
        //pub volume_usd24h: Option<f64>,
    }

    impl<'a> TryFrom<&'a MarketInfo<'a>> for botvana::market::Market {
        type Error = String;

        fn try_from(market: &'a MarketInfo<'a>) -> Result<Self, Self::Error> {
            let r#type = match market.r#type {
                "spot" => botvana::market::MarketType::Spot(botvana::market::SpotMarket {
                    base: market
                        .base_currency
                        .ok_or_else(|| "Missing base currency".to_string())?
                        .to_string(),
                    quote: market
                        .quote_currency
                        .ok_or_else(|| "Missing quote currency".to_string())?
                        .to_string(),
                }),
                "future" => botvana::market::MarketType::Futures,
                _ => return Err(format!("Invalid market type: {}", market.r#type)),
            };

            Ok(Self {
                name: market.name.to_string(),
                native_symbol: market.name.to_string(),
                size_increment: market.size_increment,
                price_increment: market.price_increment,
                r#type,
            })
        }
    }

    #[derive(Debug, Clone, PartialEq, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub enum MarketType {
        Spot,
        Future,
    }
}

pub mod ws {
    use std::borrow::Cow;

    use serde::Deserialize;

    /// FTX Websocket message
    #[derive(Debug, Clone, PartialEq, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct WsMsg<'a> {
        #[serde(borrow)]
        pub channel: &'a str,
        pub market: Option<&'a str>,
        #[serde(borrow)]
        pub data: Cow<'a, Data<'a>>,
        pub r#type: Option<String>,
    }

    /// FTX Data of websocket message
    #[derive(Debug, Clone, PartialEq, Deserialize)]
    #[serde(untagged)]
    pub enum Data<'a> {
        #[serde(borrow)]
        Trades(Box<[Trade<'a>]>),
        #[serde(borrow)]
        Orderbook(OrderbookMsg<'a>),
    }

    #[derive(Debug, Clone, PartialEq, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct OrderbookMsg<'a> {
        //pub checksum: i32,
        pub time: f64,
        pub bids: Box<[(f64, f64)]>,
        pub asks: Box<[(f64, f64)]>,
        pub action: &'a str,
    }

    #[derive(Debug, Clone, PartialEq, Deserialize)]
    pub struct Trade<'a> {
        pub id: i64,
        pub price: f64,
        pub side: &'a str,
        pub size: f64,
        pub liquidation: bool,
        pub time: &'a str,
    }

    impl<'a> TryFrom<&Trade<'a>> for botvana::market::trade::Trade {
        type Error = String;

        fn try_from(trade: &Trade<'a>) -> Result<Self, Self::Error> {
            Ok(Self {
                price: trade.price,
                size: trade.size,
                received_at: std::time::Instant::now(),
                time: trade
                    .time
                    .parse()
                    .map_err(|_| format!("error parsing: {}", trade.time))?,
            })
        }
    }
}
