// Copyright 2015-2022 Benjamin Fry <benjaminfry@me.com>
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// https://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// https://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use alloc::{boxed::Box, sync::Arc};
use core::{
    fmt::{self, Display},
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use std::{io, net::SocketAddr};

use futures_util::{
    future::{BoxFuture, FutureExt},
    stream::Stream,
};
use quinn::{
    ClientConfig, Connection, Endpoint, TransportConfig, VarInt, crypto::rustls::QuicClientConfig,
};
use tokio::time::timeout;

use crate::{
    error::ProtoError,
    quic::quic_stream::{DoqErrorCode, QuicStream},
    rustls::client_config,
    udp::UdpSocket,
    xfer::{CONNECT_TIMEOUT, DnsRequest, DnsRequestSender, DnsResponse, DnsResponseStream},
};

use super::{quic_config, quic_stream};

/// A DNS client connection for DNS-over-QUIC
#[must_use = "futures do nothing unless polled"]
#[derive(Clone)]
pub struct QuicClientStream {
    quic_connection: Connection,
    server_name: Arc<str>,
    name_server: SocketAddr,
    is_shutdown: bool,
}

impl Display for QuicClientStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        write!(formatter, "QUIC({},{})", self.name_server, self.server_name)
    }
}

impl QuicClientStream {
    /// Builder for QuicClientStream
    pub fn builder() -> QuicClientStreamBuilder {
        QuicClientStreamBuilder::default()
    }

    async fn inner_send(
        connection: Connection,
        message: DnsRequest,
    ) -> Result<DnsResponse, ProtoError> {
        let (send_stream, recv_stream) = connection.open_bi().await?;

        // RFC: The mapping specified here requires that the client selects a separate
        //  QUIC stream for each query. The server then uses the same stream to provide all the response messages for that query.
        let mut stream = QuicStream::new(send_stream, recv_stream);

        stream.send(message.into_parts().0).await?;

        // The client MUST send the DNS query over the selected stream,
        // and MUST indicate through the STREAM FIN mechanism that no further data will be sent on that stream.
        stream.finish().await?;

        stream.receive().await
    }
}

