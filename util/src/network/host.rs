// Copyright 2015, 2016 Ethcore (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

use std::net::{SocketAddr, SocketAddrV4};
use std::collections::{HashMap};
use std::hash::{Hasher};
use std::str::{FromStr};
use std::sync::*;
use std::ops::*;
use mio::*;
use mio::tcp::*;
use target_info::Target;
use hash::*;
use crypto::*;
use sha3::Hashable;
use rlp::*;
use network::handshake::Handshake;
use network::session::{Session, SessionData};
use error::*;
use io::*;
use network::{NetworkProtocolHandler, PROTOCOL_VERSION};
use network::node_table::*;
use network::stats::NetworkStats;
use network::error::DisconnectReason;
use igd::{PortMappingProtocol,search_gateway};
use network::discovery::{Discovery, TableUpdates, NodeEntry};

type Slab<T> = ::slab::Slab<T, usize>;

const _DEFAULT_PORT: u16 = 30304;
const MAX_CONNECTIONS: usize = 1024;
const MAINTENANCE_TIMEOUT: u64 = 1000;
const MAX_HANDSHAKES: usize = 100;

#[derive(Debug)]
/// Network service configuration
pub struct NetworkConfiguration {
	/// Directory path to store network configuration. None means nothing will be saved
	pub config_path: Option<String>,
	/// IP address to listen for incoming connections
	pub listen_address: SocketAddr,
	/// IP address to advertise
	pub public_address: SocketAddr,
	/// Enable NAT configuration
	pub nat_enabled: bool,
	/// Enable discovery
	pub discovery_enabled: bool,
	/// Pin to boot nodes only
	pub pin: bool,
	/// List of initial node addresses
	pub boot_nodes: Vec<String>,
	/// Use provided node key instead of default
	pub use_secret: Option<Secret>,
	/// Number of connected peers to maintain
	pub ideal_peers: u32,
}

impl NetworkConfiguration {
	/// Create a new instance of default settings.
	pub fn new() -> NetworkConfiguration {
		NetworkConfiguration {
			config_path: None,
			listen_address: SocketAddr::from_str("0.0.0.0:30304").unwrap(),
			public_address: SocketAddr::from_str("0.0.0.0:30304").unwrap(),
			nat_enabled: true,
			discovery_enabled: true,
			pin: false,
			boot_nodes: Vec::new(),
			use_secret: None,
			ideal_peers: 10,
		}
	}

	/// Create new default configuration with sepcified listen port.
	pub fn new_with_port(port: u16) -> NetworkConfiguration {
		let mut config = NetworkConfiguration::new();
		config.listen_address = SocketAddr::from_str(&format!("0.0.0.0:{}", port)).unwrap();
		config.public_address = SocketAddr::from_str(&format!("0.0.0.0:{}", port)).unwrap();
		config
	}

	/// Conduct NAT if needed.
	pub fn prepared(self) -> Self {
		let mut listen = self.listen_address;
		let mut public = self.public_address;

		if self.nat_enabled {
			info!("Enabling NAT...");
			match search_gateway() {
				Err(ref err) => info!("Error: {}", err),
				Ok(gateway) => {
					let int_addr = SocketAddrV4::from_str("127.0.0.1:30304").unwrap();
					match gateway.get_any_address(PortMappingProtocol::TCP, int_addr, 0, "Parity Node/TCP") {
						Err(ref err) => {
							info!("There was an error! {}", err);
						},
						Ok(ext_addr) => {
							info!("Local gateway: {}, External ip address: {}", gateway, ext_addr);
							public = SocketAddr::V4(ext_addr);
							listen = SocketAddr::V4(int_addr);
						},
					}
				},
			}
		}

		NetworkConfiguration {
			config_path: self.config_path,
			listen_address: listen,
			public_address: public,
			nat_enabled: false,
			discovery_enabled: self.discovery_enabled,
			pin: self.pin,
			boot_nodes: self.boot_nodes,
			use_secret: self.use_secret,
			ideal_peers: self.ideal_peers,
		}
	}
}

