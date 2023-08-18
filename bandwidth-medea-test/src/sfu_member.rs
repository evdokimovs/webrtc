use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use awc::{BoxedSocket, ws};
use webrtc::api::{API, APIBuilder};
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::ice::network_type::NetworkType;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpHeaderExtensionCapability, RTPCodecType};
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_remote::TrackRemote;

struct MarshalBytes {
    buf: Bytes,
}

impl MarshalSize for MarshalBytes {
    fn marshal_size(&self) -> usize {
        self.buf.len()
    }
}

impl Marshal for MarshalBytes {
    fn marshal_to(&self, buf: &mut [u8]) -> webrtc::util::Result<usize> {
        for (i, d) in self.buf.iter().enumerate() {
            buf[i] = *d;
        }
        Ok(self.marshal_size())
    }
}

/// Builds [API].
fn build_webrtc_api() -> API {
    let mut media_engine = MediaEngine::default();
    media_engine.register_default_codecs().unwrap();

    for uri in ["urn:ietf:params:rtp-hdrext:sdes:mid"] {
        media_engine
            .register_header_extension(
                RTCRtpHeaderExtensionCapability {
                    uri: uri.to_owned(),
                },
                RTPCodecType::Video,
                None,
            )
            .unwrap();
        media_engine
            .register_header_extension(
                RTCRtpHeaderExtensionCapability {
                    uri: uri.to_owned(),
                },
                RTPCodecType::Audio,
                None,
            )
            .unwrap();
    }

    let mut settings = SettingEngine::default();
    settings.set_network_types(Vec::from([NetworkType::Udp4]));

    APIBuilder::new()
        .with_media_engine(media_engine)
        .with_setting_engine(settings)
        .build()
}

use tokio::sync::oneshot;
use tokio::sync::mpsc;
use actix_codec::Framed;
use awc::ws::Codec;
use awc::ws::Frame;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use medea_client_api_proto::{ClientMsg, Command, Credential, Event, IceCandidate, MemberId, NegotiationRole, PeerId, RoomId, ServerMsg};
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::policy::bundle_policy::RTCBundlePolicy;
use awc::ws::Message;
use bytes::Bytes;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::media::Sample;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp::extension::HeaderExtension;
use webrtc::rtp_transceiver::rtp_receiver::RTCRtpReceiver;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::track::track_local::TrackLocal;
use webrtc::util::{Marshal, MarshalSize};

type WebSocketSender = SplitSink<Framed<BoxedSocket, Codec>, Message>;
type WebSocketReceiver = SplitStream<Framed<BoxedSocket, Codec>>;

#[derive(Clone, Debug)]
struct DescructorTester;

impl Drop for DescructorTester {
    fn drop(&mut self) {
        println!("It's dropped");
    }
}

pub struct SfuMember {
    api: API,
    sender_tracks: Vec<Arc<TrackLocalStaticSample>>,
    receiver_tracks: Vec<Arc<TrackRemote>>,
    negotiation_completed_tx: Option<oneshot::Sender<()>>,
    ws_sink: mpsc::UnboundedSender<Message>,
    room_id: RoomId,
    pcs: HashMap<PeerId, RTCPeerConnection>,
    receivers: Vec<Arc<RTCRtpReceiver>>,
}