impl DnsRequestSender for QuicClientStream {
    /// The send loop for QUIC in DNS stipulates that a new QUIC "stream" should be opened and use for sending data.
    ///
    /// It should be closed after receiving the response. TODO: AXFR/IXFR support...
    ///
    /// ```text
    /// RFC 9250    DNS over Dedicated QUIC Connections
    ///
    /// 4.2.  Stream Mapping and Usage
    ///
    ///    The mapping of DNS traffic over QUIC streams takes advantage of the
    ///    QUIC stream features detailed in Section 2 of [RFC9000], the QUIC
    ///    transport specification.
    ///
    ///    DNS query/response traffic [RFC1034] [RFC1035] follows a simple
    ///    pattern in which the client sends a query, and the server provides
    ///    one or more responses (multiple responses can occur in zone
    ///    transfers).
    ///
    ///    The mapping specified here requires that the client select a separate
    ///    QUIC stream for each query.  The server then uses the same stream to
    ///    provide all the response messages for that query.  In order for
    ///    multiple responses to be parsed, a 2-octet length field is used in
    ///    exactly the same way as the 2-octet length field defined for DNS over
    ///    TCP [RFC1035].  The practical result of this is that the content of
    ///    each QUIC stream is exactly the same as the content of a TCP
    ///    connection that would manage exactly one query.
    ///
    ///    All DNS messages (queries and responses) sent over DoQ connections
    ///    MUST be encoded as a 2-octet length field followed by the message
    ///    content as specified in [RFC1035].
    ///
    ///    The client MUST select the next available client-initiated
    ///    bidirectional stream for each subsequent query on a QUIC connection,
    ///    in conformance with the QUIC transport specification [RFC9000].
    ///    Packet losses and other network events might cause queries to arrive
    ///    in a different order.  Servers SHOULD process queries as they arrive,
    ///    as not doing so would cause unnecessary delays.
    ///
    ///    The client MUST send the DNS query over the selected stream and MUST
    ///    indicate through the STREAM FIN mechanism that no further data will
    ///    be sent on that stream.
    ///
    ///    The server MUST send the response(s) on the same stream and MUST
    ///    indicate, after the last response, through the STREAM FIN mechanism
    ///    that no further data will be sent on that stream.
    ///
    ///    Therefore, a single DNS transaction consumes a single bidirectional
    ///    client-initiated stream.  This means that the client's first query
    ///    occurs on QUIC stream 0, the second on 4, and so on (see Section 2.1
    ///    of [RFC9000]).
    ///
    ///    Servers MAY defer processing of a query until the STREAM FIN has been
    ///    indicated on the stream selected by the client.
    ///
    ///    Servers and clients MAY monitor the number of "dangling" streams.
    ///    These are open streams where the following events have not occurred
    ///    after implementation-defined timeouts:
    ///
    ///    *  the expected queries or responses have not been received or,
    ///
    ///    *  the expected queries or responses have been received but not the
    ///       STREAM FIN
    ///
    ///    Implementations MAY impose a limit on the number of such dangling
    ///    streams.  If limits are encountered, implementations MAY close the
    ///    connection.
    ///
    /// 4.2.1.  DNS Message IDs
    ///
    ///    When sending queries over a QUIC connection, the DNS Message ID MUST
    ///    be set to 0.  The stream mapping for DoQ allows for unambiguous
    ///    correlation of queries and responses, so the Message ID field is not
    ///    required.
    ///
    ///    This has implications for proxying DoQ messages to and from other
    ///    transports.  For example, proxies may have to manage the fact that
    ///    DoQ can support a larger number of outstanding queries on a single
    ///    connection than, for example, DNS over TCP, because DoQ is not
    ///    limited by the Message ID space.  This issue already exists for DoH,
    ///    where a Message ID of 0 is recommended.
    ///
    ///    When forwarding a DNS message from DoQ over another transport, a DNS
    ///    Message ID MUST be generated according to the rules of the protocol
    ///    that is in use.  When forwarding a DNS message from another transport
    ///    over DoQ, the Message ID MUST be set to 0.
    /// ```
    fn send_message(&mut self, request: DnsRequest) -> DnsResponseStream {
        if self.is_shutdown {
            panic!("can not send messages after stream is shutdown")
        }

        Box::pin(Self::inner_send(self.quic_connection.clone(), request)).into()
    }

    fn shutdown(&mut self) {
        self.is_shutdown = true;
        self.quic_connection
            .close(DoqErrorCode::NoError.into(), b"Shutdown");
    }

    fn is_shutdown(&self) -> bool {
        self.is_shutdown
    }
}

impl Stream for QuicClientStream {
    type Item = Result<(), ProtoError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.is_shutdown {
            Poll::Ready(None)
        } else {
            Poll::Ready(Some(Ok(())))
        }
    }
}

/// A QUIC connection builder for DNS-over-QUIC
#[derive(Clone)]
pub struct QuicClientStreamBuilder {
    crypto_config: Option<rustls::ClientConfig>,
    transport_config: Arc<TransportConfig>,
    bind_addr: Option<SocketAddr>,
}

impl QuicClientStreamBuilder {
    /// Constructs a new TlsStreamBuilder with the associated ClientConfig
    pub fn crypto_config(mut self, crypto_config: rustls::ClientConfig) -> Self {
        self.crypto_config = Some(crypto_config);
        self
    }

    /// Sets the address to connect from.
    pub fn bind_addr(mut self, bind_addr: SocketAddr) -> Self {
        self.bind_addr = Some(bind_addr);
        self
    }

    /// Creates a new QuicStream to the specified name_server
    ///
    /// # Arguments
    ///
    /// * `name_server` - IP and Port for the remote DNS resolver
    /// * `server_name` - The DNS name associated with a certificate
    pub fn build(self, name_server: SocketAddr, server_name: Arc<str>) -> QuicClientConnect {
        QuicClientConnect(Box::pin(self.connect(name_server, server_name)) as _)
    }

    /// Create a QuicStream with existing connection
    pub fn build_with_future(
        self,
        socket: Arc<dyn quinn::AsyncUdpSocket>,
        name_server: SocketAddr,
        server_name: Arc<str>,
    ) -> QuicClientConnect {
        QuicClientConnect(Box::pin(self.connect_with_future(socket, name_server, server_name)) as _)
    }