// Tokens
const TCP_ACCEPT: usize = MAX_CONNECTIONS + 1;
const IDLE: usize = MAX_CONNECTIONS + 2;
const DISCOVERY: usize = MAX_CONNECTIONS + 3;
const DISCOVERY_REFRESH: usize = MAX_CONNECTIONS + 4;
const DISCOVERY_ROUND: usize = MAX_CONNECTIONS + 5;
const FIRST_CONNECTION: usize = 0;
const LAST_CONNECTION: usize = FIRST_CONNECTION + MAX_CONNECTIONS - 1;

/// Protocol handler level packet id
pub type PacketId = u8;
/// Protocol / handler id
pub type ProtocolId = &'static str;

/// Messages used to communitate with the event loop from other threads.
#[derive(Clone)]
pub enum NetworkIoMessage<Message> where Message: Send + Sync + Clone {
	/// Register a new protocol handler.
	AddHandler {
		/// Handler shared instance.
		handler: Arc<NetworkProtocolHandler<Message> + Sync>,
		/// Protocol Id.
		protocol: ProtocolId,
		/// Supported protocol versions.
		versions: Vec<u8>,
	},
	/// Register a new protocol timer
	AddTimer {
		/// Protocol Id.
		protocol: ProtocolId,
		/// Timer token.
		token: TimerToken,
		/// Timer delay in milliseconds.
		delay: u64,
	},
	/// Disconnect a peer
	Disconnect(PeerId),
	/// User message
	User(Message),
}

/// Local (temporary) peer session ID.
pub type PeerId = usize;

#[derive(Debug, PartialEq, Eq)]
/// Protocol info
pub struct CapabilityInfo {
	pub protocol: ProtocolId,
	pub version: u8,
	/// Total number of packet IDs this protocol support.
	pub packet_count: u8,
}

impl Encodable for CapabilityInfo {
	fn rlp_append(&self, s: &mut RlpStream) {
		s.begin_list(2);
		s.append(&self.protocol);
		s.append(&self.version);
	}
}

/// IO access point. This is passed to all IO handlers and provides an interface to the IO subsystem.
pub struct NetworkContext<'s, Message> where Message: Send + Sync + Clone + 'static, 's {
	io: &'s IoContext<NetworkIoMessage<Message>>,
	protocol: ProtocolId,
	connections: Arc<RwLock<Slab<SharedConnectionEntry>>>,
	session: Option<StreamToken>,
}

impl<'s, Message> NetworkContext<'s, Message> where Message: Send + Sync + Clone + 'static, {
	/// Create a new network IO access point. Takes references to all the data that can be updated within the IO handler.
	fn new(io: &'s IoContext<NetworkIoMessage<Message>>, 
		protocol: ProtocolId, 
		session: Option<StreamToken>, connections: Arc<RwLock<Slab<SharedConnectionEntry>>>) -> NetworkContext<'s, Message> {
		NetworkContext {
			io: io,
			protocol: protocol,
			session: session,
			connections: connections,
		}
	}

	/// Send a packet over the network to another peer.
	pub fn send(&self, peer: PeerId, packet_id: PacketId, data: Vec<u8>) -> Result<(), UtilError> {
		if let Some(connection) = self.connections.read().unwrap().get(peer).cloned() {
			match *connection.lock().unwrap().deref_mut() {
				ConnectionEntry::Session(ref mut s) => {
					s.send_packet(self.protocol, packet_id as u8, &data).unwrap_or_else(|e| {
						warn!(target: "net", "Send error: {:?}", e);
					}); //TODO: don't copy vector data
					try!(self.io.update_registration(peer));
				},
				_ => warn!(target: "net", "Send: Peer is not connected yet")
			}
		} else  {
			trace!(target: "net", "Send: Peer no longer exist")
		}
		Ok(())
	}

	/// Respond to a current network message. Panics if no there is no packet in the context.
	pub fn respond(&self, packet_id: PacketId, data: Vec<u8>) -> Result<(), UtilError> {
		match self.session {
			Some(session) => self.send(session, packet_id, data),
			None => {
				panic!("Respond: Session does not exist")
			}
		}
	}

	/// Disable current protocol capability for given peer. If no capabilities left peer gets disconnected.
	pub fn disable_peer(&self, peer: PeerId) {
		//TODO: remove capability, disconnect if no capabilities left
		self.disconnect_peer(peer);
	}

	/// Disconnect peer. Reconnect can be attempted later.
	pub fn disconnect_peer(&self, peer: PeerId) {
		self.io.message(NetworkIoMessage::Disconnect(peer));
	}

	/// Register a new IO timer. 'IoHandler::timeout' will be called with the token.
	pub fn register_timer(&self, token: TimerToken, ms: u64) -> Result<(), UtilError> {
		self.io.message(NetworkIoMessage::AddTimer {
			token: token,
			delay: ms,
			protocol: self.protocol,
		});
		Ok(())
	}

	/// Returns peer identification string
	pub fn peer_info(&self, peer: PeerId) -> String {
		if let Some(connection) = self.connections.read().unwrap().get(peer).cloned() {
			if let ConnectionEntry::Session(ref s) = *connection.lock().unwrap().deref() {
				return s.info.client_version.clone()
			}
		}
		"unknown".to_owned()
	}
}

