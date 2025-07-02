// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::anyhow;
use async_utils::hanging_get::client::HangingGetStream;
use fidl_fuchsia_bluetooth::PeerId;
use fidl_fuchsia_bluetooth_bredr::{
    ConnectParameters, L2capParameters, ProfileMarker, ProfileProxy,
};
use fidl_fuchsia_bluetooth_sys::{
    AccessMarker, AccessProxy, HostInfo, HostWatcherMarker, HostWatcherProxy, Peer,
    ProcedureTokenProxy,
};
use fuchsia_async::{LocalExecutor, TimeoutExt, Timer};
use fuchsia_bluetooth::types::Channel;
use fuchsia_component::client::connect_to_protocol;
use fuchsia_sync::Mutex;
use futures::channel::{mpsc, oneshot};
use futures::{StreamExt, TryFutureExt};
use std::ffi::{CStr, CString};
use std::thread;

// TODO(b/414848887): Pass more descriptive errors.
enum Request {
    ReadLocalAddress(oneshot::Sender<Result<[u8; 6], anyhow::Error>>),
    GetKnownPeers(oneshot::Sender<Result<Vec<Peer>, anyhow::Error>>),
    GetPeerId(CString, oneshot::Sender<Result<PeerId, anyhow::Error>>),
    Connect(PeerId, oneshot::Sender<Result<(), anyhow::Error>>),
    Forget(PeerId, oneshot::Sender<Result<(), anyhow::Error>>),
    ConnectL2cap(PeerId, u16, oneshot::Sender<Result<(), anyhow::Error>>),
    SetDiscovery(bool, oneshot::Sender<Result<(), anyhow::Error>>),
    SetDiscoverability(bool, oneshot::Sender<Result<(), anyhow::Error>>),
    Stop,
}

pub struct WorkThread {
    thread_handle: Mutex<Option<thread::JoinHandle<Result<(), anyhow::Error>>>>,
    sender: mpsc::UnboundedSender<Request>,
}

impl WorkThread {
    pub fn spawn() -> Self {
        let (sender, receiver) = mpsc::unbounded::<Request>();

        let thread_handle = thread::spawn(move || {
            LocalExecutor::new().run_singlethreaded(Self::handle_requests(receiver))?;
            Ok(())
        });

        Self { thread_handle: Mutex::new(Some(thread_handle)), sender }
    }

    async fn handle_requests(
        mut receiver: mpsc::UnboundedReceiver<Request>,
    ) -> Result<(), anyhow::Error> {
        let mut proxies = Proxies::connect()?;
        let mut host_cache: Vec<HostInfo> = Vec::new();
        let mut peer_cache: Vec<Peer> = Vec::new();
        #[allow(clippy::collection_is_never_read)]
        let mut _l2cap_channel: Channel;

        while let Some(request) = receiver.next().await {
            match request {
                Request::ReadLocalAddress(sender) => {
                    sender
                        .send(
                            proxies
                                .get_active_host(&mut host_cache)
                                .map_ok(|host| {
                                    host.addresses
                                        .clone()
                                        .unwrap()
                                        .first()
                                        .expect("Host has no address")
                                        .bytes
                                })
                                .await,
                        )
                        .unwrap();
                }
                Request::GetKnownPeers(sender) => {
                    if let Err(err) =
                        proxies.refresh_peer_cache(std::time::Duration::ZERO, &mut peer_cache).await
                    {
                        sender.send(Err(anyhow!("refresh_peer_cache() error: {err}"))).unwrap();
                        continue;
                    }
                    sender.send(Ok(peer_cache.clone())).unwrap();
                }
                Request::GetPeerId(address, result_sender) => {
                    if let Some(peer) = proxies
                        .get_peer(&address, std::time::Duration::from_secs(2), &mut peer_cache)
                        .await?
                    {
                        result_sender.send(Ok(peer.id.unwrap())).unwrap();
                        continue;
                    }
                    result_sender.send(Err(anyhow!("Peer not found"))).unwrap();
                }
                Request::Forget(peer_id, sender) => {
                    sender.send(proxies.forget(&peer_id).await).unwrap();
                }
                Request::Connect(peer_id, result_sender) => {
                    result_sender.send(proxies.connect_peer(&peer_id).await).unwrap();
                }
                Request::ConnectL2cap(peer_id, psm, result_sender) => {
                    match proxies.connect_l2cap(&peer_id, psm).await {
                        Ok(channel) => {
                            _l2cap_channel = channel;
                            result_sender.send(Ok(())).unwrap();
                        }
                        Err(err) => {
                            result_sender.send(Err(err)).unwrap();
                        }
                    }
                }
                Request::SetDiscovery(discovery, sender) => {
                    sender.send(proxies.set_discovery(discovery).await).unwrap();
                }
                Request::SetDiscoverability(discoverable, sender) => {
                    sender.send(proxies.set_discoverability(discoverable).await).unwrap();
                }
                Request::Stop => break,
            }
        }

        Ok(())
    }

