#[macro_use]
extern crate log;

use std::{fs, io, thread};

use crossbeam_channel as channel;
use derive_more::From;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use structopt::StructOpt;

mod collector;
mod serializer;

#[derive(StructOpt, Debug)]
#[structopt(name = "uplink", about = "collect, batch, compress, publish")]
pub struct CommandLine {
    #[structopt(short = "i", help = "Device id")]
    device_id: String,
    #[structopt(short = "c", help = "Config file path")]
    config_path: String,
    #[structopt(short = "v", help = "version", default_value = "v1")]
    version: String,
    #[structopt(short = "a", help = "certs")]
    certs_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelConfig {
    pub topic: String,
    pub buf_size: u16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub device_id: String,
    pub broker: String,
    pub port: u16,
    pub channels: HashMap<String, ChannelConfig>,
    pub key: Option<PathBuf>,
    pub ca: Option<PathBuf>,
}

#[derive(Debug, From)]
pub enum InitError {
    Toml(toml::de::Error),
    File { name: String, err: io::Error },
}

/// Reads config file to generate config struct and replaces places holders
/// like bike id and data version
fn init_config(commandline: CommandLine) -> Result<Config, InitError> {
    let config = fs::read_to_string(&commandline.config_path).map_err(|err| InitError::File {
        name: commandline.config_path.clone(),
        err,
    })?;

    let device_id = commandline.device_id.trim();
    let version = commandline.version.trim();

    let mut config: Config = toml::from_str(&config)?;

    config.ca = Some(commandline.certs_dir.join(device_id).join("roots.pem"));
    config.key = Some(
        commandline
            .certs_dir
            .join(device_id)
            .join("rsa_private.pem"),
    );

    config.device_id = str::replace(&config.device_id, "{device_id}", device_id);
    for config in config.channels.values_mut() {
        let topic = str::replace(&config.topic, "{device_id}", device_id);
        let topic = str::replace(&topic, "{version}", version);

        config.topic = topic
    }

    Ok(config)
}

fn main() -> Result<(), InitError> {
    pretty_env_logger::init();
    let commandline: CommandLine = StructOpt::from_args();
    let config = init_config(commandline)?;

    let (collector_tx, collector_rx) = channel::bounded(10);

    // create collector of simulator type
    let simulator = collector::simulator::Simulator::new().unwrap();
    let mut simulator = collector::Collector::new(simulator, collector_tx.clone());

    // create can collector
    // let can = collector::can::Can::new("vcan0").unwrap();
    // let mut can = collector::Collector::new(can, collector_tx);

    let mut serializer = serializer::Serializer::new(config, collector_rx);

    thread::spawn(move || {
        serializer.start();
    });

    // thread::spawn(move || {
    //  can.start();
    // });

    simulator.start();
    Ok(())
}