/// Shared host information
pub struct HostInfo {
	/// Our private and public keys.
	keys: KeyPair,
	/// Current network configuration
	config: NetworkConfiguration,
	/// Connection nonce.
	nonce: H256,
	/// RLPx protocol version
	pub protocol_version: u32,
	/// Client identifier
	pub client_version: String,
	/// TCP connection port.
	pub listen_port: u16,
	/// Registered capabilities (handlers)
	pub capabilities: Vec<CapabilityInfo>
}

impl HostInfo {
	/// Returns public key
	pub fn id(&self) -> &NodeId {
		self.keys.public()
	}

	/// Returns secret key
	pub fn secret(&self) -> &Secret {
		self.keys.secret()
	}

	/// Increments and returns connection nonce.
	pub fn next_nonce(&mut self) -> H256 {
		self.nonce = self.nonce.sha3();
		self.nonce.clone()
	}
}

enum ConnectionEntry {
	Handshake(Handshake),
	Session(Session)
}

type SharedConnectionEntry = Arc<Mutex<ConnectionEntry>>;

#[derive(Copy, Clone)]
struct ProtocolTimer {
	pub protocol: ProtocolId,
	pub token: TimerToken, // Handler level token
}

/// Root IO handler. Manages protocol handlers, IO timers and network connections.
pub struct Host<Message> where Message: Send + Sync + Clone {
	pub info: RwLock<HostInfo>,
	tcp_listener: Mutex<TcpListener>,
	connections: Arc<RwLock<Slab<SharedConnectionEntry>>>,
	discovery: Mutex<Discovery>,
	nodes: RwLock<NodeTable>,
	handlers: RwLock<HashMap<ProtocolId, Arc<NetworkProtocolHandler<Message>>>>,
	timers: RwLock<HashMap<TimerToken, ProtocolTimer>>,
	timer_counter: RwLock<usize>,
	stats: Arc<NetworkStats>,
}

impl<Message> Host<Message> where Message: Send + Sync + Clone {
	/// Create a new instance
	pub fn new(config: NetworkConfiguration) -> Host<Message> {
		let config = config.prepared();

		let addr = config.listen_address;
		// Setup the server socket
		let tcp_listener = TcpListener::bind(&addr).unwrap();
		let keys = if let Some(ref secret) = config.use_secret { KeyPair::from_secret(secret.clone()).unwrap() } else { KeyPair::create().unwrap() };
		let endpoint = NodeEndpoint { address: addr.clone(), udp_port: addr.port() };
		let discovery = Discovery::new(&keys, endpoint, DISCOVERY);
		let path = config.config_path.clone();
		let mut host = Host::<Message> {
			info: RwLock::new(HostInfo {
				keys: keys,
				config: config,
				nonce: H256::random(),
				protocol_version: PROTOCOL_VERSION,
				client_version: format!("Parity/{}/{}-{}-{}", env!("CARGO_PKG_VERSION"), Target::arch(), Target::env(), Target::os()),
				listen_port: 0,
				capabilities: Vec::new(),
			}),
			discovery: Mutex::new(discovery),
			tcp_listener: Mutex::new(tcp_listener),
			connections: Arc::new(RwLock::new(Slab::new_starting_at(FIRST_CONNECTION, MAX_CONNECTIONS))),
			nodes: RwLock::new(NodeTable::new(path)),
			handlers: RwLock::new(HashMap::new()),
			timers: RwLock::new(HashMap::new()),
			timer_counter: RwLock::new(LAST_CONNECTION + 1),
			stats: Arc::new(NetworkStats::default()),
		};
		let port = host.info.read().unwrap().config.listen_address.port();
		host.info.write().unwrap().deref_mut().listen_port = port;

		let boot_nodes = host.info.read().unwrap().config.boot_nodes.clone();
		for n in boot_nodes {
			host.add_node(&n);
		}
		host
	}