impl SfuMember {
    pub async fn connect(url: String, room_id: RoomId, member_id: MemberId) -> Arc<Mutex<Self>> {
        let client: awc::Client = awc::Client::new();
        let api = build_webrtc_api();
        let (negotiation_completed_tx, negotiation_completed_rx) = oneshot::channel();
        // baseUrl + roomId + '/' + username + '?token=test'
        let url = format!("ws://127.0.0.1:8080/ws");
        let (_, framed) = client.ws(url.as_str()).connect().await.unwrap();
        let (mut ws_sender, ws_receiver) = framed.split();
        let (ws_tx, mut ws_rx) = mpsc::unbounded_channel::<Message>();

        actix::spawn(async move {
            while let Some(msg) = ws_rx.recv().await {
                ws_sender.send(msg).await;
            }
        });
        let msg = ClientMsg::Command {
            room_id: room_id.clone(),
            command: Command::JoinRoom {
                member_id,
                credential: Credential("test".to_string()),
            },
        };

        let msg =
            ws::Message::Text(serde_json::to_string(&msg).unwrap().into());
        ws_tx.send(msg).unwrap();

        let this = Arc::new(Mutex::new(Self {
            api,
            sender_tracks: Vec::new(),
            room_id,
            receiver_tracks: Vec::new(),
            negotiation_completed_tx: Some(negotiation_completed_tx),
            ws_sink: ws_tx,
            pcs: HashMap::new(),
            receivers: Vec::new(),
        }));
        actix::spawn(Self::ws_handler(Arc::clone(&this), ws_receiver));

        negotiation_completed_rx.await;

        this
    }

    async fn handle_peer_created(this: Arc<Mutex<Self>>, peer_id: PeerId, negotiation_role: NegotiationRole) {
        println!("PeerCreated event: {}", peer_id);
        let mut lock = this.lock().unwrap();
        let pc = lock.api.new_peer_connection(RTCConfiguration {
            bundle_policy: RTCBundlePolicy::MaxBundle,
            ..RTCConfiguration::default()
        }).await.unwrap();

        pc.on_track(Box::new({
            println!("on track subscriber");
            let this = Arc::clone(&this);
            let dtest = DescructorTester;
            move |track, _, _| {
                dtest.clone();
                println!("Receiver track received");
                Box::pin({
                    let this = Arc::clone(&this);
                    async move {
                        println!("Receiver track received");
                        let mut lock = this.lock().unwrap();
                        lock.receiver_tracks.push(track);
                    }
                })
            }
        }));

        pc.on_ice_candidate({
            let this = Arc::clone(&this);
            let room_id = lock.room_id.clone();
            Box::new(move |ice| {
                println!("On ICE Candidate");
                let this = Arc::clone(&this);
                let room_id = room_id.clone();
                Box::pin(async move {
                    if let Some(candidate) = ice {
                        match candidate.to_json() {
                            Ok(cdt) => {
                                let candidate = IceCandidate {
                                    candidate: cdt.candidate,
                                    sdp_m_line_index: cdt.sdp_mline_index,
                                    sdp_mid: cdt.sdp_mid,
                                };
                                let msg = ClientMsg::Command {
                                    room_id,
                                    command: Command::SetIceCandidate {
                                        peer_id,
                                        candidate,
                                    },
                                };
                                let msg = ws::Message::Text(
                                    serde_json::to_string(&msg).unwrap().into(),
                                );
                                this.lock().unwrap().ws_sink.send(msg);
                            }
                            _ => (),
                        }
                    }
                })
            })
        });

        match negotiation_role {
            NegotiationRole::Offerer => {
                unreachable!("SFU peers must always act as Answerer");
            }
            NegotiationRole::Answerer(sdp_offer) => {
                let answ = {
                    pc.set_remote_description(
                        RTCSessionDescription::offer(sdp_offer.clone())
                            .unwrap()
                    ).await.unwrap();

                    let ts = pc.get_transceivers().await;
                    for t in &ts {
                        println!("Direction: {:?}", t.direction());
                    }

                    let audio_receive: Vec<_> = ts.iter().filter(|t| t.direction() == RTCRtpTransceiverDirection::Recvonly).cloned().collect();

                    let audio_ts = ts
                        .iter()
                        .filter(|a| a.kind() == RTPCodecType::Audio)
                        .next()
                        .unwrap();

                    if audio_ts.direction()
                        == RTCRtpTransceiverDirection::Sendonly
                    {
                        println!("Found SendOnly transceiver");
                        let track_v = Arc::new({
                            TrackLocalStaticSample::new(
                                RTCRtpCodecCapability {
                                    mime_type: "audio/G722".to_owned(),
                                    ..Default::default()
                                },
                                format!("audio{}", peer_id),
                                "webrtc-rs".to_owned(),
                            )
                        });

                        let tr = ts
                            .iter()
                            .filter(|t| t.direction() == RTCRtpTransceiverDirection::Sendonly)
                            .find(|a| a.kind() == RTPCodecType::Audio)
                            .map(Clone::clone)
                            .unwrap();
                        let _ = tr
                            .sender()
                            .await
                            .replace_track(Some(Arc::clone(&track_v)
                                as Arc<dyn TrackLocal + Send + Sync>))
                            .await;
                        lock.sender_tracks.push(track_v);
                    }
                        let answer = pc.create_answer(None).await.unwrap();
                        pc.set_local_description(
                            RTCSessionDescription::answer(answer.sdp.clone())
                                .unwrap(),
                        )
                            .await
                            .unwrap();
                    let msg = ClientMsg::Command {
                        room_id: lock.room_id.clone(),
                        command: Command::MakeSdpAnswer {
                            peer_id,
                            sdp_answer: answer.sdp.clone(),
                            transceivers_statuses: HashMap::new(),
                        }
                    };

                    lock.ws_sink.send(ws::Message::Text(
                        serde_json::to_string(&msg).unwrap().into(),
                    ));
                    for ts in audio_receive {
                        lock.receivers.push(ts.receiver().await);
                        // if let Some(track) = ts.receiver().await.track().await {
                        //     lock.receiver_tracks.push(track);
                        // } else {
                        //     println!("fuck no tracks");
                        // }
                    }
                        answer
                };


                lock.pcs.insert(peer_id, pc);

                // if lock.pcs.len() > 1 {
                    if let Some(tx) = lock.negotiation_completed_tx.take() {
                        tx.send(());
                    }
                // }
            }
        }
    }

