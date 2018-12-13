//! Crypto connection implementation.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use super::packets_array::*;

use toxcore::crypto_core::*;
use toxcore::dht::packet::*;
use toxcore::time::*;

/// Interval in ms between sending cookie request/handshake packets.
pub const CRYPTO_SEND_PACKET_INTERVAL: u64 = 1000;

/// The maximum number of times we try to send the cookie request and handshake
/// before giving up
pub const MAX_NUM_SENDPACKET_TRIES: u8 = 8;

/// If we don't receive UDP packets for this amount of time in seconds the
/// direct UDP connection is considered dead
pub const UDP_DIRECT_TIMEOUT: u64 = 8;

/// Default rtt in milliseconds
pub const DEFAULT_RTT: u64 = 1000;

/// rtt in milliseconds for TCP connections
pub const TCP_RTT: u64 = 500;

/// The dT for the average packet receiving rate calculations.
pub const PACKET_COUNTER_AVERAGE_INTERVAL: u64 = 50;

/// How many last sizes of `send_array` should be recorded for congestion
/// control.
pub const CONGESTION_QUEUE_ARRAY_SIZE: usize = 12;

/// How many numbers of sent lossless packets should be recorded for congestion
/// control. It should be bigger than `CONGESTION_QUEUE_ARRAY_SIZE` due to rtt.
pub const CONGESTION_LAST_SENT_ARRAY_SIZE: usize = CONGESTION_QUEUE_ARRAY_SIZE * 2;

/// Minimum packets rate per second.
pub const CRYPTO_PACKET_MIN_RATE: f64 = 4.0;

/// Timeout for increasing speed after congestion event (in ms).
pub const CONGESTION_EVENT_TIMEOUT: u64 = 1000;

/// If the send queue grows so that it will take more than 2 seconds to send all
/// its packet we will reduce send rate.
pub const SEND_QUEUE_CLEARANCE_TIME: f64 = 2.0;

/// Minimum packets in the send queue to reduce send rate.
pub const CRYPTO_MIN_QUEUE_LENGTH: u32 = 64;

/// Ratio of recv queue size / recv packet rate (in seconds) times
/// the number of ms between request packets to send at that ratio.
pub const REQUEST_PACKETS_COMPARE_CONSTANT: f64 = 0.125 * 100.0;

/// Packet that should be sent every second. Depending on `ConnectionStatus` it
/// can be `CookieRequest` or `CryptoHandshake`
#[derive(Clone, Debug, Eq, PartialEq)]
enum StatusPacketEnum {
    /// `CookieRequest` packet
    CookieRequest(CookieRequest),
    /// `CryptoHandshake` packet
    CryptoHandshake(CryptoHandshake),
}

/// Packet that should be sent to the peer every second together with info how
/// many times it was sent and when it was sent last time
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusPacket {
    /// Packet that should be sent every second. Depending on `ConnectionStatus`
    /// it can be `CookieRequest` or `CryptoHandshake`
    packet: StatusPacketEnum,
    /// When packet was sent last time
    pub sent_time: Instant,
    /// How many times packet was sent
    pub num_sent: u8
}

impl StatusPacket {
    /// Create new `StatusPacket` with `CookieRequest` packet
    pub fn new_cookie_request(packet: CookieRequest) -> StatusPacket {
        StatusPacket {
            packet: StatusPacketEnum::CookieRequest(packet),
            sent_time: clock_now(),
            num_sent: 0
        }
    }

    /// Create new `StatusPacket` with `CryptoHandshake` packet
    pub fn new_crypto_handshake(packet: CryptoHandshake) -> StatusPacket {
        StatusPacket {
            packet: StatusPacketEnum::CryptoHandshake(packet),
            sent_time: clock_now(),
            num_sent: 0
        }
    }

    /// Get `Packet` that should be sent every second
    pub fn dht_packet(&self) -> Packet {
        match self.packet {
            StatusPacketEnum::CookieRequest(ref packet) => Packet::CookieRequest(packet.clone()),
            StatusPacketEnum::CryptoHandshake(ref packet) => Packet::CryptoHandshake(packet.clone()),
        }
    }

