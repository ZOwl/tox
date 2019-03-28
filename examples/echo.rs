// an example of DHT node with current code
//
#[macro_use]
extern crate log;

use std::io::{Error, ErrorKind, Write};
use std::sync::Arc;
use parking_lot::RwLock;

use futures::*;
use futures::sync::mpsc;
use hex::FromHex;
use tokio::net::UdpSocket;
use failure::Fail;

use std::net::SocketAddr;
use std::fs::OpenOptions;
use std::path::Path;

use tox::toxcore::dht::server::*;
use tox::toxcore::dht::server_ext::ServerExt;
use tox::toxcore::dht::packed_node::*;
use tox::toxcore::dht::lan_discovery::*;
use tox::toxcore::crypto_core::*;
use tox::toxcore::friend_connection::*;
use tox::toxcore::net_crypto::*;
use tox::toxcore::net_crypto::errors::SendLosslessPacketError;
use tox::toxcore::onion::client::*;
use tox::toxcore::tcp::client::Connections;
use tox::toxcore::stats::Stats;
use tox::toxcore::messenger::Messenger;
use tox::toxcore::messenger::{Packet as MsgPacket, *};
use tox::toxcore::binary_io::*;

const BOOTSTRAP_NODES: [(&str, &str); 9] = [
    // Impyy
    ("1D5A5F2F5D6233058BF0259B09622FB40B482E4FA0931EB8FD3AB8E7BF7DAF6F", "198.98.51.198:33445"),
    // nurupo
    ("F404ABAA1C99A9D37D61AB54898F56793E1DEF8BD46B1038B9D822E8460FAB67", "67.215.253.85:33445"),
    // Manolis
    ("461FA3776EF0FA655F1A05477DF1B3B614F7D6B124F7DB1DD4FE3C08B03B640F", "130.133.110.14:33445"),
    // Busindre
    ("A179B09749AC826FF01F37A9613F6B57118AE014D4196A0E1105A98F93A54702", "205.185.116.116:33445"),
    // ray65536
    ("8E7D0B859922EF569298B4D261A8CCB5FEA14FB91ED412A7603A585A25698832", "85.172.30.117:33445"),
    // fluke571
    ("3CEE1F054081E7A011234883BC4FC39F661A55B73637A5AC293DDF1251D9432B", "194.249.212.109:33445"),
    // MAH69K
    ("DA4E4ED4B697F2E9B000EEFE3A34B554ACD3F45F5C96EAEA2516DD7FF9AF7B43", "185.25.116.107:33445"),
    // clearmartin
    ("CD133B521159541FB1D326DE9850F5E56A6C724B5B8E5EB5CD8D950408E95707", "46.101.197.175:443"),
    // tastytea
    ("2B2137E094F743AC8BD44652C55F41DFACC502F125E99E4FE24D40537489E32F", "5.189.176.217:5190"),
];

#[derive(Clone)]
struct FileName(Arc<RwLock<String>>);

impl FileName {
    fn new(name: String) -> Self {
        FileName(Arc::new(RwLock::new(name)))
    }
}

const SELF_SK: &str = "B0EC513222557B96FACAF1B69651317CADB61147F7609D9CA13679D994569F48";

const FRIEND_PK: &str = "4184577773A7928B028ACFC72E041322D196A4BB9B1944163AA214ADF5B9712F";
/// Bind a UDP listener to the socket address.
fn bind_socket(addr: SocketAddr) -> UdpSocket {
    let socket = UdpSocket::bind(&addr).expect("Failed to bind UDP socket");
    socket.set_broadcast(true).expect("set_broadcast call failed");
    if addr.is_ipv6() {
        socket.set_multicast_loop_v6(true).expect("set_multicast_loop_v6 call failed");
    }
    socket
}

