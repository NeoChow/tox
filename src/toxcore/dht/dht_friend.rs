/*!
Module for friend.
*/

use std::time::{Duration, Instant};
use std::mem;
use std::net::SocketAddr;

use futures::{future, Future, stream, Stream};

use toxcore::time::*;
use toxcore::dht::packed_node::*;
use toxcore::dht::kbucket::*;
use toxcore::crypto_core::*;
use toxcore::dht::server::*;
use toxcore::io_tokio::*;
use toxcore::dht::server::hole_punching::*;

/// Hold friend related info.
#[derive(Clone, Debug)]
pub struct DhtFriend {
    /// Public key of friend
    pub pk: PublicKey,
    /// close nodes of friend
    pub close_nodes: Bucket,
    // Last time of NodesRequest packet sent
    last_nodes_req_time: Option<Instant>,
    // Counter for bootstappings.
    bootstrap_times: u32,
    /// Nodes to bootstrap.
    pub bootstrap_nodes: Bucket,
    /// struct for hole punching
    pub hole_punch: HolePunching,
}

impl DhtFriend {
    /// Create new `DhtFriend`.
    pub fn new(pk: PublicKey) -> Self {
        DhtFriend {
            pk,
            close_nodes: Bucket::new(None),
            last_nodes_req_time: None,
            bootstrap_times: 0,
            bootstrap_nodes: Bucket::new(None),
            hole_punch: HolePunching::new(),
        }
    }

    /// send NodesRequest packet to bootstap_nodes, close list
    pub fn send_nodes_req_packets(&mut self, server: &Server) -> IoFuture<()> {
        let ping_bootstrap_nodes = self.ping_bootstrap_nodes(server);
        let ping_and_get_close_nodes = self.ping_and_get_close_nodes(server);
        let send_nodes_req_random = self.send_nodes_req_random(server);

        let res = ping_bootstrap_nodes.join3(
            ping_and_get_close_nodes, send_nodes_req_random
            ).map(|_| () );

        Box::new(res)
    }

    // send NodesRequest to ping on nodes gotten by NodesResponse
    fn ping_bootstrap_nodes(&mut self, server: &Server) -> IoFuture<()> {
        let mut bootstrap_nodes = Bucket::new(None);
        mem::swap(&mut bootstrap_nodes, &mut self.bootstrap_nodes);

        let mut request_queue = server.request_queue.write();

        let bootstrap_nodes = bootstrap_nodes.to_packed();
        let nodes_sender = bootstrap_nodes.iter()
            .map(|node|
                server.send_nodes_req(*node, self.pk, request_queue.new_ping_id(node.pk))
            );

        let nodes_stream = stream::futures_unordered(nodes_sender).then(|_| Ok(()));

        Box::new(nodes_stream.for_each(|()| Ok(())))
    }

    // ping to close nodes of friend
    fn ping_and_get_close_nodes(&mut self, server: &Server) -> IoFuture<()> {
        let mut request_queue = server.request_queue.write();

        let pk = self.pk;
        let nodes_sender = self.close_nodes.nodes.iter_mut()
            .map(|node| {
                if node.last_ping_req_time.map_or(true, |time| time.elapsed() >= Duration::from_secs(PING_INTERVAL)) {
                    node.last_ping_req_time = Some(Instant::now());
                    server.send_nodes_req(node.clone().into(), pk, request_queue.new_ping_id(node.pk))
                } else {
                    Box::new(future::ok(()))
                }
            });

        let nodes_stream = stream::futures_unordered(nodes_sender).then(|_| Ok(()));

        Box::new(nodes_stream.for_each(|()| Ok(())))
    }