    async fn ws_handler(this: Arc<Mutex<Self>>, mut framed: WebSocketReceiver) {
        while let Some(msg) = framed.next().await {
            if let Frame::Text(txt) = msg.unwrap() {
                let txt = String::from_utf8(txt.to_vec()).unwrap();
                let server_msg: ServerMsg = serde_json::from_str(&txt).unwrap();

                match server_msg {
                    ServerMsg::Ping(id) => {
                        let mut lock = this.lock().unwrap();
                        let json = serde_json::to_string(&ClientMsg::Pong(id)).unwrap();
                        lock.ws_sink
                            .send(ws::Message::Text(json.into()))
                            .unwrap();
                    }
                    ServerMsg::Event { event, .. } => {
                        match event {
                            Event::PeerCreated { peer_id, negotiation_role, .. } => {
                                Self::handle_peer_created(Arc::clone(&this), peer_id, negotiation_role).await;
                            }
                            Event::PeerUpdated { peer_id, .. } => {

                            }
                            Event::IceCandidateDiscovered { peer_id, candidate, .. } => {
                                let mut lock = this.lock().unwrap();
                                let pc = lock.pcs.get_mut(&peer_id).unwrap();
                                pc.add_ice_candidate(RTCIceCandidateInit {
                                    candidate: candidate.candidate.clone(),
                                    sdp_mid: candidate.sdp_mid.clone(),
                                    sdp_mline_index: candidate.sdp_m_line_index,
                                    username_fragment: None,
                                })
                                    .await
                                    .unwrap();
                            }
                            _ => ()
                        }
                    }
                    _ => (),
                }
            }
        }
    }

