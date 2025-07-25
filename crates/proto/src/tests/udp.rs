use alloc::string::ToString;
use alloc::sync::Arc;
use core::str::FromStr;
use core::sync::atomic::AtomicBool;
use core::time::Duration;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::println;

use futures_util::stream::StreamExt;
use tracing::debug;

use crate::op::{Message, Query};
use crate::rr::rdata::NULL;
use crate::rr::{Name, RData, Record, RecordType};
use crate::runtime::RuntimeProvider;
use crate::udp::{UdpClientStream, UdpStream};
use crate::xfer::dns_handle::DnsStreamHandle;
use crate::xfer::{DnsRequest, DnsRequestOptions, DnsRequestSender, FirstAnswer, SerialMessage};

/// Test next random udpsocket.
pub async fn next_random_socket_test(provider: impl RuntimeProvider) {
    let (stream, _) = UdpStream::new(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 52),
        None,
        None,
        false,
        provider,
    );
    drop(stream.await.expect("failed to get next socket address"));
}

/// Test udp_stream.
pub async fn udp_stream_test<P: RuntimeProvider>(server_addr: IpAddr, provider: P) {
    let succeeded = Arc::new(AtomicBool::new(false));
    let succeeded_clone = succeeded.clone();
    std::thread::Builder::new()
        .name("thread_killer".to_string())
        .spawn(move || {
            let succeeded = succeeded_clone;
            for _ in 0..15 {
                std::thread::sleep(core::time::Duration::from_secs(1));
                if succeeded.load(core::sync::atomic::Ordering::Relaxed) {
                    return;
                }
            }

            println!("Thread Killer has been awoken, killing process");
            std::process::exit(-1);
        })
        .unwrap();

    let server = std::net::UdpSocket::bind(SocketAddr::new(server_addr, 0)).unwrap();
    server
        .set_read_timeout(Some(core::time::Duration::from_secs(5)))
        .unwrap(); // should receive something within 5 seconds...
    server
        .set_write_timeout(Some(core::time::Duration::from_secs(5)))
        .unwrap(); // should receive something within 5 seconds...
    let server_addr = server.local_addr().unwrap();
    println!("server listening on: {server_addr}");

    let test_bytes: &'static [u8; 8] = b"DEADBEEF";
    let send_recv_times = 4u32;

    // an in and out server
    let server_handle = std::thread::Builder::new()
        .name("test_udp_stream_ipv4:server".to_string())
        .spawn(move || {
            let mut buffer = [0_u8; 512];

            for _ in 0..send_recv_times {
                // wait for some bytes...
                //println!("receiving message: {}", _i);
                let (len, addr) = server.recv_from(&mut buffer).expect("receive failed");

                assert_eq!(&buffer[0..len], test_bytes);

                //println!("sending message len back: {}", len);
                // bounce them right back...
                assert_eq!(
                    server.send_to(&buffer[0..len], addr).expect("send failed"),
                    len
                );
            }
        })
        .unwrap();

    // setup the client, which is going to run on the testing thread...
    // the tests should run within 5 seconds... right?
    // TODO: add timeout here, so that test never hangs...
    let client_addr = match server_addr {
        SocketAddr::V4(_) => "127.0.0.1:0",
        SocketAddr::V6(_) => "[::1]:0",
    };

    println!("binding client socket");
    let socket = provider
        .bind_udp(
            client_addr.to_socket_addrs().unwrap().next().unwrap(),
            server_addr,
        )
        .await
        .expect("could not create socket"); // some random address...
    println!("bound client socket");

    let (mut stream, mut sender) = UdpStream::<P>::with_bound(socket, server_addr);

    for _i in 0..send_recv_times {
        // test once
        sender
            .send(SerialMessage::new(test_bytes.to_vec(), server_addr))
            .unwrap();
        //println!("blocking on client stream: {}", _i);
        let buffer_and_addr = stream.next().await;
        //println!("got message");
        let message = buffer_and_addr.expect("no message").expect("io error");
        assert_eq!(message.bytes(), test_bytes);
        assert_eq!(message.addr(), server_addr);
    }

    succeeded.store(true, core::sync::atomic::Ordering::Relaxed);
    server_handle.join().expect("server thread failed");
}

