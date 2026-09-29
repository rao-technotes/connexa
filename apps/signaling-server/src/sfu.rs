//! Adapter between the hub and the `connexa-sfu` media server.

use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use connexa_sfu::{Outbox, RTCIceServer, Sfu, SfuConfig, SfuStats};
use connexa_signaling::{Hub, SfuSignal};

use crate::MediaServer;
use crate::config::{IceConfig, SfuSettings};

/// An SFU that is bound but not yet connected to a hub.
pub struct SfuStarter {
    sfu: Arc<Sfu>,
    hub: Arc<OnceLock<Weak<Hub>>>,
}

/// Bind the media port. Must be called inside a Tokio runtime.
pub fn start(settings: SfuSettings, ice: IceConfig) -> Result<SfuStarter> {
    let hub: Arc<OnceLock<Weak<Hub>>> = Arc::new(OnceLock::new());
    let slot = hub.clone();
    let outbox: Outbox = Arc::new(move |room, participant, msg| {
        if let Some(hub) = slot.get().and_then(Weak::upgrade) {
            hub.send_to(room, participant, msg);
        }
    });
    let ice_servers = if ice.stun_urls.is_empty() {
        vec![]
    } else {
        vec![RTCIceServer {
            urls: ice.stun_urls,
            ..Default::default()
        }]
    };
    let sfu = Sfu::new(
        SfuConfig {
            udp_port: settings.udp_port,
            public_ip: settings.public_ip,
            ice_servers,
        },
        outbox,
    )?;
    Ok(SfuStarter { sfu, hub })
}

impl SfuStarter {
    pub fn attach(self, hub: Arc<Hub>) -> Arc<dyn MediaServer> {
        let _ = self.hub.set(Arc::downgrade(&hub));
        let stats = Arc::new(Mutex::new(SfuStats::default()));
        let (sfu, cache) = (self.sfu.clone(), stats.clone());
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                *cache.lock().unwrap() = sfu.stats().await;
            }
        });
        Arc::new(SfuMedia {
            sfu: self.sfu,
            stats,
        })
    }
}

struct SfuMedia {
    sfu: Arc<Sfu>,
    stats: Arc<Mutex<SfuStats>>,
}

#[async_trait]
impl MediaServer for SfuMedia {
    async fn signal(&self, room: &str, participant: &str, signal: SfuSignal) {
        match signal {
            SfuSignal::Offer(sdp) => self.sfu.publisher_offer(room, participant, sdp).await,
            SfuSignal::Answer(sdp) => self.sfu.subscriber_answer(room, participant, sdp).await,
            SfuSignal::Candidate(pc, candidate) => {
                self.sfu.candidate(room, participant, pc, candidate).await
            }
        }
    }

    async fn participant_left(&self, room: &str, participant: &str) {
        self.sfu.leave(room, participant).await;
    }

    async fn room_closed(&self, room: &str) {
        self.sfu.close_room(room).await;
    }

    fn gauges(&self) -> Vec<(&'static str, &'static str, u64)> {
        let s = *self.stats.lock().unwrap();
        vec![
            (
                "connexa_sfu_active_rooms",
                "Rooms with SFU media sessions",
                s.rooms,
            ),
            (
                "connexa_sfu_peers",
                "Participants connected to the SFU",
                s.peers,
            ),
            ("connexa_sfu_tracks", "Tracks being forwarded", s.tracks),
            (
                "connexa_sfu_packets_forwarded",
                "RTP packets forwarded (since start)",
                s.packets_forwarded,
            ),
        ]
    }
}