    pub async fn send_packet_task(this: Arc<Mutex<Self>>, limit_per_second: u32) {
        let track = {
            let lock = this.lock().unwrap();
            lock.sender_tracks.first().unwrap().clone()
        };
        let data = Bytes::from_iter((0..1000).map(|i| (i % 255) as u8).into_iter());
        println!("len: {}", data.len());
        let e = [
            HeaderExtension::Custom {
                uri: Cow::from("urn:ietf:params:rtp-hdrext:sdes:mid"),
                extension: Box::new(
                    MarshalBytes {
                        buf: Bytes::copy_from_slice("0".as_bytes())
                    }
                )
            }
        ];

        let mut pkt_cnt = 0;
        let mut last_period_end_at = Instant::now();
        loop {
            if last_period_end_at.elapsed() > Duration::from_secs(1) {
                println!("Packet sending speed (Pkt/sec): {}", pkt_cnt);
                pkt_cnt = 0;
                last_period_end_at = Instant::now();
                continue;
            }
            if pkt_cnt > limit_per_second {
                tokio::time::sleep(std::time::Duration::from_micros(500)).await;
                continue;
            }
            pkt_cnt += 1;
            track.write_sample_with_extensions(&Sample {
                data: data.clone(),
                duration: Duration::from_secs(1),
                ..Default::default()
            }, &e).await.unwrap();
        }
    }

    pub fn send_packet(this: Arc<Mutex<Self>>, limit_per_second: u32) {
        let track = {
            let lock = this.lock().unwrap();
            lock.sender_tracks.first().unwrap().clone()
        };
        let data = Bytes::from_iter((0..1000).map(|i| (i % 255) as u8).into_iter());
        println!("len: {}", data.len());
        let e = [
            HeaderExtension::Custom {
                uri: Cow::from("urn:ietf:params:rtp-hdrext:sdes:mid"),
                extension: Box::new(
                    MarshalBytes {
                        buf: Bytes::copy_from_slice("0".as_bytes())
                    }
                )
            }
        ];
        actix::spawn(async move {
            let mut pkt_cnt = 0;
            let mut last_period_end_at = Instant::now();
            loop {
                if last_period_end_at.elapsed() > Duration::from_secs(1) {
                    println!("Packet sending speed (Pkt/sec): {}", pkt_cnt);
                    pkt_cnt = 0;
                    last_period_end_at = Instant::now();
                    continue;
                }
                if pkt_cnt > limit_per_second {
                    tokio::time::sleep(std::time::Duration::from_micros(500)).await;
                    continue;
                }
                pkt_cnt += 1;
                track.write_sample_with_extensions(&Sample {
                    data: data.clone(),
                    duration: Duration::from_secs(1),
                    ..Default::default()
                }, &e).await.unwrap();
            }
        });
        // let data = Bytes::from_iter((0..1000).map(|i| (i % 255) as u8).into_iter());
        // let e = [
        //     HeaderExtension::Custom {
        //         uri: Cow::from("urn:ietf:params:rtp-hdrext:sdes:mid"),
        //         extension: Box::new(
        //             MarshalBytes {
        //                 buf: Bytes::copy_from_slice("0".as_bytes())
        //             }
        //         )
        //     }
        // ];
        // self.sender_tracks.first().unwrap().write_sample_with_extensions(&Sample {
        //     data: data.clone(),
        //     duration: Duration::from_secs(1),
        //     ..Default::default()
        // }, &e).await.unwrap();
    }

    pub async fn read_packet_task(this: Arc<Mutex<Self>>) {
        println!("Read packet sending speed (Pkt/sec)");
        let mut pkt_cnt = 0;
        let mut last_period_end_at = Instant::now();
        let mut remote_track = None;
        loop {
            // tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            if remote_track.is_none() {
                tokio::task::yield_now().await;
                let remote_tracks = this.lock().unwrap().receiver_tracks.clone();
                if !remote_tracks.is_empty() {
                    remote_track = Some(remote_tracks.first().unwrap().clone());
                }
            }
            if let Some(track) = &mut remote_track {
                // for remote_track in &remote_tracks {
                if last_period_end_at.elapsed() > Duration::from_secs(1) {
                    println!("Packet receiving speed (Pkt/sec): {}", pkt_cnt);
                    pkt_cnt = 0;
                    last_period_end_at = Instant::now();
                    continue;
                }
                // println!("Read packet sending speed (Pkt/sec)");
                track.read_rtp().await.unwrap();
                pkt_cnt += 1;
                // }
            }
        }
    }

    pub fn read_packet(this: Arc<Mutex<Self>>) {
        actix::spawn(async move {
            Self::read_packet_task(this).await;
        });
    }
}