/// Test udp_client_stream.
#[allow(clippy::print_stdout)]
pub async fn udp_client_stream_test(server_addr: IpAddr, provider: impl RuntimeProvider) {
    let succeeded = Arc::new(AtomicBool::new(false));
    let succeeded_clone = succeeded.clone();
    std::thread::Builder::new()
        .name("thread_killer".to_string())
        .spawn(move || {
            let succeeded = succeeded_clone;
            for _ in 0..15 {
                std::thread::sleep(core::time::Duration::from_secs(1));
                if succeeded.load(core::sync::atomic::Ordering::Relaxed) {
                    return;
                }
            }

            println!("Thread Killer has been awoken, killing process");
            std::process::exit(-1);
        })
        .unwrap();

    let server = std::net::UdpSocket::bind(SocketAddr::new(server_addr, 0)).unwrap();
    server
        .set_read_timeout(Some(core::time::Duration::from_secs(5)))
        .unwrap(); // should receive something within 5 seconds...
    server
        .set_write_timeout(Some(core::time::Duration::from_secs(5)))
        .unwrap(); // should receive something within 5 seconds...
    let server_addr = server.local_addr().unwrap();

    let mut query = Message::query();
    let test_name = Name::from_str("dead.beef.").unwrap();
    query.add_query(Query::query(test_name.clone(), RecordType::NULL));
    let test_bytes: &'static [u8; 8] = b"DEADBEEF";
    let send_recv_times = 4;

    let test_name_server = test_name;
    // an in and out server
    let server_handle = std::thread::Builder::new()
        .name("test_udp_client_stream_ipv4:server".to_string())
        .spawn(move || {
            let mut buffer = [0_u8; 512];

            for i in 0..send_recv_times {
                // wait for some bytes...
                debug!("server receiving request {}", i);
                let (len, addr) = server.recv_from(&mut buffer).expect("receive failed");
                debug!("server received request {} from: {}", i, addr);

                let request = Message::from_vec(&buffer[0..len]).expect("failed parse of request");
                assert_eq!(*request.queries()[0].name(), test_name_server.clone());
                assert_eq!(request.queries()[0].query_type(), RecordType::NULL);

                let mut message = Message::query();
                message.set_id(request.id());
                message.add_queries(request.queries().to_vec());
                message.add_answer(Record::from_rdata(
                    test_name_server.clone(),
                    0,
                    RData::NULL(NULL::with(test_bytes.to_vec())),
                ));

                // bounce them right back...
                let bytes = message.to_vec().unwrap();
                debug!("server sending response {i} to: {addr}");
                assert_eq!(
                    server.send_to(&bytes, addr).expect("send failed"),
                    bytes.len()
                );
                debug!("server sent response {i}");
                std::thread::yield_now();
            }
        })
        .unwrap();

    // setup the client, which is going to run on the testing thread...

    // the tests should run within 5 seconds... right?
    // TODO: add timeout here, so that test never hangs...
    // let timeout = Timeout::new(Duration::from_secs(5));
    let stream = UdpClientStream::builder(server_addr, provider)
        .with_timeout(Some(Duration::from_millis(500)))
        .build();
    let mut stream = stream.await.unwrap();
    let mut worked_once = false;

    for i in 0..send_recv_times {
        // test once
        let response_stream =
            stream.send_message(DnsRequest::new(query.clone(), DnsRequestOptions::default()));
        println!("client sending request {i}");
        let response = match response_stream.first_answer().await {
            Ok(response) => response,
            Err(err) => {
                println!("failed to get message: {err}");
                continue;
            }
        };
        println!("client got response {i}");

        let response = Message::from(response);
        if let RData::NULL(null) = response.answers()[0].data() {
            assert_eq!(null.anything(), test_bytes);
        } else {
            panic!("not a NULL response");
        }

        worked_once = true;
    }

    succeeded.store(true, core::sync::atomic::Ordering::Relaxed);
    server_handle.join().expect("server thread failed");

    assert!(worked_once);
}
