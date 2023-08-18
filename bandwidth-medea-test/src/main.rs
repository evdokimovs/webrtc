mod control_api;
mod sfu_member;

use std::sync::atomic::AtomicU32;
use std::time::Instant;
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::control_api::RoomBuilder;
use crate::control_api::MemberBuilder;

use actix_http::ws;
use awc::ws::Frame;
use bytes::Bytes;
use clap::Arg;
use futures::{
    channel::{mpsc, oneshot},
    future, SinkExt as _, StreamExt,
};
use medea_client_api_proto::{
    ClientMsg, Command, Credential, Event, IceCandidate, MemberId, NegotiationRole, PeerId, RoomId,
    ServerMsg,
};
use medea_control_api_proto::endpoint::web_rtc_publish::P2pMode;
use medea_control_api_proto::grpc::api;
use medea_control_api_proto::grpc::api::element::El;
use medea_control_api_proto::grpc::api::web_rtc_publish_endpoint::P2p;
use webrtc::{
    api::{media_engine::MediaEngine, setting_engine::SettingEngine, APIBuilder, API},
    ice::network_type::NetworkType,
    ice_transport::ice_candidate::RTCIceCandidateInit,
    media::Sample,
    peer_connection::{
        configuration::RTCConfiguration, policy::bundle_policy::RTCBundlePolicy,
        sdp::session_description::RTCSessionDescription, OnTrackHdlrFn,
    },
    rtp::extension::HeaderExtension,
    rtp_transceiver::{
        rtp_codec::{RTCRtpCodecCapability, RTCRtpHeaderExtensionCapability, RTPCodecType},
        rtp_transceiver_direction::RTCRtpTransceiverDirection,
    },
    track::track_local::{track_local_static_sample::TrackLocalStaticSample, TrackLocal},
    util::{Marshal, MarshalSize},
};
use crate::control_api::{ControlClient, WebRtcPlayEndpointBuilder, WebRtcPublishEndpoint, WebRtcPublishEndpointBuilder};
use crate::sfu_member::SfuMember;

#[actix_rt::main]
async fn main() {
    // let sfu_member = SfuMember::connect("ws://127.0.0.1:8080/ws/".to_string(), RoomId("pub-sub-sfu-call".to_string()), MemberId("publisher".to_string())).await;
    // SfuMember::send_packet(Arc::clone(&sfu_member), 5000);
    // tokio::time::sleep(std::time::Duration::from_secs(2600)).await;
    // return;

    // let sfu_member = SfuMember::connect("ws://127.0.0.1:8080/ws/".to_string(), RoomId("pub-sub-sfu-call".to_string()), MemberId("subscriber-2".to_string())).await;
    // SfuMember::read_packet(Arc::clone(&sfu_member));
    // tokio::time::sleep(std::time::Duration::from_secs(2600)).await;




    // let sfu_member = SfuMember::connect("ws://127.0.0.1:8080/ws/".to_string(), RoomId("new-room".to_string()), MemberId("test".to_string())).await;
    // actix::spawn(async move {
    //     SfuMember::send_packet(sfu_member, 10_000);
    //     SfuMember::read_packet(sfu_member);
        // loop {
        //     sfu_member.lock().unwrap().send_packet().await;
        // }
    // });
    // tokio::time::sleep(std::time::Duration::from_secs(100)).await;

    // let mut manager = ControlManager::new("http://127.0.0.1:6565".to_string()).await;
    // manager.create_room("hello-world".to_string()).await;
    // manager.create_member("hello-world".to_string(), "test1".to_string()).await;
    // manager.create_member("hello-world".to_string(), "test2".to_string()).await;

    let matches = clap::Command::new("medea-client")
        .subcommand_required(true)
        .subcommand(
            clap::Command::new("create").arg(Arg::new("room_id").required(true)).arg(Arg::new("member_id"))
        )
        .subcommand(
            clap::Command::new("connect")
                .subcommand_required(true)
                .subcommand(
                    clap::Command::new("receiver").arg(clap::Arg::new("addr")),
                )
                .subcommand(
                    clap::Command::new("sender").arg(clap::Arg::new("addr")),
                ),
        )
        .get_matches();

    let mut manager = ControlManager::new("http://127.0.0.1:6565".to_string()).await;
    match matches.subcommand() {
        Some(("create", create_matches)) => {
                let room_id = create_matches.get_one::<String>("room_id").unwrap();
                if let Some(member_id) = create_matches.get_one::<String>("member_id") {
                    println!("Creating new Member {room_id}/{member_id}");
                    manager.create_member(room_id.clone(), member_id.clone()).await;
                } else {
                    println!("Creating new Room {room_id}");
                    manager.create_room(room_id.clone()).await;
                }
        },
        Some(("connect", connect_matches)) => {
            match connect_matches.subcommand() {
                Some(("sender", _)) => {
                    let sfu_member = SfuMember::connect("ws://127.0.0.1:8080/ws/".to_string(), RoomId("pub-sub-sfu-call".to_string()), MemberId("publisher".to_string())).await;
                    SfuMember::send_packet_task(Arc::clone(&sfu_member), 20000).await;
                    // tokio::time::sleep(std::time::Duration::from_secs(2600)).await;
                }
                Some(("receiver", _)) => {
                    let sfu_member = SfuMember::connect("ws://127.0.0.1:8080/ws/".to_string(), RoomId("pub-sub-sfu-call".to_string()), MemberId("subscriber-2".to_string())).await;
                    SfuMember::read_packet_task(Arc::clone(&sfu_member)).await;
                    // tokio::time::sleep(std::time::Duration::from_secs(2600)).await;
                }
                _ => ()
            }
        }
        _ => ()
    }

}