	pub fn stats(&self) -> Arc<NetworkStats> {
		self.stats.clone()
	}

	pub fn add_node(&mut self, id: &str) {
		match Node::from_str(id) {
			Err(e) => { warn!("Could not add node: {:?}", e); },
			Ok(n) => {
				let entry = NodeEntry { endpoint: n.endpoint.clone(), id: n.id.clone() };
				self.nodes.write().unwrap().add_node(n);
				self.discovery.lock().unwrap().add_node(entry);
			}
		}
	}

	pub fn client_version(&self) -> String {
		self.info.read().unwrap().client_version.clone()
	}

	pub fn client_id(&self) -> NodeId {
		self.info.read().unwrap().id().clone()
	}

	fn maintain_network(&self, io: &IoContext<NetworkIoMessage<Message>>) {
		self.keep_alive(io);
		self.connect_peers(io);
	}

	fn have_session(&self, id: &NodeId) -> bool {
		self.connections.read().unwrap().iter().any(|e| 
			match *e.lock().unwrap().deref() { ConnectionEntry::Session(ref s) => s.info.id.eq(&id), _ => false  })
	}

	fn session_count(&self) -> usize {
		self.connections.read().unwrap().iter().filter(|e| 
			match *e.lock().unwrap().deref() { ConnectionEntry::Session(_) => true, _ => false  }).count()
	}

	fn connecting_to(&self, id: &NodeId) -> bool {
		self.connections.read().unwrap().iter().any(|e| 
			match *e.lock().unwrap().deref() { ConnectionEntry::Handshake(ref h) => h.id.eq(&id), _ => false  })
	}

	fn handshake_count(&self) -> usize {
		self.connections.read().unwrap().iter().filter(|e| 
			match *e.lock().unwrap().deref() { ConnectionEntry::Handshake(_) => true, _ => false  }).count()
	}

	fn keep_alive(&self, io: &IoContext<NetworkIoMessage<Message>>) {
		let mut to_kill = Vec::new();
		for e in self.connections.write().unwrap().iter_mut() {
			if let ConnectionEntry::Session(ref mut s) = *e.lock().unwrap().deref_mut() {
				if !s.keep_alive(io) {
					s.disconnect(DisconnectReason::PingTimeout);
					to_kill.push(s.token());
				}
			}
		}
		for p in to_kill {
			self.kill_connection(p, io, true);
		}
	}

	fn connect_peers(&self, io: &IoContext<NetworkIoMessage<Message>>) {
		let ideal_peers = { self.info.read().unwrap().deref().config.ideal_peers };
		let connections = self.session_count();
		if connections >= ideal_peers as usize {
			return;
		}

		let handshake_count = self.handshake_count();
		if handshake_count >= MAX_HANDSHAKES {
			return;
		}


		let nodes = { self.nodes.read().unwrap().nodes() };

		for id in nodes.iter().filter(|ref id| !self.have_session(id) && !self.connecting_to(id)).take(MAX_HANDSHAKES - handshake_count) {
			self.connect_peer(&id, io);
		}
		debug!(target: "net", "Connecting peers: {} sessions, {} pending", self.session_count(), self.handshake_count());
	}

	#[allow(single_match)]
	fn connect_peer(&self, id: &NodeId, io: &IoContext<NetworkIoMessage<Message>>) {
		if self.have_session(id)
		{
			trace!("Aborted connect. Node already connected.");
			return;
		}
		if self.connecting_to(id) {
			trace!("Aborted connect. Node already connecting.");
			return;
		}

		let socket = {
			let address = {
				let mut nodes = self.nodes.write().unwrap();
				let node = nodes.get_mut(id).unwrap();
				node.last_attempted = Some(::time::now());
				node.endpoint.address
			};
			match TcpStream::connect(&address) {
				Ok(socket) => socket,
				Err(_) => {
					warn!("Cannot connect to node");
					return;
				}
			}
		};
		self.create_connection(socket, Some(id), io);
	}