    /// Check if one second is elapsed since last time when the packet was sent
    fn is_time_elapsed(&self) -> bool {
        clock_elapsed(self.sent_time) > Duration::from_millis(CRYPTO_SEND_PACKET_INTERVAL)
    }

    /// Check if packet should be sent to the peer
    pub fn should_be_sent(&self) -> bool {
        self.num_sent == 0 || self.is_time_elapsed() && self.num_sent < MAX_NUM_SENDPACKET_TRIES
    }

    /// Check if we didn't receive response to this packet on time
    pub fn is_timed_out(&self) -> bool {
        self.num_sent >= MAX_NUM_SENDPACKET_TRIES && self.is_time_elapsed()
    }
}

/** Status of crypto connection.

Initial state is `CookieRequesting`. In this status we send up to 8
`CookieRequest` packets every second and waiting for a `CookieResponse` packet.
After receiving a `CookieResponse` packet we change the status to
`HandshakeSending` in which we send up to 8 `CryptoHandshake` packets every
second and waiting for `CryptoHandshake` packet from the other side. When we
received `CryptoHandshake` packet we change the status to `NotConfirmed` and
continue sending `CryptoHandshake` packets because we don't know if the other
side received our `CryptoHandshake` packet. Only after receiving first
`CryptoData` packet we change status to `Established` and stop sending
`CryptoHandshake` packets. In this status connection is considered as fully
established.

It's also possible that we received `CryptoHandshake` packet but didn't create
crypto connection yet. This means that we should skip first two states and use
`NotConfirmed` state as initial. We can do it because `CryptoHandshake` contains
`Cookie` that we can use to send our `CryptoHandshake`.

*/
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConnectionStatus {
    /// We are sending cookie request packets and haven't received cookie
    /// response yet.
    CookieRequesting {
        /// ID used in the cookie request packets for this connection
        cookie_request_id: u64,
        /// Packet that should be sent every second
        packet: StatusPacket,
    },
    /// We are sending handshake packets and haven't received handshake from the
    /// other side yet.
    HandshakeSending {
        /// Nonce that should be used to encrypt outgoing packets
        sent_nonce: Nonce,
        /// Packet that should be sent every second
        packet: StatusPacket,
    },
    /// A handshake packet has been received from the other side but no
    /// encrypted packets. Continue sending handshake packets because we can't
    /// know if the other side has received them.
    NotConfirmed {
        /// Nonce that should be used to encrypt outgoing packets
        sent_nonce: Nonce,
        /// Nonce that should be used to decrypt incoming packets
        received_nonce: Nonce,
        /// `PublicKey` of the other side for this session
        peer_session_pk: PublicKey,
        /// `PrecomputedKey` for this session that is used to encrypt and
        /// decrypt data packets
        session_precomputed_key: PrecomputedKey,
        /// Packet that should be sent every second
        packet: StatusPacket,
    },
    /// A valid encrypted packet has been received from the other side.
    /// Connection is fully established.
    Established {
        /// Nonce that should be used to encrypt outgoing packets
        sent_nonce: Nonce,
        /// Nonce that should be used to decrypt incoming packets
        received_nonce: Nonce,
        /// `PublicKey` of the other side for this session
        peer_session_pk: PublicKey,
        /// `PrecomputedKey` for this session that is used to encrypt and
        /// decrypt data packets
        session_precomputed_key: PrecomputedKey,
    },
}

/// Sent but not confirmed data packet that is stored in `PacketsArray`
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SentPacket {
    /// Packet data
    pub data: Vec<u8>,
    /// Time when we sent this packet last time
    pub sent_time: Instant,
    /// True if a request was received for this packet and rtt was elapsed at
    /// that moment
    pub requested: bool
}

impl SentPacket {
    /// Create new `SentPacket`
    pub fn new(data: Vec<u8>) -> SentPacket {
        SentPacket {
            data,
            sent_time: clock_now(),
            requested: false
        }
    }
}

/// Received but not handled data packet that is stored in `PacketsArray`
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecvPacket {
    /// Packet data
    pub data: Vec<u8>
}

impl RecvPacket {
    /// Create new `RecvPacket`
    pub fn new(data: Vec<u8>) -> RecvPacket {
        RecvPacket {
            data
        }
    }
}