struct ControlManager(ControlClient);

impl ControlManager {
    pub async fn new(dst: String) -> Self {
        ControlManager(ControlClient::connect(dst).await)
    }

    pub async fn create_room(&mut self, room_id: String) {
        self.0
            .create(
                RoomBuilder::default()
                    .id(room_id)
                    .build()
                    .unwrap()
                    .build_request(String::new()),
            )
            .await;
    }

    pub async fn create_member(&mut self, room_id: String, member_id: String) -> String {
        use medea_control_api_proto::grpc::api;
        let spec = self.0.get(&room_id).await;
        let mut all_publishers_fids = Vec::new();
        let mut endpoints_to_create = Vec::new();
        if let Some(El::Room(room)) = spec.el {
            for (id, member) in room.pipeline {
                if let Some(api::room::element::El::Member(member)) = member.el {
                    for (id, endpoint) in member.pipeline {
                        if let Some(api::member::element::El::WebrtcPub(publish)) = endpoint.el {
                            all_publishers_fids.push((
                                format!("play-{}", member.id),
                                format!("local://{}/{}/{}", room.id, member.id, publish.id)
                            ));
                            endpoints_to_create.push((
                                format!("{}/{}", room.id, member.id),
                                format!("play-{}", member_id),
                                format!("local://{}/{}/publish", room_id, member_id),
                            ))
                        }
                    }
                }
            }
        }

        let mut member = MemberBuilder::default();
        member.id(member_id.clone());
        member.credentials(medea_control_api_proto::grpc::api::member::Credentials::Plain("test".to_string()));
        for (play_name, publisher_fid) in all_publishers_fids {
            member.add_endpoint(WebRtcPlayEndpointBuilder::default().id(play_name).src(publisher_fid).build().unwrap());
        }
        member.add_endpoint(WebRtcPublishEndpointBuilder::default().id("publish").p2p_mode(P2pMode::Never).build().unwrap());

        let mut credentials = self.0.create(
            member.build().unwrap().build_request(room_id)
        ).await;

        for (url, id, src) in endpoints_to_create {
            self.0.create(
                WebRtcPlayEndpointBuilder::default().id(id).src(src).build().unwrap().build_request(url)
            ).await;
        }


        credentials.remove(&member_id).unwrap()
    }
}

mod control {
    pub async fn create_member(room_id: String, member_id: String) {

    }

    pub async fn create_room(room_id: String) {

    }
}
