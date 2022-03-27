use crate::base::{Config, Package};

use bytes::Bytes;
use disk::Storage;
use flume::{Receiver, RecvError};
use log::{error, info};
use rumqttc::*;
use serde::Serialize;
use std::io;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::{select, time};

#[derive(Error, Debug)]
pub enum Error {
    #[error("Collector recv error {0}")]
    Collector(#[from] RecvError),
    #[error("Serde error {0}")]
    Serde(#[from] serde_json::Error),
    #[error("Io error {0}")]
    Io(#[from] io::Error),
    #[error("Mqtt client error {0}")]
    Client(#[from] MqttError),
    #[error("Storage is disabled/missing")]
    MissingPersistence,
}

enum Status {
    Normal,
    SlowEventloop(Publish),
    EventLoopReady,
    EventLoopCrash(Publish),
}

#[cfg(test)]
mod mock {
    use super::*;

    #[derive(Clone)]
    pub struct MockClient {
        pub net_tx: flume::Sender<Request>,
    }

    #[derive(thiserror::Error, Debug)]
    pub enum Error {
        #[error("Net Send error {0}")]
        Request(#[from] flume::SendError<Request>),
        #[error("Net TrySend error {0}")]
        TryRequest(#[from] flume::TrySendError<Request>),
    }

    impl MockClient {
        pub async fn publish<S, V>(
            &self,
            topic: S,
            qos: QoS,
            retain: bool,
            payload: V,
        ) -> Result<(), Error>
        where
            S: Into<String>,
            V: Into<Vec<u8>>,
        {
            let mut publish = Publish::new(topic, qos, payload);
            publish.retain = retain;
            let publish = Request::Publish(publish);
            self.net_tx.send_async(publish).await?;
            Ok(())
        }

        pub fn try_publish<S, V>(
            &self,
            topic: S,
            qos: QoS,
            retain: bool,
            payload: V,
        ) -> Result<(), Error>
        where
            S: Into<String>,
            V: Into<Vec<u8>>,
        {
            let mut publish = Publish::new(topic, qos, payload);
            publish.retain = retain;
            let publish = Request::Publish(publish);
            self.net_tx.try_send(publish)?;
            Ok(())
        }

        pub async fn publish_bytes<S>(
            &self,
            topic: S,
            qos: QoS,
            retain: bool,
            payload: Bytes,
        ) -> Result<(), Error>
        where
            S: Into<String>,
        {
            let mut publish = Publish::from_bytes(topic, qos, payload);
            publish.retain = retain;
            let publish = Request::Publish(publish);
            self.net_tx.send_async(publish).await?;
            Ok(())
        }
    }
}

#[cfg(not(test))]
pub type MqttClient = AsyncClient;
#[cfg(test)]
pub type MqttClient = mock::MockClient;
#[cfg(not(test))]
pub type MqttError = ClientError;
#[cfg(test)]
pub type MqttError = mock::Error;

/// The uplink Serializer is the component that deals with sending data to the Bytebeam platform.
/// In case of network issues, the Serializer enters various states depending on severeness, managed by `Serializer::start()`.                                                                                       
///
/// ```text
///        ┌───────────────────┐
///        │Serializer::start()│
///        └─────────┬─────────┘
///                  │
///                  │ State transitions happen
///                  │ within the loop{}             Load data in Storage from
///                  │                               previouse sessions/iterations                  AsyncClient has crashed
///          ┌───────▼──────┐                       ┌─────────────────────┐                      ┌───────────────────────┐
///          │EventLoopReady├───────────────────────►Serializer::catchup()├──────────────────────►EventLoopCrash(publish)│
///          └───────▲──────┘                       └──────────┬──────────┘                      └───────────┬───────────┘
///                  │                                         │                                             │
///                  │                                         │ No more data left in Storage                │
///                  │                                         │                                             │
///     ┌────────────┴────────────┐                        ┌───▼──┐                             ┌────────────▼─────────────┐
///     │Serializer::disk(publish)│                        │Normal│                             │Serializer::crash(publish)├──┐
///     └────────────▲────────────┘                        └───┬──┘                             └─────────────────────────▲┘  │
///                  │                                         │                                 Write all data to Storage└───┘
///                  │                                         │
///                  │                                         │
///      ┌───────────┴──────────┐                   ┌──────────▼─────────┐
///      │SlowEventloop(publish)◄───────────────────┤Serializer::normal()│
///      └──────────────────────┘                   └────────────────────┘
///       Slow Network,                             Forward all data to Bytebeam,
///       save to Storage before forwarding         through AsyncClient
///
///```
pub struct Serializer {
    config: Arc<Config>,
    collector_rx: Receiver<Box<dyn Package>>,
    client: MqttClient,
    storage: Option<Storage>,
    metrics: Metrics,
}

impl Serializer {
    pub fn new(
        config: Arc<Config>,
        collector_rx: Receiver<Box<dyn Package>>,
        client: MqttClient,
    ) -> Result<Serializer, Error> {
        let metrics_config = config.streams.get("metrics").unwrap();
        let metrics = Metrics::new(&metrics_config.topic);

        let storage = match &config.persistence {
            Some(persistence) => {
                let storage = Storage::new(
                    &persistence.path,
                    persistence.max_file_size,
                    persistence.max_file_count,
                )?;
                Some(storage)
            }
            None => None,
        };

        Ok(Serializer { config, collector_rx, client, storage, metrics })
    }

    /// Write all data received, from here-on, to disk only.
    async fn crash(&mut self, mut publish: Publish) -> Result<Status, Error> {
        let storage = match &mut self.storage {
            Some(s) => s,
            None => return Err(Error::MissingPersistence),
        };
        // Write failed publish to disk first
        publish.pkid = 1;

        loop {
            let data = self.collector_rx.recv_async().await?;
            let topic = data.topic();
            let payload = data.serialize();

            let mut publish = Publish::new(topic.as_ref(), QoS::AtLeastOnce, payload);
            publish.pkid = 1;

            if let Err(e) = publish.write(&mut storage.writer()) {
                error!("Failed to fill write buffer during bad network. Error = {:?}", e);
                continue;
            }

            match storage.flush_on_overflow() {
                Ok(_) => {}
                Err(e) => {
                    error!(
                        "Failed to flush write buffer to disk during bad network. Error = {:?}",
                        e
                    );
                    continue;
                }
            }
        }
    }

    /// Write new data to disk until back pressure due to slow n/w is resolved
    async fn disk(&mut self, publish: Publish) -> Result<Status, Error> {
        let storage = match &mut self.storage {
            Some(s) => s,
            None => return Err(Error::MissingPersistence),
        };
        info!("Switching to slow eventloop mode!!");

        // Note: self.client.publish() is executing code before await point
        // in publish method every time. Verify this behaviour later
        let publish =
            self.client.publish(&publish.topic, QoS::AtLeastOnce, false, &publish.payload[..]);
        tokio::pin!(publish);

        loop {
            select! {
                data = self.collector_rx.recv_async() => {
                      let data = data?;
                      if let Some((errors, count)) = data.anomalies() {
                        self.metrics.add_errors(errors, count);
                      }

                      let topic = data.topic();
                      let payload = data.serialize();
                      let payload_size = payload.len();
                      let mut publish = Publish::new(topic.as_ref(), QoS::AtLeastOnce, payload);
                      publish.pkid = 1;

                      match publish.write(&mut storage.writer()) {
                           Ok(_) => self.metrics.add_total_disk_size(payload_size),
                           Err(e) => {
                               error!("Failed to fill disk buffer. Error = {:?}", e);
                               continue
                           }
                      }

                      match storage.flush_on_overflow() {
                            Ok(deleted) => if deleted.is_some() {
                                self.metrics.increment_lost_segments();
                            },
                            Err(e) => {
                                error!("Failed to flush disk buffer. Error = {:?}", e);
                                continue
                            }
                      }
                }
                o = &mut publish => {
                    o?;
                    return Ok(Status::EventLoopReady)
                }
            }
        }
    }

    /// Write new collector data to disk while sending existing data on
    /// disk to mqtt eventloop. Collector rx is selected with blocking
    /// `publish` instead of `try publish` to ensure that transient back
    /// pressure due to a lot of data on disk doesn't switch state to
    /// `Status::SlowEventLoop`
    async fn catchup(&mut self) -> Result<Status, Error> {
        let storage = match &mut self.storage {
            Some(s) => s,
            None => return Err(Error::MissingPersistence),
        };
        info!("Switching to catchup mode!!");

        let max_packet_size = self.config.max_packet_size;
        let client = self.client.clone();

        // Done reading all the pending files
        if storage.reload_on_eof().unwrap() {
            return Ok(Status::Normal);
        }

        let publish = match read(storage.reader(), max_packet_size) {
            Ok(Packet::Publish(publish)) => publish,
            Ok(packet) => unreachable!("{:?}", packet),
            Err(e) => {
                error!("Failed to read from storage. Forcing into Normal mode. Error = {:?}", e);
                return Ok(Status::Normal);
            }
        };

        let send = send_publish(client, publish.topic, publish.payload);
        tokio::pin!(send);

        loop {
            select! {
                data = self.collector_rx.recv_async() => {
                      let data = data?;
                      if let Some((errors, count)) = data.anomalies() {
                        self.metrics.add_errors(errors, count);
                      }

                      let topic = data.topic();
                      let payload = data.serialize();
                      let payload_size = payload.len();
                      let mut publish = Publish::new(topic.as_ref(), QoS::AtLeastOnce, payload);
                      publish.pkid = 1;

                      match publish.write(&mut storage.writer()) {
                           Ok(_) => self.metrics.add_total_disk_size(payload_size),
                           Err(e) => {
                               error!("Failed to fill disk buffer. Error = {:?}", e);
                               continue
                           }
                      }

                      match storage.flush_on_overflow() {
                            Ok(deleted) => if deleted.is_some() {
                                self.metrics.increment_lost_segments();
                            },
                            Err(e) => {
                                error!("Failed to flush write buffer to disk during catchup. Error = {:?}", e);
                                continue
                            }
                      }
                }
                o = &mut send => {
                    // Send failure implies eventloop crash. Switch state to
                    // indefinitely write to disk to not loose data
                    let client = match o {
                        Ok(c) => c,
                        Err(MqttError::Request(request)) => match request.into_inner() {
                            Request::Publish(publish) => return Ok(Status::EventLoopCrash(publish)),
                            request => unreachable!("{:?}", request),
                        },
                        Err(e) => return Err(e.into()),
                    };

                    match storage.reload_on_eof() {
                        // Done reading all pending files
                        Ok(true) => return Ok(Status::Normal),
                        Ok(false) => {},
                        Err(e) => {
                            error!("Failed to reload storage. Forcing into Normal mode. Error = {:?}", e);
                            return Ok(Status::Normal)
                        }
                    }

                    let publish = match read(storage.reader(), max_packet_size) {
                        Ok(Packet::Publish(publish)) => publish,
                        Ok(packet) => unreachable!("{:?}", packet),
                        Err(e) => {
                            error!("Failed to read from storage. Forcing into Normal mode. Error = {:?}", e);
                            return Ok(Status::Normal)
                        }
                    };


                    let payload = publish.payload;
                    let payload_size = payload.len();
                    self.metrics.sub_total_disk_size(payload_size);
                    self.metrics.add_total_sent_size(payload_size);
                    send.set(send_publish(client, publish.topic, payload));
                }
            }
        }
    }

    async fn normal(&mut self) -> Result<Status, Error> {
        info!("Switching to normal mode!!");
        let mut interval = time::interval(time::Duration::from_secs(10));

        loop {
            let failed = select! {
                data = self.collector_rx.recv_async() => {
                    let data = data?;

                    // Extract anomalies detected by package during collection
                    if let Some((errors, count)) = data.anomalies() {
                        self.metrics.add_errors(errors, count);
                    }

                    let topic = data.topic();
                    let payload = data.serialize();
                    let payload_size = payload.len();
                    match self.client.try_publish(topic.as_ref(), QoS::AtLeastOnce, false, payload) {
                        Ok(_) => {
                            self.metrics.add_total_sent_size(payload_size);
                            continue;
                        }
                        Err(MqttError::TryRequest(request)) => request,
                        Err(e) => return Err(e.into()),
                    }

                }
                _ = interval.tick() => {
                    let (topic, payload) = self.metrics.next();
                    let payload_size = payload.len();
                    match self.client.try_publish(topic, QoS::AtLeastOnce, false, payload) {
                        Ok(_) => {
                            self.metrics.add_total_sent_size(payload_size);
                            continue;
                        }
                        Err(MqttError::TryRequest(request)) => request,
                        Err(e) => return Err(e.into()),
                    }
                }
            };

            match failed.into_inner() {
                Request::Publish(publish) => return Ok(Status::SlowEventloop(publish)),
                request => unreachable!("{:?}", request),
            };
        }
    }

    /// Direct mode is used in case uplink is used with persistence disabled.
    /// It is operated differently from all other modes. Failure is terminal.
    async fn direct(&mut self) -> Result<(), Error> {
        let mut interval = time::interval(time::Duration::from_secs(10));

        loop {
            let payload_size = select! {
                data = self.collector_rx.recv_async() => {
                    let data = data?;

                    // Extract anomalies detected by package during collection
                    if let Some((errors, count)) = data.anomalies() {
                        self.metrics.add_errors(errors, count);
                    }

                    let topic = data.topic();
                    let payload = data.serialize();
                    let payload_size = payload.len();
                    self.client.publish(topic.as_ref(), QoS::AtLeastOnce, false, payload).await?;
                    payload_size
                }
                _ = interval.tick() => {
                    let (topic, payload) = self.metrics.next();
                    let payload_size = payload.len();
                    self.client.publish(topic, QoS::AtLeastOnce, false, payload).await?;
                    payload_size
                }
            };

            self.metrics.add_total_sent_size(payload_size);
        }
    }

    pub async fn start(&mut self) -> Result<(), Error> {
        if self.storage.is_none() {
            return self.direct().await;
        }

        let mut status = Status::EventLoopReady;

        loop {
            let next_status = match status {
                Status::Normal => self.normal().await?,
                Status::SlowEventloop(publish) => self.disk(publish).await?,
                Status::EventLoopReady => self.catchup().await?,
                Status::EventLoopCrash(publish) => self.crash(publish).await?,
            };

            status = next_status;
        }
    }
}

async fn send_publish(
    client: MqttClient,
    topic: String,
    payload: Bytes,
) -> Result<MqttClient, MqttError> {
    client.publish_bytes(topic, QoS::AtLeastOnce, false, payload).await?;
    Ok(client)
}

#[derive(Debug, Default, Serialize)]
struct Metrics {
    #[serde(skip_serializing)]
    topic: String,
    sequence: u32,
    timestamp: u64,
    total_sent_size: usize,
    total_disk_size: usize,
    lost_segments: usize,
    errors: String,
    error_count: usize,
}

impl Metrics {
    pub fn new<T: Into<String>>(topic: T) -> Metrics {
        Metrics { topic: topic.into(), errors: String::with_capacity(1024), ..Default::default() }
    }

    pub fn add_total_sent_size(&mut self, size: usize) {
        self.total_sent_size = self.total_sent_size.saturating_add(size);
    }

    pub fn add_total_disk_size(&mut self, size: usize) {
        self.total_disk_size = self.total_disk_size.saturating_add(size);
    }

    pub fn sub_total_disk_size(&mut self, size: usize) {
        self.total_disk_size = self.total_disk_size.saturating_sub(size);
    }

    pub fn increment_lost_segments(&mut self) {
        self.lost_segments += 1;
    }

    // pub fn add_error<S: Into<String>>(&mut self, error: S) {
    //     self.error_count += 1;
    //     if self.errors.len() > 1024 {
    //         return;
    //     }
    //
    //     self.errors.push_str(", ");
    //     self.errors.push_str(&error.into());
    // }

    pub fn add_errors<S: Into<String>>(&mut self, error: S, count: usize) {
        self.error_count += count;
        if self.errors.len() > 1024 {
            return;
        }

        self.errors.push_str(&error.into());
        self.errors.push_str(" | ");
    }

    pub fn next(&mut self) -> (&str, Vec<u8>) {
        let timestamp =
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::from_secs(0));
        self.timestamp = timestamp.as_millis() as u64;
        self.sequence += 1;

        let payload = serde_json::to_vec(&vec![&self]).unwrap();
        self.errors.clear();
        self.lost_segments = 0;
        (&self.topic, payload)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn normal_to_slow_network() {
        let config = Arc::new(Config {
            broker: "localhost".to_owned(),
            port: 1883,
            device_id: "123".to_owned(),
            ..Default::default()
        });
        let (data_tx, data_rx) = flume::bounded(10);
        let (net_tx, net_rx) = flume::bounded(10);
        let client = MqttClient { net_tx };

        let mut serializer = Serializer::new(config, data_rx, client).unwrap();
        std::thread::spawn(move || {
            tokio::runtime::Runtime::new().unwrap().block_on(serializer.start())
        });
    }
}