/** Secure connection to send data between two friends that provides encryption,
ordered delivery, and perfect forward secrecy.

It can use both UDP and TCP (over relays) transport protocols to send data and
can switch between them without the peers needing to disconnect and reconnect.

*/
#[derive(Clone, Debug, PartialEq)]
pub struct CryptoConnection {
    /// Precomputed key of our DHT `SecretKey` and peer's DHT `PublicKey`
    pub dht_precomputed_key: PrecomputedKey,
    /// Long term `PublicKey` of the peer we are connected to
    pub peer_real_pk: PublicKey,
    /// DHT `PublicKey` of the peer we are connected to
    pub peer_dht_pk: PublicKey,
    /// `SecretKey` for this session
    pub session_sk: SecretKey,
    /// `PublicKey` for this session
    pub session_pk: PublicKey,
    /// Current connection status
    pub status: ConnectionStatus,
    /// Address to send UDP packets directly to the peer
    pub udp_addr: Option<SocketAddr>, // TODO: separate v4 and v6?
    /// Time when last UDP packet was received
    pub udp_received_time: Option<Instant>,
    /// Time when we made an attempt to send UDP packet
    pub udp_send_attempt_time: Option<Instant>,
    /// Buffer of sent packets
    pub send_array: PacketsArray<SentPacket>,
    /// Buffer of received packets
    pub recv_array: PacketsArray<RecvPacket>,
    /// Round trip time - the lowest (for all packets) difference between time
    /// when a packet was sent and time when we received the confirmation
    pub rtt: Duration,
    /// Time when the last request packet was sent.
    pub request_packet_sent_time: Option<Instant>,

    // Stats for congestion control

    /// Time since the last stats calculation.
    pub stats_calculation_time: Instant,
    /// Number of received lossless packets since the last rate calculation.
    pub packets_received: u32,
    /// Number of sent lossless packets. It does not include resent packets.
    pub packets_sent: u32,
    /// Number of resent lossless packets.
    pub packets_resent: u32,
    /// Current position in `last_send_array_sizes` and `last_num_packets` arrays.
    pub last_sendqueue_counter: u32,
    /// Last sizes of `send_array`.
    pub last_send_array_sizes: [u32; CONGESTION_QUEUE_ARRAY_SIZE],
    /// Last sent packets counts.
    pub last_num_packets_sent: [u32; CONGESTION_LAST_SENT_ARRAY_SIZE],
    /// Last resent packets counts.
    pub last_num_packets_resent: [u32; CONGESTION_LAST_SENT_ARRAY_SIZE],
    /// Congestion event is a time when we couldn't send all packets due to
    /// slow connection.
    pub last_congestion_event: Option<Instant>,
    /// Rate of receiving lossless packets.
    pub packet_recv_rate: f64,
    /// Estimated packets send rate.
    pub packet_send_rate: f64,
    /// Estimated requested packets send rate.
    pub packet_send_rate_requested: f64,
}

impl CryptoConnection {
    /// Create new `CryptoConnection` with `CookieRequesting` status. This
    /// function is used when we initiate crypto connection with a friend.
    pub fn new(dht_sk: &SecretKey, dht_pk: PublicKey, real_pk: PublicKey, peer_real_pk: PublicKey, peer_dht_pk: PublicKey) -> CryptoConnection {
        let dht_precomputed_key = precompute(&peer_dht_pk, dht_sk);
        let (session_pk, session_sk) = gen_keypair();

        let cookie_request_id = random_u64();
        let cookie_request_payload = CookieRequestPayload {
            pk: real_pk,
            id: cookie_request_id
        };
        let cookie_request = CookieRequest::new(&dht_precomputed_key, &dht_pk, &cookie_request_payload);
        let status = ConnectionStatus::CookieRequesting {
            cookie_request_id,
            packet: StatusPacket::new_cookie_request(cookie_request)
        };

        CryptoConnection {
            dht_precomputed_key,
            peer_real_pk,
            peer_dht_pk,
            session_sk,
            session_pk,
            status,
            udp_addr: None,
            udp_received_time: None,
            udp_send_attempt_time: None,
            send_array: PacketsArray::new(),
            recv_array: PacketsArray::new(),
            rtt: Duration::from_millis(DEFAULT_RTT),
            request_packet_sent_time: None,
            stats_calculation_time: clock_now(),
            packets_received: 0,
            packets_sent: 0,
            packets_resent: 0,
            last_sendqueue_counter: 0,
            last_send_array_sizes: [0; CONGESTION_QUEUE_ARRAY_SIZE],
            last_num_packets_sent: [0; CONGESTION_LAST_SENT_ARRAY_SIZE],
            last_num_packets_resent: [0; CONGESTION_LAST_SENT_ARRAY_SIZE],
            last_congestion_event: None,
            packet_recv_rate: 0.0,
            packet_send_rate: CRYPTO_PACKET_MIN_RATE,
            packet_send_rate_requested: CRYPTO_PACKET_MIN_RATE,
        }
    }

