use super::*;

use counters::Counters;
use crossbeam_queue::ArrayQueue;
use packet::PacketPool;
use socket::RawSock;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time;

const NUM_PKTS: usize = 10;
const NUM_PART: usize = 20;
const PART_SZ: usize = 3072;
const MAX_PACKET: usize = 1500;

fn packet_free(q: Arc<ArrayQueue<BoxPkt>>, pool: &mut dyn PacketPool) {
    while let Ok(p) = q.pop() {
        pool.free(p);
    }
}

fn packet_pool(test: &str, q: Arc<ArrayQueue<BoxPkt>>) -> Box<PktsDpdk> {
    let mut counters = Counters::new(test).unwrap();
    Box::new(PktsDpdk::new(
        test,
        q,
        &mut counters,
        NUM_PKTS,
        NUM_PART,
        PART_SZ,
    ))
}

fn delete_veth() {
    let args = [
        "link", "del", "r2_eth1", "type", "veth", "peer", "name", "r2_eth2",
    ];
    Command::new("ip")
        .args(&args)
        .spawn()
        .expect("veth failed")
        .wait()
        .unwrap();
}

// We get random packets if ipv6 is enabled, we want only our own packets
fn disable_ipv6(eth: &str) -> String {
    let mut name = "net.ipv6.conf.".to_string();
    name.push_str(eth);
    name.push_str(".disable_ipv6=1");
    name
}

fn create_veth() {
    let args = [
        "link", "add", "r2_eth1", "type", "veth", "peer", "name", "r2_eth2",
    ];
    Command::new("ip")
        .args(&args)
        .spawn()
        .expect("veth failed")
        .wait()
        .unwrap();

    let args = ["r2_eth1", "up"];
    Command::new("ifconfig")
        .args(&args)
        .spawn()
        .expect("ifconfig eth1 fail")
        .wait()
        .unwrap();
    let args = ["-w", &disable_ipv6("r2_eth1")];
    Command::new("sysctl")
        .args(&args)
        .spawn()
        .expect("ipv6 disable fail")
        .wait()
        .unwrap();

    let args = ["-w", &disable_ipv6("r2_eth2")];
    Command::new("sysctl")
        .args(&args)
        .spawn()
        .expect("ipv6 disable fail")
        .wait()
        .unwrap();
    let args = ["r2_eth2", "up"];
    Command::new("ifconfig")
        .args(&args)
        .spawn()
        .expect("ifconfig eth2 fail")
        .wait()
        .unwrap();
}

#[test]
fn read_write() {
    delete_veth();
    create_veth();

    let mut glob = DpdkGlobal::new(128, 1);

    let q_tx = Arc::new(ArrayQueue::new(NUM_PKTS));
    let mut pool_tx = packet_pool("dpdk_read_write_tx", q_tx.clone());
    let params = Params {
        name: "r2_eth1",
        hw: DpdkHw::AfPacket,
        pool: pool_tx.dpdk_pool,
    };
    let dpdk_tx = match glob.add(params) {
        Ok(dpdk) => dpdk,
        Err(err) => panic!("Error {:?} creating dpdk port", err),
    };

    let q_rx = Arc::new(ArrayQueue::new(NUM_PKTS));
    let mut pool_rx = packet_pool("dpdk_read_write_rx", q_rx.clone());
    let params = Params {
        name: "r2_eth2",
        hw: DpdkHw::AfPacket,
        pool: pool_rx.dpdk_pool,
    };
    let dpdk_rx = match glob.add(params) {
        Ok(dpdk) => dpdk,
        Err(err) => panic!("Error {:?} creating dpdk port", err),
    };

    let wait = Arc::new(AtomicUsize::new(0));
    let done = wait.clone();
    let tname = "dpdk_eal".to_string();
    let handler = thread::Builder::new().name(tname).spawn(move || {
        let data: Vec<u8> = (0..MAX_PACKET).map(|x| (x % 256) as u8).collect();
        loop {
            packet_free(q_rx.clone(), &mut *pool_rx);
            packet_free(q_tx.clone(), &mut *pool_tx);
            let pkt = pool_tx.pkt(0);
            if pkt.is_none() {
                continue;
            }
            let mut pkt = pkt.unwrap();
            assert!(pkt.append(&mut *pool_tx, &data[0..]));
            assert_eq!(dpdk_tx.sendmsg(pkt), MAX_PACKET);

            let pkt = dpdk_rx.recvmsg(&mut *pool_rx, 0);
            if pkt.is_none() {
                continue;
            }
            let pkt = pkt.unwrap();
            let pktlen = pkt.len();
            assert_eq!(MAX_PACKET, pktlen);
            let (buf, len) = match pkt.data(0) {
                Some((d, s)) => (d, s),
                None => panic!("Cant get offset 0"),
            };
            assert_eq!(len, pktlen);
            for i in 0..MAX_PACKET {
                assert_eq!(buf[i], i as u8);
            }
            done.fetch_add(1, Ordering::Relaxed);
        }
    });

    while wait.load(Ordering::Relaxed) == 0 {
        let wait = time::Duration::from_millis(1);
        thread::sleep(wait)
    }

    handler.unwrap().join().unwrap();
    delete_veth();
}
