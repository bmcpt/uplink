#[doc = include_str!("../../README.md")]
use std::collections::HashMap;
use std::sync::Arc;
use std::thread;

use anyhow::Error;

use flume::{bounded, Receiver, Sender};
use log::error;
use tokio::task;

mod base;
mod collector;

pub mod config {
    pub use crate::base::{Config, Ota, Persistence, Stats};
}

use base::actions::tunshell::{Relay, TunshellSession};
use base::actions::Actions;
pub use base::actions::{Action, ActionResponse};
use base::mqtt::Mqtt;
use base::serializer::Serializer;
pub use base::{Config, Stream};
pub use base::{Package, Point};
pub use collector::simulator::Simulator;
use collector::systemstats::StatCollector;
pub use collector::tcpjson::{Bridge, Payload};
pub use disk::Storage;

pub struct Uplink {
    pub action_rx: Receiver<Action>,
    pub data_tx: Sender<Box<dyn Package>>,
    pub action_status: Stream<ActionResponse>,
}

impl Uplink {
    pub fn new(config: Arc<Config>) -> Result<Uplink, Error> {
        let enable_stats = config.stats.enabled;

        let (native_actions_tx, native_actions_rx) = bounded(10);
        let (tunshell_keys_tx, tunshell_keys_rx) = bounded(10);
        let (collector_tx, collector_rx) = bounded(10);
        let (bridge_actions_tx, bridge_actions_rx) = bounded(10);

        let action_status_topic = &config.streams.get("action_status").unwrap().topic;
        let action_status =
            Stream::new("action_status", action_status_topic, 1, collector_tx.clone());

        let mut mqtt = Mqtt::new(config.clone(), native_actions_tx);
        let mut serializer = Serializer::new(config.clone(), collector_rx, mqtt.client())?;
        let action_rx = bridge_actions_rx.clone();
        let data_tx = collector_tx.clone();
        let status_stream = action_status.clone();

        let rt = tokio::runtime::Runtime::new().unwrap();
        thread::spawn(move || {
            rt.block_on(async {
                task::spawn(async move {
                    if let Err(e) = serializer.start().await {
                        error!("Serializer stopped!! Error = {:?}", e);
                    }
                });

                task::spawn(async move {
                    mqtt.start().await;
                });

                if enable_stats {
                    let stat_collector = StatCollector::new(config.clone(), collector_tx.clone());
                    thread::spawn(move || stat_collector.start());
                }

                let tunshell_config = config.clone();
                let tunshell_session = TunshellSession::new(
                    tunshell_config,
                    Relay::default(),
                    false,
                    tunshell_keys_rx,
                    status_stream.clone(),
                );
                thread::spawn(move || tunshell_session.start());

                let controllers: HashMap<String, Sender<base::Control>> = HashMap::new();
                let mut actions = Actions::new(
                    config.clone(),
                    controllers,
                    native_actions_rx,
                    tunshell_keys_tx,
                    status_stream,
                    bridge_actions_tx,
                )
                .await;
                actions.start().await;
            })
        });

        Ok(Uplink { action_rx, data_tx, action_status })
    }
}