    pub fn join(&self) -> Result<(), anyhow::Error> {
        self.sender.clone().unbounded_send(Request::Stop).unwrap();
        if let Err(err) =
            self.thread_handle.lock().take().unwrap().join().expect("Failed to join work thread")
        {
            return Err(anyhow!("Work thread exited with error: {err}"));
        }
        Ok(())
    }

    // Write address of active host into `addr_byte_buff`.
    pub async fn read_local_address(&self, addr_byte_buff: *mut u8) -> Result<(), anyhow::Error> {
        let addr_bytes_slice = unsafe { std::slice::from_raw_parts_mut(addr_byte_buff, 6) };
        let (sender, receiver) = oneshot::channel::<Result<[u8; 6], anyhow::Error>>();
        self.sender.clone().unbounded_send(Request::ReadLocalAddress(sender))?;
        addr_bytes_slice.clone_from_slice(&receiver.await??);
        Ok(())
    }

    // Get identifier of peer at `address`.
    pub async fn get_peer_id(&self, address: &CStr) -> Result<PeerId, anyhow::Error> {
        let (sender, receiver) = oneshot::channel::<Result<PeerId, anyhow::Error>>();
        self.sender.clone().unbounded_send(Request::GetPeerId(address.to_owned(), sender))?;
        receiver.await?
    }

    pub async fn get_known_peers(&self) -> Result<Vec<Peer>, anyhow::Error> {
        let (sender, receiver) = oneshot::channel::<Result<Vec<Peer>, anyhow::Error>>();
        self.sender.clone().unbounded_send(Request::GetKnownPeers(sender))?;
        receiver.await?
    }

    // Connect to peer with given identifier.
    pub async fn connect_peer(&self, peer_id: PeerId) -> Result<(), anyhow::Error> {
        let (sender, receiver) = oneshot::channel::<Result<(), anyhow::Error>>();
        self.sender.clone().unbounded_send(Request::Connect(peer_id, sender))?;
        receiver.await?
    }

    // Forget peer and delete all bonding information, if peer is found.
    pub async fn forget_peer(&self, peer_id: PeerId) -> Result<(), anyhow::Error> {
        let (sender, receiver) = oneshot::channel::<Result<(), anyhow::Error>>();
        self.sender.clone().unbounded_send(Request::Forget(peer_id, sender))?;
        receiver.await?
    }

    // Connect a basic L2CAP channel.
    pub async fn connect_l2cap_channel(
        &self,
        peer_id: PeerId,
        psm: u16,
    ) -> Result<(), anyhow::Error> {
        let (sender, receiver) = oneshot::channel::<Result<(), anyhow::Error>>();
        self.sender.clone().unbounded_send(Request::ConnectL2cap(peer_id, psm, sender))?;
        receiver.await?
    }

    // Set discovery state.
    pub async fn set_discovery(&self, discovery: bool) -> Result<(), anyhow::Error> {
        let (sender, receiver) = oneshot::channel::<Result<(), anyhow::Error>>();
        self.sender.clone().unbounded_send(Request::SetDiscovery(discovery, sender))?;
        receiver.await?
    }

    // Set discoverability state.
    pub async fn set_discoverability(&self, discoverable: bool) -> Result<(), anyhow::Error> {
        let (sender, receiver) = oneshot::channel::<Result<(), anyhow::Error>>();
        self.sender.clone().unbounded_send(Request::SetDiscoverability(discoverable, sender))?;
        receiver.await?
    }
}