	#[allow(block_in_if_condition_stmt)]
	fn create_connection(&self, socket: TcpStream, id: Option<&NodeId>, io: &IoContext<NetworkIoMessage<Message>>) {
		let nonce = self.info.write().unwrap().next_nonce();
		let mut connections = self.connections.write().unwrap();
		if connections.insert_with(|token| {
			let mut handshake = Handshake::new(token, id, socket, &nonce, self.stats.clone()).expect("Can't create handshake");
			handshake.start(io, &self.info.read().unwrap(), id.is_some()).and_then(|_| io.register_stream(token)).unwrap_or_else (|e| {
				debug!(target: "net", "Handshake create error: {:?}", e);
			});
			Arc::new(Mutex::new(ConnectionEntry::Handshake(handshake)))
		}).is_none() {
			warn!("Max connections reached");
		}
	}

	fn accept(&self, io: &IoContext<NetworkIoMessage<Message>>) {
		trace!(target: "net", "accept");
		loop {
			let socket = match self.tcp_listener.lock().unwrap().accept() {
				Ok(None) => break,
				Ok(Some((sock, _addr))) => sock,
				Err(e) => {
					warn!("Error accepting connection: {:?}", e);
					break
				},
			};
			self.create_connection(socket, None, io);
		}
		io.update_registration(TCP_ACCEPT).expect("Error registering TCP listener");
	}

	#[allow(single_match)]
	fn connection_writable(&self, token: StreamToken, io: &IoContext<NetworkIoMessage<Message>>) {
		let mut create_session = false;
		let mut kill = false;
		if let Some(connection) = self.connections.read().unwrap().get(token).cloned() {
			match *connection.lock().unwrap().deref_mut() {
				ConnectionEntry::Handshake(ref mut h) => {
					match h.writable(io, &self.info.read().unwrap()) {
						Err(e) => {
							debug!(target: "net", "Handshake write error: {}:{:?}", token, e);
							kill = true;
						},
						Ok(_) => ()
					}
					if h.done() {
						create_session = true;
					}
				},
				ConnectionEntry::Session(ref mut s) => {
					match s.writable(io, &self.info.read().unwrap()) {
						Err(e) => {
							debug!(target: "net", "Session write error: {}:{:?}", token, e);
							kill = true;
						},
						Ok(_) => ()
					}
					io.update_registration(token).unwrap_or_else(|e| debug!(target: "net", "Session registration error: {:?}", e));
				}
			}
		} 
		if kill {
			self.kill_connection(token, io, true); //TODO: mark connection as dead an check in kill_connection
			return;
		} else if create_session {
			self.start_session(token, io);
			io.update_registration(token).unwrap_or_else(|e| debug!(target: "net", "Session registration error: {:?}", e));
		}
	}

	fn connection_closed(&self, token: TimerToken, io: &IoContext<NetworkIoMessage<Message>>) {
		self.kill_connection(token, io, true);
	}