    /// Create new `CryptoConnection` with `NotConfirmed` status. This function
    /// is used when we got `CryptoHandshake` packet from a friend but didn't
    /// create `CryptoConnection` yet.
    pub fn new_not_confirmed(
        dht_sk: &SecretKey,
        peer_real_pk: PublicKey,
        peer_dht_pk: PublicKey,
        received_nonce: Nonce,
        peer_session_pk: PublicKey,
        cookie: EncryptedCookie,
        symmetric_key: &secretbox::Key
    ) -> CryptoConnection {
        let dht_precomputed_key = precompute(&peer_dht_pk, dht_sk);
        let (session_pk, session_sk) = gen_keypair();
        let sent_nonce = gen_nonce();

        let our_cookie = Cookie::new(peer_real_pk, peer_dht_pk);
        let our_encrypted_cookie = EncryptedCookie::new(symmetric_key, &our_cookie);
        let handshake_payload = CryptoHandshakePayload {
            base_nonce: sent_nonce,
            session_pk,
            cookie_hash: cookie.hash(),
            cookie: our_encrypted_cookie,
        };
        let handshake = CryptoHandshake::new(&dht_precomputed_key, &handshake_payload, cookie);
        let status = ConnectionStatus::NotConfirmed {
            sent_nonce,
            received_nonce,
            peer_session_pk,
            session_precomputed_key: precompute(&peer_session_pk, &session_sk),
            packet: StatusPacket::new_crypto_handshake(handshake)
        };

        CryptoConnection {
            dht_precomputed_key,
            peer_real_pk,
            peer_dht_pk,
            session_sk,
            session_pk,
            status,
            udp_addr: None,
            udp_received_time: None,
            udp_send_attempt_time: None,
            send_array: PacketsArray::new(),
            recv_array: PacketsArray::new(),
            rtt: Duration::from_millis(DEFAULT_RTT),
            request_packet_sent_time: None,
            stats_calculation_time: clock_now(),
            packets_received: 0,
            packets_sent: 0,
            packets_resent: 0,
            last_sendqueue_counter: 0,
            last_send_array_sizes: [0; CONGESTION_QUEUE_ARRAY_SIZE],
            last_num_packets_sent: [0; CONGESTION_LAST_SENT_ARRAY_SIZE],
            last_num_packets_resent: [0; CONGESTION_LAST_SENT_ARRAY_SIZE],
            last_congestion_event: None,
            packet_recv_rate: 0.0,
            packet_send_rate: CRYPTO_PACKET_MIN_RATE,
            packet_send_rate_requested: CRYPTO_PACKET_MIN_RATE,
        }
    }

    /// Get `CookieRequest` or `CryptoHandshake` if it should be sent depending
    /// on connection status and update sent counter
    pub fn packet_to_send(&mut self) -> Option<Packet> {
        match self.status {
            ConnectionStatus::CookieRequesting { ref mut packet, .. }
            | ConnectionStatus::HandshakeSending { ref mut packet, .. }
            | ConnectionStatus::NotConfirmed { ref mut packet, .. } => {
                if packet.should_be_sent() {
                    packet.num_sent += 1;
                    packet.sent_time = clock_now();
                    Some(packet.dht_packet())
                } else {
                    None
                }
            },
            ConnectionStatus::Established { .. } => None,
        }
    }