struct Proxies {
    access_proxy: AccessProxy,
    profile_proxy: ProfileProxy,
    host_watcher_stream: HangingGetStream<HostWatcherProxy, Vec<HostInfo>>,
    peer_watcher_stream: HangingGetStream<AccessProxy, (Vec<Peer>, Vec<PeerId>)>,
    discovery_session: Mutex<Option<ProcedureTokenProxy>>,
    discoverability_session: Mutex<Option<ProcedureTokenProxy>>,
}

impl Proxies {
    fn connect() -> Result<Self, anyhow::Error> {
        let access_proxy = connect_to_protocol::<AccessMarker>()?;
        let profile_proxy = connect_to_protocol::<ProfileMarker>()?;
        let host_watcher_stream = HangingGetStream::new_with_fn_ptr(
            connect_to_protocol::<HostWatcherMarker>()?,
            HostWatcherProxy::watch,
        );
        let peer_watcher_stream =
            HangingGetStream::new_with_fn_ptr(access_proxy.clone(), AccessProxy::watch_peers);
        let discovery_session: Mutex<Option<ProcedureTokenProxy>> = Mutex::new(None);
        let discoverability_session: Mutex<Option<ProcedureTokenProxy>> = Mutex::new(None);

        Ok(Proxies {
            access_proxy,
            profile_proxy,
            host_watcher_stream,
            peer_watcher_stream,
            discovery_session,
            discoverability_session,
        })
    }

    async fn connect_peer(&self, peer_id: &PeerId) -> Result<(), anyhow::Error> {
        self.access_proxy
            .connect(peer_id)
            .await
            .map_err(|fidl_error| {
                anyhow!("fuchsia.bluetooth.sys.Access/Connect error: {fidl_error}")
            })
            .and_then(|connect_result| {
                connect_result.map_err(|sapphire_err| {
                    anyhow!("fuchsia.bluetooth.sys.Access/Connect error: {sapphire_err:?}")
                })
            })
    }

    async fn forget(&self, peer_id: &PeerId) -> Result<(), anyhow::Error> {
        match self.access_proxy.forget(peer_id).await {
            Err(fidl_error) => {
                Err(anyhow!("fuchsia.bluetooth.sys.Access/Forget error: {fidl_error}"))
            }
            Ok(Err(fidl_fuchsia_bluetooth_sys::Error::PeerNotFound)) => {
                println!("Asked to forget nonexistent peer.");
                Ok(())
            }
            Ok(Err(sapphire_err)) => {
                Err(anyhow!("fuchsia.bluetooth.sys.Access/Forget error: {sapphire_err:?}"))
            }
            Ok(Ok(_)) => Ok(()),
        }
    }

