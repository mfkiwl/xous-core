#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

mod api;
use api::*;
use num_traits::*;
use com::api::{ComIntSources, Ipv4Conf, NET_MTU};

mod device;

use byteorder::{ByteOrder, NetworkEndian};
use xous::{send_message, Message, CID, SID, msg_scalar_unpack, msg_blocking_scalar_unpack};
use xous_ipc::Buffer;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::str::FromStr;

use smoltcp::phy::{Medium, Device};
use smoltcp::iface::{InterfaceBuilder, NeighborCache, Routes, Interface};
use smoltcp::socket::{IcmpEndpoint, IcmpPacketMetadata, IcmpSocket, IcmpSocketBuffer, SocketSet};
use smoltcp::wire::{
    EthernetAddress, Icmpv4Packet, Icmpv4Repr, IpAddress, IpCidr, Ipv4Address, Ipv4Cidr, IpEndpoint
};
use smoltcp::socket::{UdpPacketMetadata, UdpSocket, UdpSocketBuffer, SocketHandle};
use smoltcp::{
    time::{Duration, Instant},
};
use std::thread;
use std::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

macro_rules! send_icmp_ping {
    ( $repr_type:ident, $packet_type:ident, $ident:expr, $seq_no:expr,
      $echo_payload:expr, $socket:expr, $remote_addr:expr ) => {{
        let icmp_repr = $repr_type::EchoRequest {
            ident: $ident,
            seq_no: $seq_no,
            data: &$echo_payload,
        };

        let icmp_payload = $socket.send(icmp_repr.buffer_len(), $remote_addr).expect("couldn't send ping");

        let icmp_packet = $packet_type::new_unchecked(icmp_payload);
        (icmp_repr, icmp_packet)
    }};
}

macro_rules! get_icmp_pong {
    ( $repr_type:ident, $repr:expr, $payload:expr, $waiting_queue:expr, $remote_addr:expr,
      $timestamp:expr, $received:expr ) => {{
        if let $repr_type::EchoReply { seq_no, data, .. } = $repr {
            if let Some(_) = $waiting_queue.get(&seq_no) {
                let packet_timestamp_ms = NetworkEndian::read_i64(data);
                log::info!(
                    "{} bytes from {}: icmp_seq={}, time={}ms",
                    data.len(),
                    $remote_addr,
                    seq_no,
                    $timestamp.total_millis() - packet_timestamp_ms
                );
                $waiting_queue.remove(&seq_no);
                $received += 1;
            }
        }
    }};
}