    /// Check if this connection is timed out, i.e. we didn't receive expected
    /// packet in time
    pub fn is_timed_out(&self) -> bool {
        match self.status {
            ConnectionStatus::CookieRequesting { ref packet, .. }
            | ConnectionStatus::HandshakeSending { ref packet, .. }
            | ConnectionStatus::NotConfirmed { ref packet, .. } => packet.is_timed_out(),
            ConnectionStatus::Established { .. } => false, // TODO: timeout?
        }
    }

    /// Set time when last UDP packet was received to now
    pub fn update_udp_received_time(&mut self) {
        self.udp_received_time = Some(clock_now())
    }

    /// Set time when we made an attempt to send UDP packet
    pub fn update_udp_send_attempt_time(&mut self) {
        self.udp_send_attempt_time = Some(clock_now())
    }

    /// Check if we received the last UDP packet not later than 8 seconds ago
    pub fn is_udp_alive(&self) -> bool {
        self.udp_received_time
            .map(|time| clock_elapsed(time) < Duration::from_secs(UDP_DIRECT_TIMEOUT))
            .unwrap_or(false)
    }

    /// Check if we should send UDP packet regardless of whether UDP is dead or
    /// alive. In this case we shouldn't rely on UDP only and send the same
    /// packet via TCP relay
    pub fn udp_attempt_should_be_made(&self) -> bool {
        self.udp_send_attempt_time
            .map(|time| clock_elapsed(time) >= Duration::from_secs(UDP_DIRECT_TIMEOUT / 2))
            .unwrap_or(true)
    }

    /// Calculate packets receive rate.
    fn calculate_recv_rate(&mut self, now: Instant) {
        let dt = now - self.stats_calculation_time;
        self.packet_recv_rate = self.packets_received as f64 / (dt.as_secs() as f64 + dt.subsec_millis() as f64 / 1000.0);
    }

    /// Calculate packets send rate.
    fn calculate_send_rate(&mut self, now: Instant) {
        let pos = self.last_sendqueue_counter as usize % CONGESTION_QUEUE_ARRAY_SIZE;
        let n_p_pos = self.last_sendqueue_counter as usize % CONGESTION_LAST_SENT_ARRAY_SIZE;
        self.last_sendqueue_counter = (self.last_sendqueue_counter + 1) %
            // divide by the common multiple to prevent overflow
            (CONGESTION_QUEUE_ARRAY_SIZE * CONGESTION_LAST_SENT_ARRAY_SIZE) as u32;

        let send_array_len = self.send_array.len();

        self.last_send_array_sizes[pos] = send_array_len;
        self.last_num_packets_sent[n_p_pos] = self.packets_sent;
        self.last_num_packets_resent[n_p_pos] = self.packets_resent;

        // How changed the size of send_array per CONGESTION_QUEUE_ARRAY_SIZE * PACKET_COUNTER_AVERAGE_INTERVAL interval
        let sum = send_array_len as i32 - self.last_send_array_sizes[(pos + 1) % CONGESTION_QUEUE_ARRAY_SIZE] as i32;

        // The maximum allowed delay
        const CONGESTION_MAX_DELAY: usize = CONGESTION_LAST_SENT_ARRAY_SIZE - CONGESTION_QUEUE_ARRAY_SIZE;

        // Based on rtt offset in number of positions for last_num_packets arrays (one position equals 50 ms)
        let delay = ((
            self.rtt.as_secs() * 1000 +
                self.rtt.subsec_millis() as u64 +
                PACKET_COUNTER_AVERAGE_INTERVAL / 2 // add half of the interval to make delay rounded
        ) / PACKET_COUNTER_AVERAGE_INTERVAL) as usize;
        let delay = delay.min(CONGESTION_MAX_DELAY);

        // Total number of sent packets per CONGESTION_QUEUE_ARRAY_SIZE * PACKET_COUNTER_AVERAGE_INTERVAL interval
        // For instance if the delay is 3 elements marked with '+' will be taken ('x' is the current pos):
        // ...++++++++++++..x......
        let mut total_sent = 0;
        let mut total_resent = 0;
        for i in 0 .. CONGESTION_QUEUE_ARRAY_SIZE {
            let i = (n_p_pos + (CONGESTION_MAX_DELAY - delay) + i) % CONGESTION_LAST_SENT_ARRAY_SIZE;
            total_sent += self.last_num_packets_sent[i];
            total_resent += self.last_num_packets_resent[i];
        }

        if sum > 0 {
            // send_array increased i.e. we sent more packets that was delivered
            // decrease total_sent packets by this number so that it includes only delivered packets
            total_sent -= sum as u32;
        } else if total_resent > -sum as u32 {
            // send_array decreased and not all resent packets were delivered
            // use this number to count only delivered packets
            total_resent = -sum as u32;
        }

        // Average number of successfully delivered packets per second
        let coeff = 1000.0 / (CONGESTION_QUEUE_ARRAY_SIZE as f64 * PACKET_COUNTER_AVERAGE_INTERVAL as f64);
        let min_speed = total_sent as f64 * coeff;
        let min_speed_request = (total_sent + total_resent) as f64 * coeff;

        // Time necessary to send all packets from send queue
        let send_array_time = send_array_len as f64 / min_speed;

        // And, finally, estimated packets send rate
        let packet_send_rate = if send_array_time > SEND_QUEUE_CLEARANCE_TIME && send_array_len > CRYPTO_MIN_QUEUE_LENGTH {
            // It will take more than SEND_QUEUE_CLEARANCE_TIME seconds to send
            // all packets from send queue. Reduce packets send rate in this case
            min_speed / (send_array_time / SEND_QUEUE_CLEARANCE_TIME)
        } else if self.last_congestion_event.map_or(true, |time| (now - time) > Duration::from_millis(CONGESTION_EVENT_TIMEOUT)) {
            // Congestion event happened long ago so increase packets send rate
            min_speed * 1.2
        } else {
            // Congestion event happened recently so decrease packets send rate
            min_speed * 0.9
        };
        let packet_send_rate = packet_send_rate.max(CRYPTO_PACKET_MIN_RATE);
        let packet_send_rate_requested = min_speed_request * 1.2;
        let packet_send_rate_requested = packet_send_rate_requested.max(packet_send_rate);

        self.packet_send_rate = packet_send_rate;
        self.packet_send_rate_requested = packet_send_rate_requested;
    }