    async fn connect_with_future(
        self,
        socket: Arc<dyn quinn::AsyncUdpSocket>,
        name_server: SocketAddr,
        server_name: Arc<str>,
    ) -> Result<QuicClientStream, ProtoError> {
        let endpoint_config = quic_config::endpoint();
        let endpoint = Endpoint::new_with_abstract_socket(
            endpoint_config,
            None,
            socket,
            Arc::new(quinn::TokioRuntime),
        )?;
        self.connect_inner(endpoint, name_server, server_name).await
    }

    async fn connect(
        self,
        name_server: SocketAddr,
        server_name: Arc<str>,
    ) -> Result<QuicClientStream, ProtoError> {
        let connect = if let Some(bind_addr) = self.bind_addr {
            <tokio::net::UdpSocket as UdpSocket>::connect_with_bind(name_server, bind_addr)
        } else {
            <tokio::net::UdpSocket as UdpSocket>::connect(name_server)
        };

        let socket = connect.await?;
        let socket = socket.into_std()?;
        let endpoint_config = quic_config::endpoint();
        let endpoint = Endpoint::new(endpoint_config, None, socket, Arc::new(quinn::TokioRuntime))?;
        self.connect_inner(endpoint, name_server, server_name).await
    }

    async fn connect_inner(
        self,
        endpoint: Endpoint,
        name_server: SocketAddr,
        server_name: Arc<str>,
    ) -> Result<QuicClientStream, ProtoError> {
        // ensure the ALPN protocol is set correctly
        let crypto_config = if let Some(crypto_config) = self.crypto_config {
            crypto_config
        } else {
            client_config()?
        };

        let quic_connection = connect_quic(
            name_server,
            server_name.clone(),
            quic_stream::DOQ_ALPN,
            crypto_config,
            self.transport_config,
            endpoint,
        )
        .await?;

        Ok(QuicClientStream {
            quic_connection,
            server_name,
            name_server,
            is_shutdown: false,
        })
    }
}

pub(crate) async fn connect_quic(
    addr: SocketAddr,
    server_name: Arc<str>,
    protocol: &[u8],
    mut crypto_config: rustls::ClientConfig,
    transport_config: Arc<TransportConfig>,
    mut endpoint: Endpoint,
) -> Result<Connection, ProtoError> {
    if crypto_config.alpn_protocols.is_empty() {
        crypto_config.alpn_protocols = vec![protocol.to_vec()];
    }
    let early_data_enabled = crypto_config.enable_early_data;

    let mut client_config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(crypto_config)?));
    client_config.transport_config(transport_config.clone());

    endpoint.set_default_client_config(client_config);

    let connecting = endpoint.connect(addr, &server_name)?;
    // TODO: for Client/Dynamic update, don't use RTT, for queries, do use it.

    Ok(if early_data_enabled {
        match connecting.into_0rtt() {
            Ok((new_connection, _)) => new_connection,
            Err(connecting) => connect_with_timeout(connecting).await?,
        }
    } else {
        connect_with_timeout(connecting).await?
    })
}

async fn connect_with_timeout(connecting: quinn::Connecting) -> Result<Connection, io::Error> {
    match timeout(CONNECT_TIMEOUT, connecting).await {
        Ok(Ok(connection)) => Ok(connection),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("QUIC handshake timed out after {CONNECT_TIMEOUT:?}",),
        )),
    }
}

impl Default for QuicClientStreamBuilder {
    fn default() -> Self {
        let mut transport_config = quic_config::transport();
        // clients never accept new bidirectional streams
        transport_config.max_concurrent_bidi_streams(VarInt::from_u32(0));

        Self {
            crypto_config: None,
            transport_config: Arc::new(transport_config),
            bind_addr: None,
        }
    }
}

/// A future that resolves to an QuicClientStream
pub struct QuicClientConnect(BoxFuture<'static, Result<QuicClientStream, ProtoError>>);

impl Future for QuicClientConnect {
    type Output = Result<QuicClientStream, ProtoError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.poll_unpin(cx)
    }
}

/// A future that resolves to
pub struct QuicClientResponse(BoxFuture<'static, Result<DnsResponse, ProtoError>>);

impl Future for QuicClientResponse {
    type Output = Result<DnsResponse, ProtoError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.as_mut().poll(cx).map_err(ProtoError::from)
    }
}