fn set_ipv4_addr<DeviceT>(iface: &mut Interface<'_, DeviceT>, cidr: Ipv4Cidr)
where
    DeviceT: for<'d> Device<'d>,
{
    iface.update_ip_addrs(|addrs| {
        let dest = addrs.iter_mut().next().expect("trouble updating ipv4 addresses in routing table");
        *dest = IpCidr::Ipv4(cidr);
    });
}

struct SmoltcpTimer {
    ticktimer: ticktimer_server::Ticktimer,
}
impl SmoltcpTimer {
    pub fn new() -> Self {
        SmoltcpTimer {
            ticktimer: ticktimer_server::Ticktimer::new().unwrap(),
        }
    }
    pub fn now(&self) -> Instant {
        Instant::from_millis(self.ticktimer.elapsed_ms() as i64)
    }
}

#[derive(num_derive::FromPrimitive, num_derive::ToPrimitive, Debug)]
enum WaitOp {
    WaitMs,
    PollAt,
    Quit,
}

pub struct UdpState {
    handle: SocketHandle,
    cid: CID,
    sid: SID,
}

#[xous::xous_main]
fn xmain() -> ! {
    log_server::init_wait().unwrap();
    log::set_max_level(log::LevelFilter::Info);
    log::info!("my PID is {}", xous::process::id());

    let xns = xous_names::XousNames::new().unwrap();
    let net_sid = xns.register_name(api::SERVER_NAME_NET, None).expect("can't register server");
    let net_conn = xous::connect(net_sid).unwrap();
    log::trace!("registered with NS -- {:?}", net_sid);

    // hook the COM interrupt listener
    let mut llio = llio::Llio::new(&xns).unwrap();
    let net_cid = xous::connect(net_sid).unwrap();
    llio.hook_com_event_callback(Opcode::ComInterrupt.to_u32().unwrap(), net_cid).unwrap();
    llio.com_event_enable(true).unwrap();
    // setup the interrupt masks
    let com = com::Com::new(&xns).unwrap();
    let mut com_int_list: Vec::<ComIntSources> = vec![];
    com.ints_get_active(&mut com_int_list);
    log::info!("COM initial pending interrupts: {:?}", com_int_list);
    com_int_list.clear();
    com_int_list.push(ComIntSources::WlanIpConfigUpdate);
    com_int_list.push(ComIntSources::WlanRxReady);
    com_int_list.push(ComIntSources::BatteryCritical);
    com.ints_enable(&com_int_list);
    com_int_list.clear();
    com.ints_get_active(&mut com_int_list);
    log::info!("COM pending interrupts after enabling: {:?}", com_int_list);

    // build the waiting thread
    let wait_conn = Arc::new(AtomicU32::new(0));
    thread::spawn({
        let parent_conn = net_conn.clone();
        let wait_conn_clone = wait_conn.clone();
        move || {
            let wait_sid = xous::create_server().unwrap();
            wait_conn_clone.store(xous::connect(wait_sid).unwrap(), Ordering::SeqCst);
            let tt = ticktimer_server::Ticktimer::new().unwrap();
            loop {
                let msg = xous::receive_message(wait_sid).unwrap();
                match FromPrimitive::from_usize(msg.body.id()) {
                    Some(WaitOp::WaitMs) => msg_scalar_unpack!(msg, duration_lsb, duration_msb, _, _, {
                        if duration_msb != 0 {
                            log::error!("wait duration exceeds API bounds");
                        }
                        tt.sleep_ms(duration_lsb).unwrap();
                        send_message(parent_conn, Message::new_scalar(Opcode::NetPump.to_usize().unwrap(), 0, 0, 0, 0)).expect("couldn't pump the net loop");
                    }),
                    Some(WaitOp::PollAt) => msg_scalar_unpack!(msg, deadline_lsb, deadline_msb, _, _, {
                        let deadline: u64 = (deadline_lsb as u64) | ((deadline_msb as u64) << 32);
                        let now = tt.elapsed_ms();
                        if deadline > now {
                            log::info!("sleeping for {}", deadline - now);
                            tt.sleep_ms((deadline - now) as usize).unwrap();
                            send_message(parent_conn, Message::new_scalar(Opcode::NetPump.to_usize().unwrap(), 0, 0, 0, 0)).expect("couldn't pump the net loop");
                        }
                    }),
                    Some(WaitOp::Quit) => break,
                    None => log::error!("got unknown message: {:?}", msg)
                }
            }
            xous::destroy_server(wait_sid).unwrap();
        }
    });
    // wait until the waiting thread starts and has populated a reverse connection ID
    while wait_conn.load(Ordering::SeqCst) == 0 {
        xous::yield_slice();
    }

    let mut net_config: Option<Ipv4Conf> = None;

    // ping-specific storage
    let ping_remote_addr = IpAddress::from_str("10.0.245.1").expect("invalid address format");

    let icmp_rx_buffer = IcmpSocketBuffer::new(vec![IcmpPacketMetadata::EMPTY], vec![0; 256]);
    let icmp_tx_buffer = IcmpSocketBuffer::new(vec![IcmpPacketMetadata::EMPTY], vec![0; 256]);
    let icmp_socket = IcmpSocket::new(icmp_rx_buffer, icmp_tx_buffer);

    let mut sockets = SocketSet::new(vec![]);
    let icmp_handle = sockets.add(icmp_socket);
    let mut udp_handles = HashMap::<u16, UdpState>::new();
    // UDP requires multiple copies. The way it works is that Tx can come from anyone;
    // for Rx, copies of a CID,SID tuple are kept for every clone is kept in a HashMap. This
    // allows for the Rx data to be cc:'d to each clone, and identified by SID upon drop
    let mut udp_clones = HashMap::<u16, HashMap::<[u32; 4], CID>>::new(); // additional clones for UDP responders

    let mut send_at = Instant::from_millis(0);
    let mut seq_no = 0;
    let mut received = 0;
    let mut echo_payload = [0xffu8; 40];
    let mut waiting_queue = HashMap::new();
    let ident = 0x22b;

    let count = 10; // number of ping iters
    let interval = Duration::from_secs(1);
    let timeout = Duration::from_secs(10);

    // link storage
    let timer = SmoltcpTimer::new();
    let neighbor_cache = NeighborCache::new(BTreeMap::new());
    let ip_addrs = [IpCidr::new(Ipv4Address::UNSPECIFIED.into(), 0)];
    let routes = Routes::new(BTreeMap::new());

    let device = device::NetPhy::new(&xns);
    let device_caps = device.capabilities();
    let medium = device.capabilities().medium;
    let mut builder = InterfaceBuilder::new(device)
        .ip_addrs(ip_addrs)
        .routes(routes);
    if medium == Medium::Ethernet {
        builder = builder
            .ethernet_addr(EthernetAddress::from_bytes(&[0; 6]))
            .neighbor_cache(neighbor_cache);
    }
    let mut iface = builder.finalize();

    // DNS hooks - the DNS server can ask the Net crate to tickle it when IP configs change using these hooks
    // Currently, we assume there is only one DNS server in Xous. I suppose you could
    // upgrade the code to handle multiple DNS servers, but...why???
    // ... nevermind, someone will invent a $reason because there was never a shiny
    // new feature that a coder didn't love and *had* to have *right now*.
    let mut dns_ipv4_hook = XousScalarEndpoint::new();
    let mut dns_ipv6_hook = XousScalarEndpoint::new();
    let mut dns_allclear_hook = XousScalarEndpoint::new();

    log::trace!("ready to accept requests");
    // register a suspend/resume listener
    let sr_cid = xous::connect(net_sid).expect("couldn't create suspend callback connection");
    let mut susres = susres::Susres::new(&xns, api::Opcode::SuspendResume as u32, sr_cid).expect("couldn't create suspend/resume object");

    let mut cid_to_disconnect: Option<CID> = None;
    loop {
        let mut msg = xous::receive_message(net_sid).unwrap();
        if let Some(dc_cid) = cid_to_disconnect.take() { // disconnect previous loop iter's connection after d/c OK response was sent
            unsafe{
                match xous::disconnect(dc_cid) {
                   Ok(_) => {},
                   Err(xous::Error::ServerNotFound) => {
                       log::trace!("Disconnect returned the expected error code for a remote that has been destroyed.")
                   },
                   Err(e) => {
                       log::error!("Attempt to de-allocate CID to destroyed server met with error: {:?}", e);
                   },
                }
            }
        }
        match FromPrimitive::from_usize(msg.body.id()) {
            Some(Opcode::DnsHookAddIpv4) => {
                let mut buf = unsafe{Buffer::from_memory_message_mut(msg.body.memory_message_mut().unwrap())};
                let hook = buf.to_original::<XousPrivateServerScalarHook, _>().unwrap();
                if dns_ipv4_hook.is_set() {
                    buf.replace(NetMemResponse::AlreadyUsed).unwrap();
                } else {
                    dns_ipv4_hook.set(
                        xous::connect(SID::from_array(hook.one_time_sid)).unwrap(),
                        hook.op,
                        hook.args,
                    );
                    buf.replace(NetMemResponse::Ok).unwrap();
                }
            }
            Some(Opcode::DnsHookAddIpv6) => {
                let mut buf = unsafe{Buffer::from_memory_message_mut(msg.body.memory_message_mut().unwrap())};
                let hook = buf.to_original::<XousPrivateServerScalarHook, _>().unwrap();
                if dns_ipv6_hook.is_set() {
                    buf.replace(NetMemResponse::AlreadyUsed).unwrap();
                } else {
                    dns_ipv6_hook.set(
                        xous::connect(SID::from_array(hook.one_time_sid)).unwrap(),
                        hook.op,
                        hook.args,
                    );
                    buf.replace(NetMemResponse::Ok).unwrap();
                }
            }
            Some(Opcode::DnsHookAllClear) => {
                let mut buf = unsafe{Buffer::from_memory_message_mut(msg.body.memory_message_mut().unwrap())};
                let hook = buf.to_original::<XousPrivateServerScalarHook, _>().unwrap();
                if dns_allclear_hook.is_set() {
                    buf.replace(NetMemResponse::AlreadyUsed).unwrap();
                } else {
                    dns_allclear_hook.set(
                        xous::connect(SID::from_array(hook.one_time_sid)).unwrap(),
                        hook.op,
                        hook.args,
                    );
                    buf.replace(NetMemResponse::Ok).unwrap();
                }

            }
            Some(Opcode::DnsUnhookAll) => msg_blocking_scalar_unpack!(msg, _, _, _, _, {
                dns_ipv4_hook.clear();
                dns_ipv6_hook.clear();
                dns_allclear_hook.clear();
                xous::return_scalar(msg.sender, 1).expect("couldn't ack unhook");
            }),
            Some(Opcode::UdpBind) => {
                let mut buf = unsafe{Buffer::from_memory_message_mut(msg.body.memory_message_mut().unwrap())};
                let udpspec = buf.to_original::<NetUdpBind, _>().unwrap();

                let buflen = if let Some(maxlen) = udpspec.max_payload {
                    maxlen as usize
                } else {
                    NET_MTU as usize
                };
                if udp_handles.contains_key(&udpspec.port) {
                    // if we're already connected, just register the extra listener in the clones array
                    let sid = udpspec.cb_sid;
                    let cid = xous::connect(SID::from_array(sid)).unwrap();
                    if let Some(clone_map) = udp_clones.get_mut(&udpspec.port) {
                        // if a clone already exists, put the additional clone into the map
                        match clone_map.insert(sid, cid) {
                            Some(_) => {
                                log::error!("Something went wrong in a UDP clone operation -- same SID registered twice");
                                buf.replace(NetMemResponse::SocketInUse).unwrap()
                            }, // the same SID has double-registered, this is an error
                            None => buf.replace(NetMemResponse::Ok).unwrap()
                        }
                    } else {
                        // otherwise, create the clone mapping entry
                        let mut newmap = HashMap::new();
                        newmap.insert(sid, cid);
                        udp_clones.insert(
                            udpspec.port,
                            newmap
                        );
                    }
                    buf.replace(NetMemResponse::Ok).unwrap();
                } else {
                    let udp_rx_buffer = UdpSocketBuffer::new(vec![UdpPacketMetadata::EMPTY], vec![0; buflen]);
                    let udp_tx_buffer = UdpSocketBuffer::new(vec![UdpPacketMetadata::EMPTY], vec![0; buflen]);
                    let mut udp_socket = UdpSocket::new(udp_rx_buffer, udp_tx_buffer);
                    match udp_socket.bind(udpspec.port) {
                        Ok(_) => {
                            let sid = SID::from_array(udpspec.cb_sid);
                            let udpstate = UdpState {
                                handle: sockets.add(udp_socket),
                                cid: xous::connect(sid).unwrap(),
                                sid
                            };
                            udp_handles.insert(udpspec.port, udpstate);
                            buf.replace(NetMemResponse::Ok).unwrap();
                        }
                        Err(e) => {
                            log::error!("Udp couldn't bind to socket: {:?}", e);
                            buf.replace(NetMemResponse::Invalid).unwrap();
                        }
                    }
                }
            },
            Some(Opcode::UdpClose) => {
                let mut buf = unsafe{Buffer::from_memory_message_mut(msg.body.memory_message_mut().unwrap())};
                let udpspec = buf.to_original::<NetUdpBind, _>().unwrap();
                // need to find the SID that matches either in the clone array, or the primary binding.
                // first check the clone array, then fall back to the primary binding
                match udp_clones.get_mut(&udpspec.port) {
                    Some(clone_map) => {
                        match clone_map.remove(&udpspec.cb_sid) {
                            Some(cid) => {
                                cid_to_disconnect = Some(cid);
                                buf.replace(NetMemResponse::Ok).unwrap();
                                continue;
                            }
                            None => {}
                        }
                    }
                    None => {}
                }
                match udp_handles.remove(&udpspec.port) {
                    Some(udpstate) => {
                        if udpstate.sid == SID::from_array(udpspec.cb_sid) {
                            match udp_clones.get_mut(&udpspec.port) {
                                // if the clone map is nil, close the socket, we're done
                                None => {
                                    sockets.get::<UdpSocket>(udpstate.handle).close();
                                    buf.replace(NetMemResponse::Ok).unwrap();
                                }
                                // if the clone map has entries, promote an arbitrary map entry to the primary handle
                                Some(clone_map) => {
                                    if clone_map.len() == 0 {
                                        // removing SIDs doesn't remove the map, so it's possible to have an empty mapping. Get rid of it, and we're done.
                                        udp_clones.remove(&udpspec.port);
                                        sockets.get::<UdpSocket>(udpstate.handle).close();
                                        buf.replace(NetMemResponse::Ok).unwrap();
                                    } else {
                                        // take an arbitrary key, re-insert it into the handles map.
                                        let new_primary_sid = *clone_map.keys().next().unwrap(); // unwrap is appropriate because len already checked as not 0
                                        let udpstate = UdpState {
                                            handle: udpstate.handle,
                                            cid: *clone_map.get(&new_primary_sid).unwrap(),
                                            sid: SID::from_array(new_primary_sid),
                                        };
                                        udp_handles.insert(udpspec.port, udpstate);
                                        // now remove it from the clone map
                                        clone_map.remove(&new_primary_sid);
                                        // clean up the clone map if it's empty
                                        if clone_map.len() == 0 {
                                            udp_clones.remove(&udpspec.port);
                                        }
                                        buf.replace(NetMemResponse::Ok).unwrap();
                                    }
                                }
                            }
                        }
                    }
                    _ => {
                        buf.replace(NetMemResponse::Invalid).unwrap()
                    }
                }
            },
            Some(Opcode::UdpTx) => {
                use std::convert::TryInto;
                let mut buf = unsafe{Buffer::from_memory_message_mut(msg.body.memory_message_mut().unwrap())};
                let udp_tx = buf.to_original::<NetUdpTransmit, _>().unwrap();
                match udp_handles.get_mut(&udp_tx.local_port) {
                    Some(udpstate) => {
                        if let Some(dest_socket) = udp_tx.dest_socket {
                            let endpoint = IpEndpoint::new(
                                dest_socket.addr.try_into().unwrap(),
                                dest_socket.port
                            );
                            let mut socket = sockets.get::<UdpSocket>(udpstate.handle);
                            match socket.send_slice(&udp_tx.data[..udp_tx.len as usize], endpoint) {
                                Ok(_) => buf.replace(NetMemResponse::Sent(udp_tx.len)).unwrap(),
                                _ => buf.replace(NetMemResponse::LibraryError).unwrap(),
                            }
                        } else {
                            buf.replace(NetMemResponse::Invalid).unwrap()
                        }
                    }
                    _ => buf.replace(NetMemResponse::Invalid).unwrap()
                }
            },
            Some(Opcode::UdpSetTtl) => msg_scalar_unpack!(msg, ttl, port, _, _, {
                match udp_handles.get_mut(&(port as u16)) {
                    Some(udpstate) => {
                        let mut socket = sockets.get::<UdpSocket>(udpstate.handle);
                        let checked_ttl = if ttl > 255 || ttl == 0 {
                            64
                        } else {
                            ttl as u8
                        };
                        socket.set_hop_limit(Some(checked_ttl));
                    }
                    None => {
                        log::error!("Set TTL message received, but no port was bound! port {} ttl {}", port, ttl);
                    }
                }
            }),
            Some(Opcode::UdpGetTtl) => msg_blocking_scalar_unpack!(msg, port, _, _, _, {
                match udp_handles.get_mut(&(port as u16)) {
                    Some(udpstate) => {
                        let socket = sockets.get::<UdpSocket>(udpstate.handle);
                        let ttl = socket.hop_limit().unwrap_or(64); // 64 is the value used by smoltcp if hop limit isn't set
                        xous::return_scalar(msg.sender, ttl as usize).expect("couldn't return TTL");
                    }
                    None => {
                        log::error!("Set TTL message received, but no port was bound! port {}", port);
                        xous::return_scalar(msg.sender, usize::MAX).expect("couldn't return TTL");
                    }
                }
            }),

            Some(Opcode::ComInterrupt) => {
                com_int_list.clear();
                let maybe_rxlen = com.ints_get_active(&mut com_int_list);
                log::debug!("COM got interrupts: {:?}, {:?}", com_int_list, maybe_rxlen);
                for &pending in com_int_list.iter() {
                    if pending == ComIntSources::Invalid {
                        log::error!("COM interrupt vector had an error, ignoring event.");
                        continue;
                    }
                }
                for &pending in com_int_list.iter() {
                    match pending {
                        ComIntSources::BatteryCritical => {
                            log::warn!("Battery is critical! TODO: go into SHIP mode");
                        },
                        ComIntSources::WlanIpConfigUpdate => {
                            // right now the WLAN implementation only does IPV4. So IPV6 compatibility ends here.
                            // if IPV6 gets added to the EC/COM bus, ideally this is one of a couple spots in Xous that needs a tweak.
                            let config = com.wlan_get_config().expect("couldn't retrieve updated ipv4 config");
                            log::info!("Network config acquired: {:?}", config);
                            net_config = Some(config);
                            let mac = EthernetAddress::from_bytes(&config.mac);

                            // we need to clear the ARP cache in case we've migrated base stations (e.g. in a wireless network
                            // that is coverd by multiple AP), as the host AP's MAC address would have changed, and we wouldn't
                            // be able to route responses back. I can't seem to find a function in smoltcp 0.7.5 that allows us
                            // to neatly clear the ARP cache as the BTreeMap that underlies it is moved into the container and
                            // no "clear" API is exposed, so let's just rebuild the whole interface if we get a DHCP renewal.
                            let neighbor_cache = NeighborCache::new(BTreeMap::new());
                            let ip_addrs = [IpCidr::new(Ipv4Address::UNSPECIFIED.into(), 0)];
                            let routes = Routes::new(BTreeMap::new());
                            let device = device::NetPhy::new(&xns);
                            let medium = device.capabilities().medium;
                            let mut builder = InterfaceBuilder::new(device)
                                .ip_addrs(ip_addrs)
                                .routes(routes);
                            if medium == Medium::Ethernet {
                                builder = builder
                                    .ethernet_addr(mac)
                                    .neighbor_cache(neighbor_cache);
                            }
                            iface = builder.finalize();

                            let ip_addr =
                                Ipv4Cidr::new(Ipv4Address::new(
                                    config.addr[0],
                                    config.addr[1],
                                    config.addr[2],
                                    config.addr[3],
                                ), 24);
                            set_ipv4_addr(&mut iface, ip_addr);
                            let default_v4_gw = Ipv4Address::new(
                                config.gtwy[0],
                                config.gtwy[1],
                                config.gtwy[2],
                                config.gtwy[3],
                            );

                            // reset the default route, in case it has changed
                            iface.routes_mut().remove_default_ipv4_route();
                            match iface.routes_mut().add_default_ipv4_route(default_v4_gw) {
                                Ok(route) => log::info!("routing table updated successfully [{:?}]", route),
                                Err(e) => log::error!("routing table update error: {}", e),
                            }
                            dns_allclear_hook.notify();
                            dns_ipv4_hook.notify_custom_args([
                                Some(u32::from_be_bytes(config.dns1)),
                                None, None, None,
                            ]);
                            // the current implementation always returns 0.0.0.0 as the second dns,
                            // ignore this if that's what we've got; otherwise, pass it on.
                            if config.dns2 != [0, 0, 0, 0] {
                                dns_ipv4_hook.notify_custom_args([
                                    Some(u32::from_be_bytes(config.dns2)),
                                    None, None, None,
                                ]);
                            }
                        },
                        ComIntSources::WlanRxReady => {
                            if let Some(_config) = net_config {
                                if let Some(rxlen) = maybe_rxlen {
                                    match iface.device_mut().push_rx_avail(rxlen) {
                                        None => {} //log::info!("pushed {} bytes avail to iface", rxlen),
                                        Some(_) => log::warn!("Got more packets, but smoltcp didn't drain them in time"),
                                    }
                                    send_message(
                                        net_conn,
                                        Message::new_scalar(Opcode::NetPump.to_usize().unwrap(), 0, 0, 0, 0)
                                    ).expect("WlanRxReady couldn't pump the loop");
                                } else {
                                    log::error!("Got RxReady interrupt but no packet length specified!");
                                }
                            }
                        },
                        ComIntSources::WlanSsidScanDone => {
                            log::info!("got ssid scan done");
                        },
                        _ => {
                            log::error!("Invalid interrupt type received");
                        }
                    }
                }
                com.ints_ack(&com_int_list);
            }
            Some(Opcode::NetPump) => {
                let timestamp = timer.now();
                match iface.poll(&mut sockets, timestamp) {
                    Ok(_) => { }
                    Err(e) => {
                        log::debug!("poll error: {}", e);
                    }
                }

                {
                    for (port, udpstate) in udp_handles.iter() {
                        let handle = udpstate.handle;
                        let mut socket = sockets.get::<UdpSocket>(handle);
                        match socket.recv() {
                            Ok((data, endpoint)) => {
                                log::info!(
                                    "udp:{} recv data: {:x?} from {}",
                                    port,
                                    data,
                                    endpoint
                                );
                                // return the data/endpoint tuple to the caller
                                let mut response = NetUdpResponse {
                                    endpoint_ip_addr: NetIpAddr::from(endpoint.addr),
                                    len: data.len() as u16,
                                    endpoint_port: endpoint.port,
                                    data: [0; UDP_RESPONSE_MAX_LEN],
                                };
                                for (&src, dst) in data.iter().zip(response.data.iter_mut()) {
                                    *dst = src;
                                }
                                let buf = Buffer::into_buf(response).expect("couldn't convert UDP response to memory message");
                                buf.send(udpstate.cid, NetUdpCallback::RxData.to_u32().unwrap()).expect("couldn't send UDP response");
                                // now send copies to the cloned receiver array, if they exist
                                if let Some(clone_map) = udp_clones.get(port) {
                                    for &cids in clone_map.values() {
                                        let buf = Buffer::into_buf(response).expect("couldn't convert UDP response to memory message");
                                        buf.send(cids, NetUdpCallback::RxData.to_u32().unwrap()).expect("couldn't send UDP response");
                                    }
                                }
                            }
                            Err(_) => {
                                // do nothing
                            },
                        };
                    }
                }

                // this enclosure contains an ICMP implementation that needs to be deconstructed
                // at the moment it bakes in a pinger and ping responder -- we need to remove the pinger
                // and expose it to a generic interface. Once we figure out what that is.
                {
                    let timestamp = timer.now();
                    let mut socket = sockets.get::<IcmpSocket>(icmp_handle);
                    if !socket.is_open() {
                        log::info!("Binding smoltcp to icmp socket");
                        socket.bind(IcmpEndpoint::Ident(ident)).expect("couldn't bind to icmp socket");
                        send_at = timestamp;
                    }

                    if socket.can_send() && seq_no < count as u16 && send_at <= timestamp {
                        NetworkEndian::write_i64(&mut echo_payload, timestamp.total_millis());

                        let (icmp_repr, mut icmp_packet) = send_icmp_ping!(
                            Icmpv4Repr,
                            Icmpv4Packet,
                            ident,
                            seq_no,
                            echo_payload,
                            socket,
                            ping_remote_addr
                        );
                        icmp_repr.emit(&mut icmp_packet, &device_caps.checksum);
                        log::trace!("icmp pkt: {:?}", icmp_packet);

                        waiting_queue.insert(seq_no, timestamp);
                        seq_no += 1;
                        send_at += interval;
                    }

                    if socket.can_recv() {
                        let (payload, _) = socket.recv().expect("couldn't receive on socket despite asserting availability");
                        log::trace!("icmp payload: {:x?}", payload);

                        let icmp_packet = Icmpv4Packet::new_checked(&payload).expect("couldn't make icmp payload");
                        let icmp_repr =
                            Icmpv4Repr::parse(&icmp_packet, &device_caps.checksum).expect("error parsing icmp4 repr");
                        get_icmp_pong!(
                            Icmpv4Repr,
                            icmp_repr,
                            payload,
                            waiting_queue,
                            ping_remote_addr,
                            timestamp,
                            received
                        );
                    }

                    waiting_queue.retain(|seq, from| {
                        if timestamp - *from < timeout {
                            true
                        } else {
                            log::info!("From {} icmp_seq={} timeout", ping_remote_addr, seq);
                            false
                        }
                    });

                    if seq_no == count as u16 && waiting_queue.is_empty() {
                        log::info!("{} packets transmitted, {} received, {:.0}% packet loss",
                                seq_no,
                                received,
                                100.0 * (seq_no - received) as f64 / seq_no as f64
                        );
                        seq_no += 1; // extinguish the message after it has printed once
                    }
                }

                // establish our next check-up interval
                // it's unclear why the "ping" example has such a crazy complicated poll_at mechanism...
                let timestamp = timer.now();
                match iface.poll_at(&sockets, timestamp) {
                    Some(poll_at) if timestamp < poll_at => {
                        //log::info!("poll_at: {}", poll_at);
                        xous::try_send_message(wait_conn.load(Ordering::SeqCst),
                            Message::new_scalar(
                                WaitOp::PollAt.to_usize().unwrap(),
                                (poll_at.total_millis() as u64 & 0xFFFF_FFFF) as usize,
                                ((poll_at.total_millis() as u64 >> 32) & 0xFFF_FFFF) as usize,
                                0, 0)
                        ).ok();
                    }
                    Some(_) => {
                        //log::info!("didn't get a specific wait_at, polling immediately...");
                        xous::try_send_message(net_conn,
                            Message::new_scalar(
                                Opcode::NetPump.to_usize().unwrap(),
                                0, 0, 0, 0)
                        ).ok();
                    },
                    None => {
                        /*
                        let wait_time = (send_at - timestamp).millis();
                        log::info!("default wait: {}", wait_time);
                        send_message(wait_conn.load(Ordering::SeqCst),
                            Message::new_scalar(
                                WaitOp::WaitMs.to_usize().unwrap(),
                                (wait_time & 0xFFFF_FFFF) as usize,
                                ((wait_time >> 32) & 0xFFF_FFFF) as usize,
                                0, 0)
                        ).expect("couldn't issue wait message");
                        */
                    }
                }
            }
            Some(Opcode::SuspendResume) => xous::msg_scalar_unpack!(msg, token, _, _, _, {
                // handle an suspend/resume state stuff here. right now, it's a NOP
                susres.suspend_until_resume(token).expect("couldn't execute suspend/resume");
            }),
            Some(Opcode::Quit) => {
                log::warn!("quit received");
                break;
            }
            None => {
                log::error!("couldn't convert opcode: {:?}", msg);
            }
        }
    }
    // clean up our program
    log::trace!("main loop exit, destroying servers");
    xns.unregister_server(net_sid).unwrap();
    xous::destroy_server(net_sid).unwrap();
    log::trace!("quitting");
    xous::terminate_process(0)
}