    /// Reset congestion counters after they were used for stats calculation.
    fn reset_congestion_counters(&mut self, now: Instant) {
        self.packets_received = 0;
        self.packets_sent = 0;
        self.packets_resent = 0;
        self.stats_calculation_time = now;
    }

    /// Update stats necessary for congestion control. Should be called every 50 ms.
    pub fn update_congestion_stats(&mut self) {
        let now = clock_now();
        self.calculate_recv_rate(now);
        self.calculate_send_rate(now);
        self.reset_congestion_counters(now);
    }

    /// Calculate the interval in ms for request packet.
    pub fn request_packet_interval(&self) -> Duration {
        let request_packet_interval = REQUEST_PACKETS_COMPARE_CONSTANT / ((self.recv_array.len() as f64 + 1.0) / (self.packet_recv_rate + 1.0));
        let request_packet_interval = request_packet_interval.min(
            CRYPTO_PACKET_MIN_RATE / self.packet_recv_rate * CRYPTO_SEND_PACKET_INTERVAL as f64 + PACKET_COUNTER_AVERAGE_INTERVAL as f64
        );
        let request_packet_interval = (request_packet_interval.round() as u64)
            .max(PACKET_COUNTER_AVERAGE_INTERVAL) // lower bound
            .min(CRYPTO_SEND_PACKET_INTERVAL); // upper bound
        Duration::from_millis(request_packet_interval)
    }

    /// Check if the connection is established.
    pub fn is_established(&self) -> bool {
        match self.status {
            ConnectionStatus::Established { .. } => true,
            _ => false,
        }
    }