	fn connection_readable(&self, token: StreamToken, io: &IoContext<NetworkIoMessage<Message>>) {
		let mut ready_data: Vec<ProtocolId> = Vec::new();
		let mut packet_data: Option<(ProtocolId, PacketId, Vec<u8>)> = None;
		let mut create_session = false;
		let mut kill = false;
		if let Some(connection) = self.connections.read().unwrap().get(token).cloned() {
			match *connection.lock().unwrap().deref_mut() {
				ConnectionEntry::Handshake(ref mut h) => {
					if let Err(e) = h.readable(io, &self.info.read().unwrap()) {
						debug!(target: "net", "Handshake read error: {}:{:?}", token, e);
						kill = true;
					}
					if h.done() {
						create_session = true;
					}
				},
				ConnectionEntry::Session(ref mut s) => {
					match s.readable(io, &self.info.read().unwrap()) {
						Err(e) => {
							debug!(target: "net", "Handshake read error: {}:{:?}", token, e);
							kill = true;
						},
						Ok(SessionData::Ready) => {
							for (p, _) in self.handlers.read().unwrap().iter() {
								if s.have_capability(p)  {
									ready_data.push(p);
								}
							}
						},
						Ok(SessionData::Packet {
							data,
							protocol,
							packet_id,
						}) => {
							match self.handlers.read().unwrap().get(protocol) {
								None => { warn!(target: "net", "No handler found for protocol: {:?}", protocol) },
								Some(_) => packet_data = Some((protocol, packet_id, data)),
							}
						},
						Ok(SessionData::None) => {},
					}
				}
			}
		} 
		if kill {
			self.kill_connection(token, io, true); //TODO: mark connection as dead an check in kill_connection
			return;
		} else if create_session {
			self.start_session(token, io);
			io.update_registration(token).unwrap_or_else(|e| debug!(target: "net", "Session registration error: {:?}", e));
		}
		for p in ready_data {
			let h = self.handlers.read().unwrap().get(p).unwrap().clone();
			h.connected(&NetworkContext::new(io, p, Some(token), self.connections.clone()), &token);
		}
		if let Some((p, packet_id, data)) = packet_data {
			let h = self.handlers.read().unwrap().get(p).unwrap().clone();
			h.read(&NetworkContext::new(io, p, Some(token), self.connections.clone()), &token, packet_id, &data[1..]);
		}
		io.update_registration(token).unwrap_or_else(|e| debug!(target: "net", "Token registration error: {:?}", e));
	}

	fn start_session(&self, token: StreamToken, io: &IoContext<NetworkIoMessage<Message>>) {
		let mut connections = self.connections.write().unwrap();
		if connections.get(token).is_none() {
			return; // handshake expired
		}
		connections.replace_with(token, |c| {
			match Arc::try_unwrap(c).ok().unwrap().into_inner().unwrap() {
				ConnectionEntry::Handshake(h) => {
					let session = Session::new(h, io, &self.info.read().unwrap()).expect("Session creation error");
					io.update_registration(token).expect("Error updating session registration");
					self.stats.inc_sessions();
					Some(Arc::new(Mutex::new(ConnectionEntry::Session(session))))
				},
				_ => { None } // handshake expired
			}
		}).ok();
	}

	fn connection_timeout(&self, token: StreamToken, io: &IoContext<NetworkIoMessage<Message>>) {
		self.kill_connection(token, io, true)
	}

	fn kill_connection(&self, token: StreamToken, io: &IoContext<NetworkIoMessage<Message>>, remote: bool) {
		let mut to_disconnect: Vec<ProtocolId> = Vec::new();
		{
			let mut connections = self.connections.write().unwrap();
			if let Some(connection) = connections.get(token).cloned() {
				match *connection.lock().unwrap().deref_mut() {
					ConnectionEntry::Handshake(ref h) => {
						connections.remove(token);
						if remote {
							self.nodes.write().unwrap().note_failure(h.id());
						}
					},
					ConnectionEntry::Session(ref mut s) if s.is_ready() => {
						for (p, _) in self.handlers.read().unwrap().iter() {
							if s.have_capability(p)  {
								to_disconnect.push(p);
							}
						}
						connections.remove(token);
						if remote {
							self.nodes.write().unwrap().note_failure(s.id());
						}
					},
					_ => {},
				}
			}
			io.deregister_stream(token).expect("Error deregistering stream");
		}
		for p in to_disconnect {
			let h = self.handlers.read().unwrap().get(p).unwrap().clone();
			h.disconnected(&NetworkContext::new(io, p, Some(token), self.connections.clone()), &token);
		}
	}

	fn update_nodes(&self, io: &IoContext<NetworkIoMessage<Message>>, node_changes: TableUpdates) {
		let connections = self.connections.write().unwrap();
		let mut to_remove: Vec<PeerId> = Vec::new();
		for c in connections.iter() {
			match *c.lock().unwrap().deref_mut() {
				ConnectionEntry::Handshake(ref h) => {
					if node_changes.removed.contains(&h.id()) {
						to_remove.push(h.token());
					}
				}
				ConnectionEntry::Session(ref s) => {
					if node_changes.removed.contains(&s.id()) {
						to_remove.push(s.token());
					}
				}
			}
		}
		for i in to_remove {
			self.kill_connection(i, io, false);
		}
		self.nodes.write().unwrap().update(node_changes);
	}
}