fn main() {
    env_logger::init();

    if crypto_init().is_err() {
        panic!("Crypto initialization failed.");
    }

    let (dht_pk, dht_sk) = gen_keypair();

    let real_sk_bytes: [u8; 32] = FromHex::from_hex(SELF_SK).unwrap();
    let real_sk = SecretKey::from_slice(&real_sk_bytes).unwrap();
    let real_pk = real_sk.public_key();

    // Create a channel for server to communicate with network
    let (tx, rx) = mpsc::channel(32);

    let local_addr: SocketAddr = "0.0.0.0:33447".parse().unwrap(); // 0.0.0.0 for IPv4
    // let local_addr: SocketAddr = "[::]:33445".parse().unwrap(); // [::] for IPv6

    let socket = bind_socket(local_addr);
    let stats = Stats::new();

    let lan_discovery_sender = LanDiscoverySender::new(tx.clone(), dht_pk, local_addr.is_ipv6());

    let (tcp_incoming_tx, _tcp_incoming_rx) = mpsc::unbounded();

    let mut dht_server = Server::new(tx.clone(), dht_pk, dht_sk.clone());
    dht_server.enable_lan_discovery(true);
    dht_server.enable_ipv6_mode(local_addr.is_ipv6());

    let tcp_connections = Connections::new(dht_pk, dht_sk.clone(), tcp_incoming_tx);
    let onion_client = OnionClient::new(dht_server.clone(), tcp_connections.clone(), real_sk.clone(), real_pk);

    let (lossless_tx, lossless_rx) = mpsc::unbounded();
    let (lossy_tx, lossy_rx) = mpsc::unbounded();

    let net_crypto = NetCrypto::new(NetCryptoNewArgs {
        udp_tx: tx,
        lossless_tx,
        lossy_tx,
        dht_pk,
        dht_sk,
        real_pk,
        real_sk: real_sk.clone(),
        precomputed_keys: dht_server.get_precomputed_keys(),
    });

    dht_server.set_net_crypto(net_crypto.clone());

    let friend_connections = FriendConnections::new(
        real_sk,
        real_pk,
        dht_server.clone(),
        tcp_connections.clone(),
        onion_client.clone(),
        net_crypto.clone(),
    );

    let friend_pk_bytes: [u8; 32] = FromHex::from_hex(FRIEND_PK).unwrap();
    let friend_pk = PublicKey::from_slice(&friend_pk_bytes).unwrap();

    friend_connections.add_friend(friend_pk);

    // Bootstrap from nodes
    for &(pk, saddr) in &BOOTSTRAP_NODES {
        // get PK bytes of the bootstrap node
        let bootstrap_pk_bytes: [u8; 32] = FromHex::from_hex(pk).unwrap();
        // create PK from bytes
        let bootstrap_pk = PublicKey::from_slice(&bootstrap_pk_bytes).unwrap();

        let node = PackedNode::new(saddr.parse().unwrap(), &bootstrap_pk);

        dht_server.add_initial_bootstrap(node);
        onion_client.add_path_node(node);
    }

    let mut messenger = Messenger::new();
    let (file_control_tx, file_control_rx) = mpsc::unbounded();
    let (file_data_tx, file_data_rx) = mpsc::channel(32);
    let net_crypto_ms = net_crypto.clone();
    messenger.set_net_crypto(net_crypto_ms);
    messenger.set_tx_file_data(file_data_tx);
    messenger.set_tx_file_control(file_control_tx);
    let onion_friend = OnionFriend::new(friend_pk);
    messenger.add_friend(friend_pk, onion_friend);

    let ms_c = messenger.clone();

    let file_name = FileName::new("".to_owned());

    let file_name_c = file_name.clone();
    let file_control_future = file_control_rx
        .map_err(|()| -> Error { unreachable!("rx can't fail") })
        .for_each(move |(pk, packet)| {
            if let Packet::FileControl(packet) = packet {
                println!("FileControl = {:?}", packet);
                Box::new(future::ok(())) as Box<Future<Item = (), Error = Error> + Send>
            } else if let Packet::FileSendRequest(packet) = packet {
                println!("FileSendRequest = {:?}", packet);
                if packet.file_type == FileType::Avatar {
                    return Box::new(future::ok(())) as Box<Future<Item = (), Error = Error> + Send>
                }
                *file_name_c.0.write() = packet.file_name;
                Box::new(ms_c.send_file_control(friend_pk, packet.file_id, TransferDirection::Receive, ControlType::Accept)
                    .map_err(|e| Error::new(ErrorKind::Other, e.compat()))) as Box<Future<Item = (), Error = Error> + Send>
            } else {
                Box::new(future::ok(())) as Box<Future<Item = (), Error = Error> + Send>
            }
        });

    let file_name_c = file_name.clone();
    let file_data_future = file_data_rx
        .map_err(|()| -> Error { unreachable!("rx can't fail") })
        .for_each(move |(pk, packet, position)| {
            if let Packet::FileData(packet) = packet {
                let mut file = OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(Path::new(&(*file_name_c.0.read())))
                    .unwrap();
                println!("FileData = {:?}, position = {}", packet, position);
                file.write(&packet.data).unwrap();
                future::ok(())
            } else {
                future::ok(())
            }
        });

    let net_crypto_c = net_crypto.clone();
    let lossless_future = lossless_rx
        .map_err(|()| -> Error { unreachable!("rx can't fail") })
        .for_each(move |(pk, packet)| {
            println!("Lossless!!! {:?}", packet);
            let future = if packet[0] == 0x18 {
                let f1 = net_crypto_c.send_lossless(pk, vec![0x18]);
                let f2 = net_crypto_c.send_lossless(pk, b"\x30tox-rs".to_vec());
                let f3 = net_crypto_c.send_lossless(pk, b"\x40Hi from tox-rs!".to_vec());

                Box::new(f1.join3(f2, f3).map(|_| ()).map_err(|e| Error::new(ErrorKind::Other, e.compat()))) as Box<Future<Item = (), Error = Error> + Send>
            } else if packet[0] == 0x10 { // wtf?
                Box::new(net_crypto_c.send_lossless(pk, vec![0x10]).map_err(|e| Error::new(ErrorKind::Other, e.compat()))) as Box<Future<Item = (), Error = Error> + Send>
            } else if packet[0] == 0x40 {
                Box::new(net_crypto_c.send_lossless(pk, packet).map_err(|e| Error::new(ErrorKind::Other, e.compat()))) as Box<Future<Item = (), Error = Error> + Send>
            } else if packet[0] == 0x50 { // File Send Request
                let (_, packet) = FileSendRequest::from_bytes(&packet).unwrap();
                Box::new(messenger.handle_file_send_request(friend_pk, packet).map_err(|e| Error::new(ErrorKind::Other, e.compat()))) as Box<Future<Item = (), Error = Error> + Send>
            } else if packet[0] == 0x51 { // File Control
                let (_, packet) = FileControl::from_bytes(&packet).unwrap();
                Box::new(messenger.handle_file_control(friend_pk, packet).map_err(|e| Error::new(ErrorKind::Other, e.compat()))) as Box<Future<Item = (), Error = Error> + Send>
            } else if packet[0] == 0x52 { // File Data
                let (_, packet) = FileData::from_bytes(&packet).unwrap();
                Box::new(messenger.handle_file_data(friend_pk, packet).map_err(|e| Error::new(ErrorKind::Other, e.compat()))) as Box<Future<Item = (), Error = Error> + Send>
            }  else {
                Box::new(future::ok(()))
            };
            future
        });

    let lossy_future = lossy_rx
        .map_err(|()| -> Error { unreachable!("rx can't fail") })
        .for_each(|_| {
            println!("Lossy!!!");
            future::ok(())
        });

    let vec = vec![
        Box::new(dht_server.run_socket(socket, rx, stats)) as Box<Future<Item = (), Error = Error> + Send>,
        Box::new(lan_discovery_sender.run().map_err(|e| Error::new(ErrorKind::Other, e.compat()))),
        Box::new(tcp_connections.run()),
        Box::new(onion_client.run()),
        Box::new(net_crypto.run().map_err(|e| Error::new(ErrorKind::Other, e.compat()))),
        Box::new(friend_connections.run()),
        Box::new(lossless_future),
        Box::new(lossy_future),
        Box::new(file_control_future),
        Box::new(file_data_future),
    ];

    let future = future::select_all(vec)
        .map(|_| ())
        .map_err(|(e, _, _)| error!("Processing ended with error: {:?}", e));

    info!("Running DHT server on {}", local_addr);

    tokio::run(future);
}