    /// Check if the connection is not confirmed.
    pub fn is_not_confirmed(&self) -> bool {
        match self.status {
            ConnectionStatus::NotConfirmed { .. } => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio_executor;
    use tokio_timer::clock::*;

    use toxcore::time::ConstNow;

    #[test]
    fn status_packet_should_be_sent() {
        crypto_init();
        // just created packet should be sent
        let mut packet = StatusPacket::new_cookie_request(CookieRequest {
            pk: gen_keypair().0,
            nonce: gen_nonce(),
            payload: vec![42; 88]
        });

        assert!(packet.should_be_sent());

        // packet shouldn't be sent if it was sent not earlier than 1 second ago
        packet.num_sent += 1;
        assert!(!packet.should_be_sent());

        let mut enter = tokio_executor::enter().unwrap();
        let clock = Clock::new_with_now(ConstNow(
            packet.sent_time + Duration::from_millis(CRYPTO_SEND_PACKET_INTERVAL + 1000)
        ));

        with_default(&clock, &mut enter, |_| {
            // packet should be sent if it was sent earlier than 1 second ago
            assert!(packet.should_be_sent());
            // packet shouldn't be sent if it was sent 8 times or more
            packet.num_sent += MAX_NUM_SENDPACKET_TRIES;
            assert!(!packet.should_be_sent());
        });
    }

    #[test]
    fn status_packet_is_timed_out() {
        crypto_init();
        // just created packet isn't timed out
        let mut packet = StatusPacket::new_cookie_request(CookieRequest {
            pk: gen_keypair().0,
            nonce: gen_nonce(),
            payload: vec![42; 88]
        });

        assert!(!packet.is_timed_out());

        // packet is timed out if it was sent 8 times and 1 second elapsed since last sending
        packet.num_sent += MAX_NUM_SENDPACKET_TRIES;

        let mut enter = tokio_executor::enter().unwrap();
        let clock = Clock::new_with_now(ConstNow(
            packet.sent_time + Duration::from_millis(CRYPTO_SEND_PACKET_INTERVAL + 1000)
        ));

        with_default(&clock, &mut enter, |_| {
            assert!(packet.is_timed_out());
        });
    }

    #[test]
    fn sent_packet_clone() {
        crypto_init();
        let sent_packet = SentPacket::new(vec![42; 123]);
        let sent_packet_c = sent_packet.clone();
        assert_eq!(sent_packet_c, sent_packet);
    }

    #[test]
    fn recv_packet_clone() {
        crypto_init();
        let recv_packet = RecvPacket::new(vec![42; 123]);
        let recv_packet_c = recv_packet.clone();
        assert_eq!(recv_packet_c, recv_packet);
    }

    #[test]
    fn crypto_connection_clone() {
        crypto_init();
        let (dht_pk, dht_sk) = gen_keypair();
        let (real_pk, _real_sk) = gen_keypair();
        let (peer_dht_pk, _peer_dht_sk) = gen_keypair();
        let (peer_real_pk, _peer_real_sk) = gen_keypair();
        let mut connection = CryptoConnection::new(&dht_sk, dht_pk, real_pk, peer_real_pk, peer_dht_pk);

        let connection_c = connection.clone();
        assert_eq!(connection_c, connection);

        let crypto_handshake = CryptoHandshake {
            cookie: EncryptedCookie {
                nonce: secretbox::gen_nonce(),
                payload: vec![42; 88]
            },
            nonce: gen_nonce(),
            payload: vec![42; 248]
        };

        connection.status = ConnectionStatus::HandshakeSending {
            sent_nonce: gen_nonce(),
            packet: StatusPacket::new_crypto_handshake(crypto_handshake.clone())
        };

        let connection_c = connection.clone();
        assert_eq!(connection_c, connection);

        let (peer_session_pk, _peer_session_sk) = gen_keypair();
        let (_session_pk, session_sk) = gen_keypair();
        let session_precomputed_key = precompute(&peer_session_pk, &session_sk);
        connection.status = ConnectionStatus::NotConfirmed {
            sent_nonce: gen_nonce(),
            received_nonce: gen_nonce(),
            peer_session_pk,
            session_precomputed_key,
            packet: StatusPacket::new_crypto_handshake(crypto_handshake),
        };

        let connection_c = connection.clone();
        assert_eq!(connection_c, connection);

        let (peer_session_pk, _peer_session_sk) = gen_keypair();
        let (_session_pk, session_sk) = gen_keypair();
        let session_precomputed_key = precompute(&peer_session_pk, &session_sk);
        connection.status = ConnectionStatus::Established {
            sent_nonce: gen_nonce(),
            received_nonce: gen_nonce(),
            peer_session_pk,
            session_precomputed_key,
        };

        let connection_c = connection.clone();
        assert_eq!(connection_c, connection);
    }

    #[test]
    fn update_congestion_stats() {
        crypto_init();
        let (dht_pk, dht_sk) = gen_keypair();
        let (real_pk, _real_sk) = gen_keypair();
        let (peer_dht_pk, _peer_dht_sk) = gen_keypair();
        let (peer_real_pk, _peer_real_sk) = gen_keypair();
        let mut connection = CryptoConnection::new(&dht_sk, dht_pk, real_pk, peer_real_pk, peer_dht_pk);

        let now = Instant::now();

        connection.stats_calculation_time = now;
        connection.packets_received = 300;
        connection.packets_sent = 200;
        connection.packets_resent = 100;

        let next_now = now + Duration::from_millis(PACKET_COUNTER_AVERAGE_INTERVAL);
        let mut enter = tokio_executor::enter().unwrap();
        let clock = Clock::new_with_now(ConstNow(next_now));

        with_default(&clock, &mut enter, |_| {
            connection.update_congestion_stats();
        });

        assert_eq!(connection.packets_received, 0);
        assert_eq!(connection.packets_sent, 0);
        assert_eq!(connection.packets_resent, 0);
        assert_eq!(connection.stats_calculation_time, next_now);
        assert_eq!(connection.packet_recv_rate, 6000.0);
    }

    #[test]
    fn request_packet_interval() {
        crypto_init();
        let (dht_pk, dht_sk) = gen_keypair();
        let (real_pk, _real_sk) = gen_keypair();
        let (peer_dht_pk, _peer_dht_sk) = gen_keypair();
        let (peer_real_pk, _peer_real_sk) = gen_keypair();
        let mut connection = CryptoConnection::new(&dht_sk, dht_pk, real_pk, peer_real_pk, peer_dht_pk);

        connection.packet_recv_rate = 500.0;

        // increasing packets queue length should cause the interval decreasing
        for &(len, interval) in &[
            (80, 58),
            (90, 58),
            (100, 58),
            (110, 56),
            (120, 52),
            (130, 50),
            (140, 50),
            (150, 50),
        ] {
            connection.recv_array.buffer_end = len;
            assert_eq!(connection.request_packet_interval().subsec_millis(), interval);
        }
    }

    #[test]
    fn is_established() {
        crypto_init();
        let (dht_pk, dht_sk) = gen_keypair();
        let (real_pk, _real_sk) = gen_keypair();
        let (peer_dht_pk, _peer_dht_sk) = gen_keypair();
        let (peer_real_pk, _peer_real_sk) = gen_keypair();
        let mut connection = CryptoConnection::new(&dht_sk, dht_pk, real_pk, peer_real_pk, peer_dht_pk);

        connection.status = ConnectionStatus::Established {
            sent_nonce: gen_nonce(),
            received_nonce: gen_nonce(),
            peer_session_pk: gen_keypair().0,
            session_precomputed_key: precompute(&gen_keypair().0, &gen_keypair().1),
        };

        assert!(!connection.is_not_confirmed());
        assert!(connection.is_established());
    }

    #[test]
    fn is_not_confirmed() {
        crypto_init();
        let (dht_pk, dht_sk) = gen_keypair();
        let (real_pk, _real_sk) = gen_keypair();
        let (peer_dht_pk, _peer_dht_sk) = gen_keypair();
        let (peer_real_pk, _peer_real_sk) = gen_keypair();
        let mut connection = CryptoConnection::new(&dht_sk, dht_pk, real_pk, peer_real_pk, peer_dht_pk);

        let crypto_handshake = CryptoHandshake {
            cookie: EncryptedCookie {
                nonce: secretbox::gen_nonce(),
                payload: vec![42; 88]
            },
            nonce: gen_nonce(),
            payload: vec![42; 248]
        };

        connection.status = ConnectionStatus::NotConfirmed {
            sent_nonce: gen_nonce(),
            received_nonce: gen_nonce(),
            peer_session_pk: gen_keypair().0,
            session_precomputed_key: precompute(&gen_keypair().0, &gen_keypair().1),
            packet: StatusPacket::new_crypto_handshake(crypto_handshake),
        };

        assert!(connection.is_not_confirmed());
        assert!(!connection.is_established());
    }
}