    // send NodesRequest to random node which is in close list
    fn send_nodes_req_random(&mut self, server: &Server) -> IoFuture<()> {
        if self.last_nodes_req_time.map_or(false, |time| clock_elapsed(time) < Duration::from_secs(NODES_REQ_INTERVAL)) &&
            self.bootstrap_times >= MAX_BOOTSTRAP_TIMES {
            return Box::new(future::ok(()));
        }

        let good_nodes = self.close_nodes.nodes.iter()
            .filter(|&node| !node.is_bad())
            .map(|node| node.clone().into())
            .collect::<Vec<PackedNode>>();

        if !good_nodes.is_empty() {
            let mut request_queue = server.request_queue.write();

            let num_nodes = good_nodes.len();
            let mut random_node = random_usize() % num_nodes;
            // increase probability of sending packet to a close node (has lower index)
            if random_node != 0 {
                random_node -= random_usize() % (random_node + 1);
            }

            let random_node = good_nodes[random_node];

            let res = server.send_nodes_req(random_node, self.pk, request_queue.new_ping_id(random_node.pk));
            self.bootstrap_times += 1;
            self.last_nodes_req_time = Some(Instant::now());

            res
        } else {
            Box::new(future::ok(()))
        }
    }

    /// get Socket Address list of a friend, a friend can have multi IP address bacause of NAT
    pub fn get_addrs_of_clients(&self, is_ipv6_mode: bool) -> Vec<SocketAddr> {
        self.close_nodes.nodes.iter()
            .map(|node| node.get_socket_addr(is_ipv6_mode))
            .filter_map(|addr| addr)
            .collect::<Vec<SocketAddr>>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use toxcore::dht::packet::*;
    use futures::sync::mpsc;
    use futures::Future;

    #[test]
    fn friend_new_test() {
        let _ = DhtFriend::new(gen_keypair().0);
    }

    #[test]
    fn friend_ping_bootstrap_nodes_test() {
        crypto_init();

        let (friend_pk, _friend_sk) = gen_keypair();
        let mut friend = DhtFriend::new(friend_pk);
        let (pk, sk) = gen_keypair();
        let (tx, rx) = mpsc::unbounded::<(DhtPacket, SocketAddr)>();
        let server = Server::new(tx, pk, sk.clone());

        let (node_pk1, node_sk1) = gen_keypair();
        let (node_pk2, node_sk2) = gen_keypair();
        assert!(friend.bootstrap_nodes.try_add(&friend_pk, &PackedNode {
            pk: node_pk1,
            saddr: "127.0.0.1:33445".parse().unwrap(),
        }));
        assert!(friend.bootstrap_nodes.try_add(&friend_pk, &PackedNode {
            pk: node_pk2,
            saddr: "127.0.0.1:33446".parse().unwrap(),
        }));

        assert!(friend.send_nodes_req_packets(&server).wait().is_ok());

        rx.take(2).map(|(packet, addr)| {
            let nodes_req = unpack!(packet, DhtPacket::NodesRequest);

            let mut request_queue = server.request_queue.write();

            if addr == SocketAddr::V4("127.0.0.1:33445".parse().unwrap()) {
                let nodes_req_payload = nodes_req.get_payload(&node_sk1).unwrap();
                assert!(request_queue.check_ping_id(node_pk1, nodes_req_payload.id));
            } else {
                let nodes_req_payload = nodes_req.get_payload(&node_sk2).unwrap();
                assert!(request_queue.check_ping_id(node_pk2, nodes_req_payload.id));
            }
        }).collect().wait().unwrap();
    }

    #[test]
    fn friend_ping_and_get_close_nodes_test() {
        crypto_init();

        let (friend_pk, _friend_sk) = gen_keypair();
        let mut friend = DhtFriend::new(friend_pk);
        let (pk, sk) = gen_keypair();
        let (tx, rx) = mpsc::unbounded::<(DhtPacket, SocketAddr)>();
        let server = Server::new(tx, pk, sk.clone());

        // Test with no close_nodes entry, send nothing, but return ok
        assert!(friend.send_nodes_req_packets(&server).wait().is_ok());

        // Now, test with close_nodes entry
        let (node_pk1, node_sk1) = gen_keypair();
        let (node_pk2, node_sk2) = gen_keypair();
        assert!(friend.close_nodes.try_add(&friend_pk, &PackedNode {
            pk: node_pk1,
            saddr: "127.0.0.1:33445".parse().unwrap(),
        }));
        assert!(friend.close_nodes.try_add(&friend_pk, &PackedNode {
            pk: node_pk2,
            saddr: "127.0.0.1:33446".parse().unwrap(),
        }));

        // Now send packet
        assert!(friend.send_nodes_req_packets(&server).wait().is_ok());

        rx.take(2).map(|(packet, addr)| {
            let nodes_req = unpack!(packet, DhtPacket::NodesRequest);

            let mut request_queue = server.request_queue.write();

            if addr == SocketAddr::V4("127.0.0.1:33445".parse().unwrap()) {
                let nodes_req_payload = nodes_req.get_payload(&node_sk1).unwrap();
                assert!(request_queue.check_ping_id(node_pk1, nodes_req_payload.id));
            } else {
                let nodes_req_payload = nodes_req.get_payload(&node_sk2).unwrap();
                assert!(request_queue.check_ping_id(node_pk2, nodes_req_payload.id));
            }
        }).collect().wait().unwrap();
    }

    #[test]
    fn friend_send_nodes_req_random_test() {
        crypto_init();

        let (friend_pk, _friend_sk) = gen_keypair();
        let mut friend = DhtFriend::new(friend_pk);
        let (pk, sk) = gen_keypair();
        let (tx, rx) = mpsc::unbounded::<(DhtPacket, SocketAddr)>();
        let server = Server::new(tx, pk, sk.clone());

        // Test with no close_nodes entry, send nothing, but return ok
        assert!(friend.send_nodes_req_packets(&server).wait().is_ok());

        // Now, test with close_nodes entry
        let (node_pk1, node_sk1) = gen_keypair();
        let (node_pk2, node_sk2) = gen_keypair();
        assert!(friend.close_nodes.try_add(&friend_pk, &PackedNode {
            pk: node_pk1,
            saddr: "127.0.0.1:33445".parse().unwrap(),
        }));
        assert!(friend.close_nodes.try_add(&friend_pk, &PackedNode {
            pk: node_pk2,
            saddr: "127.0.0.1:33446".parse().unwrap(),
        }));

        // Now send packet
        assert!(friend.send_nodes_req_packets(&server).wait().is_ok());

        let (received, _rx) = rx.into_future().wait().unwrap();
        let (packet, addr) = received.unwrap();
        let nodes_req = unpack!(packet, DhtPacket::NodesRequest);

        let mut request_queue = server.request_queue.write();

        if addr == SocketAddr::V4("127.0.0.1:33445".parse().unwrap()) {
            let nodes_req_payload = nodes_req.get_payload(&node_sk1).unwrap();
            assert!(request_queue.check_ping_id(node_pk1, nodes_req_payload.id));
        } else {
            let nodes_req_payload = nodes_req.get_payload(&node_sk2).unwrap();
            assert!(request_queue.check_ping_id(node_pk2, nodes_req_payload.id));
        }
    }

    #[test]
    fn friend_get_addrs_of_clients_test() {
        let (friend_pk, _friend_sk) = gen_keypair();
        let mut friend = DhtFriend::new(friend_pk);

        let (node_pk1, _node_sk1) = gen_keypair();
        assert!(friend.close_nodes.try_add(&friend_pk, &PackedNode {
            pk: node_pk1,
            saddr: "127.0.0.1:33445".parse().unwrap(),
        }));
        let (node_pk2, _node_sk2) = gen_keypair();
        assert!(friend.close_nodes.try_add(&friend_pk, &PackedNode {
            pk: node_pk2,
            saddr: "[2001:db8::1]:33445".parse().unwrap(),
        }));
        let (node_pk3, _node_sk3) = gen_keypair();
        assert!(friend.close_nodes.try_add(&friend_pk, &PackedNode {
            pk: node_pk3,
            saddr: "127.0.0.2:33445".parse().unwrap(),
        }));
        let (node_pk4, _node_sk4) = gen_keypair();
        assert!(friend.close_nodes.try_add(&friend_pk, &PackedNode {
            pk: node_pk4,
            saddr: "127.0.0.3:33445".parse().unwrap(),
        }));

        assert!(friend.get_addrs_of_clients(true).contains(&"127.0.0.1:33445".parse().unwrap()));
        assert!(friend.get_addrs_of_clients(true).contains(&"127.0.0.2:33445".parse().unwrap()));
        assert!(friend.get_addrs_of_clients(true).contains(&"127.0.0.3:33445".parse().unwrap()));
        assert!(friend.get_addrs_of_clients(true).contains(&"[2001:db8::1]:33445".parse().unwrap()));
    }
}
