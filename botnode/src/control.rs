use crate::prelude::*;

/// Control engine for Botnode
///
/// The control engine maintains the connection to Botvana server.
pub struct ControlEngine {
    bot_id: BotId,
    server_addr: String,
    status: BotnodeStatus,
    ping_interval: std::time::Duration,
}

impl ControlEngine {
    pub fn new<T: ToString>(bot_id: BotId, server_addr: T) -> Self {
        Self {
            bot_id,
            server_addr: server_addr.to_string(),
            status: BotnodeStatus::Offline,
            ping_interval: std::time::Duration::from_secs(5),
        }
    }
}

#[async_trait(?Send)]
impl Engine for ControlEngine {
    type Data = ();

    async fn start(mut self, shutdown: Shutdown) -> Result<(), EngineError> {
        info!("Starting control engine");

        while let Err(e) = control_loop(&mut self, shutdown.clone()).await {
            error!("Control engine error: {:?}", e);
            async_std::task::sleep(std::time::Duration::from_secs(1)).await;
        }

        Ok(())
    }

    /// Returns dummy data receiver
    fn data_rx(&self) -> ring_channel::RingReceiver<Self::Data> {
        let (_data_tx, data_rx) =
            ring_channel::ring_channel::<()>(NonZeroUsize::new(1024).unwrap());
        data_rx
    }
}

impl ToString for ControlEngine {
    fn to_string(&self) -> String {
        "control-engine".to_string()
    }
}

#[derive(Clone, PartialEq)]
enum BotnodeStatus {
    Connecting,
    Online,
    Offline,
}

/// Runs the Botnode control engine that runs the connection to Botvana
///
/// This connects to Botvana server on a given address, sends the Hello
/// message and runs the loop.
async fn control_loop(control: &mut ControlEngine, shutdown: Shutdown) -> Result<(), EngineError> {
    let _token = shutdown
        .delay_shutdown_token()
        .map_err(|_| EngineError {})?;

    control.status = BotnodeStatus::Connecting;

    let stream = TcpStream::connect(control.server_addr.clone())
        .await
        .map_err(|_| EngineError {})?;

    let mut framed = Framed::new(stream, BotvanaCodec);

    let msg = Message::hello(control.bot_id.clone());
    if let Err(e) = framed.send(msg).await {
        error!("Error framing the message: {:?}", e);
    }

    loop {
        futures::select! {
            msg = framed.next().fuse() => {
                match msg {
                    Some(Ok(msg)) => {
                        if matches!(
                            control.status,
                            BotnodeStatus::Offline | BotnodeStatus::Connecting
                            ) {
                            control.status = BotnodeStatus::Online;
                        }

                        debug!("received from server = {:?}", msg);
                    }
                    Some(Err(e)) => {
                        error!("Botvana connection error: {:?}", e);
                        return Err(EngineError {});
                    }
                    None => {
                        error!("disconnected from botvana");
                        return Err(EngineError {});
                    }
                }
            }
            _ = async_std::task::sleep(control.ping_interval).fuse() => {
                framed.send(Message::ping()).await.unwrap();
            }
            _ = shutdown.wait_shutdown_triggered().fuse() => {
                break Ok(());
            }
        }
    }
}