    async fn get_active_host<'a>(
        &mut self,
        host_cache: &'a mut Vec<HostInfo>,
    ) -> Result<&'a HostInfo, anyhow::Error> {
        if let Some(host_watcher_result) = self
            .host_watcher_stream
            .next()
            .on_timeout(std::time::Duration::from_millis(100), || None)
            .await
        {
            let Ok(new_host_list) = host_watcher_result else {
                return Err(anyhow!(
                    "fuchsia.bluetooth.sys.HostWatcher error: {}",
                    host_watcher_result.unwrap_err()
                ));
            };
            *host_cache = new_host_list
        }
        host_cache.first().ok_or_else(|| anyhow!("No hosts"))
    }

    async fn refresh_peer_cache(
        &mut self,
        timeout: std::time::Duration,
        peer_cache: &mut Vec<Peer>,
    ) -> Result<(), fidl::Error> {
        match self.peer_watcher_stream.next().on_timeout(timeout, || None).await {
            Some(Ok((updated, removed))) => {
                removed.iter().for_each(|removed_id| {
                    let _ = peer_cache.extract_if(.., |peer| peer.id.unwrap() == *removed_id);
                });
                updated.iter().for_each(|updated_peer| {
                    let _ = peer_cache
                        .extract_if(.., |peer| peer.id.unwrap() == updated_peer.id.unwrap());
                });
                peer_cache.extend(updated);
                Ok(())
            }
            Some(Err(err)) => Err(err),
            None => Ok(()),
        }
    }

    // `address` should encode a BD_ADDR as a string of bytes in little-endian order.
    // If `timeout` >= 1 second, a discovery session will be established.
    // Returns None if peer is not found before `timeout` elapses.
    async fn get_peer<'a>(
        &mut self,
        address: &CString,
        mut timeout: std::time::Duration,
        peer_cache: &'a mut Vec<Peer>,
    ) -> Result<Option<&'a Peer>, anyhow::Error> {
        let addr_matches =
            |peer: &Peer| peer.address.unwrap().bytes.iter().eq(address.to_bytes().iter().rev());
        // To satisfy borrow checker, must first check if peer exists before generating a reference
        // to the peer in the conditional scope. See "Problem case #3" in "non-lexical lifetimes"
        // rust-lang RFC.
        if peer_cache.iter().any(addr_matches) {
            return Ok(Some(peer_cache.iter().find(|peer: &&Peer| addr_matches(peer)).unwrap()));
        }

        let (_token, discovery_session_server) = fidl::endpoints::create_proxy();
        let second = std::time::Duration::from_secs(1);
        if timeout >= second {
            timeout -= second;
            if let Err(err) = self.access_proxy.start_discovery(discovery_session_server).await? {
                return Err(anyhow!("fuchsia.bluetooth.sys.Access/StartDiscovery error: {err:?}"));
            }
            // Allow discovery session to activate.
            Timer::new(second).await;
        }

        self.refresh_peer_cache(timeout, peer_cache).await?;
        if peer_cache.iter().any(addr_matches) {
            return Ok(Some(peer_cache.iter().find(|peer: &&Peer| addr_matches(peer)).unwrap()));
        }
        return Ok(None);
    }

    async fn connect_l2cap(&self, peer_id: &PeerId, psm: u16) -> Result<Channel, anyhow::Error> {
        match self
            .profile_proxy
            .connect(
                peer_id,
                &ConnectParameters::L2cap(L2capParameters { psm: Some(psm), ..Default::default() }),
            )
            .await
        {
            Ok(Ok(channel_res)) => Ok(channel_res
                .try_into()
                .map_err(|err| anyhow!("Couldn't convert FIDL to BT channel: {err:?}"))?),
            Ok(Err(sapphire_err)) => {
                Err(anyhow!("fuchsia.bluetooth.bredr.Profile/Connect error: {sapphire_err:?}"))
            }
            Err(fidl_err) => {
                Err(anyhow!("fuchsia.bluetooth.bredr.Profile/Connect error: {fidl_err}"))
            }
        }
    }

    async fn set_discovery(&mut self, discovery: bool) -> Result<(), anyhow::Error> {
        let mut discovery_session = self.discovery_session.lock();
        if !discovery {
            if discovery_session.take().is_none() {
                eprintln!("Asked to revoke nonexistent discovery session.");
            }
            return Ok(());
        }
        if discovery_session.is_some() {
            return Ok(());
        }
        let (token, discovery_session_server) = fidl::endpoints::create_proxy();
        if let Err(err) = self.access_proxy.start_discovery(discovery_session_server).await? {
            return Err(anyhow!("fuchsia.bluetooth.sys.Access/StartDiscovery error: {err:?}"));
        }
        *discovery_session = Some(token);
        // Allow discovery session to activate.
        Timer::new(std::time::Duration::from_secs(1)).await;
        Ok(())
    }

    async fn set_discoverability(&mut self, discoverable: bool) -> Result<(), anyhow::Error> {
        let mut discoverability_session = self.discoverability_session.lock();
        if !discoverable {
            if discoverability_session.take().is_none() {
                eprintln!("Asked to revoke nonexistent discoverability session.");
            }
            return Ok(());
        }
        if discoverability_session.is_some() {
            return Ok(());
        }
        let (token, discoverability_session_server) = fidl::endpoints::create_proxy();
        if let Err(err) =
            self.access_proxy.make_discoverable(discoverability_session_server).await?
        {
            return Err(anyhow!("fuchsia.bluetooth.sys.Access/MakeDiscoverable error: {err:?}"));
        }
        *discoverability_session = Some(token);
        Ok(())
    }
}