impl<Message> IoHandler<NetworkIoMessage<Message>> for Host<Message> where Message: Send + Sync + Clone + 'static {
	/// Initialize networking
	fn initialize(&self, io: &IoContext<NetworkIoMessage<Message>>) {
		io.register_stream(TCP_ACCEPT).expect("Error registering TCP listener");
		io.register_stream(DISCOVERY).expect("Error registering UDP listener");
		io.register_timer(IDLE, MAINTENANCE_TIMEOUT).expect("Error registering Network idle timer");
		io.register_timer(DISCOVERY_REFRESH, 7200).expect("Error registering discovery timer");
		io.register_timer(DISCOVERY_ROUND, 300).expect("Error registering discovery timer");
	}

	fn stream_hup(&self, io: &IoContext<NetworkIoMessage<Message>>, stream: StreamToken) {
		trace!(target: "net", "Hup: {}", stream);
		match stream {
			FIRST_CONNECTION ... LAST_CONNECTION => self.connection_closed(stream, io),
			_ => warn!(target: "net", "Unexpected hup"),
		};
	}

	fn stream_readable(&self, io: &IoContext<NetworkIoMessage<Message>>, stream: StreamToken) {
		match stream {
			FIRST_CONNECTION ... LAST_CONNECTION => self.connection_readable(stream, io),
			DISCOVERY => {
				if let Some(node_changes) = self.discovery.lock().unwrap().readable() {
					self.update_nodes(io, node_changes);
				}
				io.update_registration(DISCOVERY).expect("Error updating disicovery registration");
			},
			TCP_ACCEPT => self.accept(io), 
			_ => panic!("Received unknown readable token"),
		}
	}

	fn stream_writable(&self, io: &IoContext<NetworkIoMessage<Message>>, stream: StreamToken) {
		match stream {
			FIRST_CONNECTION ... LAST_CONNECTION => self.connection_writable(stream, io),
			DISCOVERY => {
				self.discovery.lock().unwrap().writable();
				io.update_registration(DISCOVERY).expect("Error updating disicovery registration");
			}
			_ => panic!("Received unknown writable token"),
		}
	}

	fn timeout(&self, io: &IoContext<NetworkIoMessage<Message>>, token: TimerToken) {
		match token {
			IDLE => self.maintain_network(io),
			FIRST_CONNECTION ... LAST_CONNECTION => self.connection_timeout(token, io),
			DISCOVERY_REFRESH => {
				self.discovery.lock().unwrap().refresh();
				io.update_registration(DISCOVERY).expect("Error updating disicovery registration");
			},
			DISCOVERY_ROUND => {
				if let Some(node_changes) = self.discovery.lock().unwrap().round() {
					self.update_nodes(io, node_changes);
				}
				io.update_registration(DISCOVERY).expect("Error updating disicovery registration");
			},
			_ => match self.timers.read().unwrap().get(&token).cloned() {
				Some(timer) => match self.handlers.read().unwrap().get(timer.protocol).cloned() {
						None => { warn!(target: "net", "No handler found for protocol: {:?}", timer.protocol) },
						Some(h) => { h.timeout(&NetworkContext::new(io, timer.protocol, None, self.connections.clone()), timer.token); }
				},
				None => { warn!("Unknown timer token: {}", token); } // timer is not registerd through us
			}
		}
	}

	fn message(&self, io: &IoContext<NetworkIoMessage<Message>>, message: &NetworkIoMessage<Message>) {
		match *message {
			NetworkIoMessage::AddHandler {
				ref handler,
				ref protocol,
				ref versions
			} => {
				let h = handler.clone();
				h.initialize(&NetworkContext::new(io, protocol, None, self.connections.clone()));
				self.handlers.write().unwrap().insert(protocol, h);
				let mut info = self.info.write().unwrap();
				for v in versions {
					info.capabilities.push(CapabilityInfo { protocol: protocol, version: *v, packet_count:0 });
				}
			},
			NetworkIoMessage::AddTimer {
				ref protocol,
				ref delay,
				ref token,
			} => {
				let handler_token = {
					let mut timer_counter = self.timer_counter.write().unwrap();
					let counter = timer_counter.deref_mut();
					let handler_token = *counter;
					*counter += 1;
					handler_token
				};
				self.timers.write().unwrap().insert(handler_token, ProtocolTimer { protocol: protocol, token: *token });
				io.register_timer(handler_token, *delay).expect("Error registering timer");
			},
			NetworkIoMessage::Disconnect(ref peer) => {
				if let Some(connection) = self.connections.read().unwrap().get(*peer).cloned() {
					match *connection.lock().unwrap().deref_mut() {
						ConnectionEntry::Handshake(_) => {},
						ConnectionEntry::Session(ref mut s) => { s.disconnect(DisconnectReason::DisconnectRequested); } 
					}
				} 
				self.kill_connection(*peer, io, false);
			},
			NetworkIoMessage::User(ref message) => {
				for (p, h) in self.handlers.read().unwrap().iter() {
					h.message(&NetworkContext::new(io, p, None, self.connections.clone()), &message);
				}
			}
		}
	}

	fn register_stream(&self, stream: StreamToken, reg: Token, event_loop: &mut EventLoop<IoManager<NetworkIoMessage<Message>>>) {
		match stream {
			FIRST_CONNECTION ... LAST_CONNECTION => {
				if let Some(connection) = self.connections.read().unwrap().get(stream).cloned() {
					match *connection.lock().unwrap().deref() {
						ConnectionEntry::Handshake(ref h) => h.register_socket(reg, event_loop).expect("Error registering socket"),
						ConnectionEntry::Session(_) => warn!("Unexpected session stream registration")
					}
				} else {} // expired
			}
			DISCOVERY => self.discovery.lock().unwrap().register_socket(event_loop).expect("Error registering discovery socket"),
			TCP_ACCEPT => event_loop.register(self.tcp_listener.lock().unwrap().deref(), Token(TCP_ACCEPT), EventSet::all(), PollOpt::edge()).expect("Error registering stream"),
			_ => warn!("Unexpected stream registration")
		}
	}

	fn deregister_stream(&self, stream: StreamToken, event_loop: &mut EventLoop<IoManager<NetworkIoMessage<Message>>>) {
		match stream {
			FIRST_CONNECTION ... LAST_CONNECTION => {
				let mut connections = self.connections.write().unwrap();
				if let Some(connection) = connections.get(stream).cloned() {
					match *connection.lock().unwrap().deref() {
						ConnectionEntry::Handshake(ref h) => h.deregister_socket(event_loop).expect("Error deregistering socket"),
						ConnectionEntry::Session(ref s) => s.deregister_socket(event_loop).expect("Error deregistering session socket"),
					}
					connections.remove(stream);
				} 
			},
			DISCOVERY => (),
			TCP_ACCEPT => event_loop.deregister(self.tcp_listener.lock().unwrap().deref()).unwrap(),
			_ => warn!("Unexpected stream deregistration")
		}
	}

	fn update_stream(&self, stream: StreamToken, reg: Token, event_loop: &mut EventLoop<IoManager<NetworkIoMessage<Message>>>) {
		match stream {
			FIRST_CONNECTION ... LAST_CONNECTION => {
				if let Some(connection) = self.connections.read().unwrap().get(stream).cloned() {
					match *connection.lock().unwrap().deref() {
						ConnectionEntry::Handshake(ref h) => h.update_socket(reg, event_loop).expect("Error updating socket"),
						ConnectionEntry::Session(ref s) => s.update_socket(reg, event_loop).expect("Error updating socket"),
					}
				} else {} // expired
			}
			DISCOVERY => self.discovery.lock().unwrap().update_registration(event_loop).expect("Error reregistering discovery socket"),
			TCP_ACCEPT => event_loop.reregister(self.tcp_listener.lock().unwrap().deref(), Token(TCP_ACCEPT), EventSet::all(), PollOpt::edge()).expect("Error reregistering stream"),
			_ => warn!("Unexpected stream update")
		}
	}
}
