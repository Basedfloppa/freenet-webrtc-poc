//! Thin async wrapper around RTCPeerConnection — datachannel-only
//! variant.
//!
//! Why no media: `navigator.mediaDevices` is `undefined` inside
//! freenet's null-origin sandbox iframe (the spec gates MediaDevices
//! behind "secure context" and `null` origin doesn't qualify). So
//! camera/mic isn't reachable from a webapp-as-contract. RTCDataChannel
//! has no such restriction — `RTCPeerConnection` itself works fine in
//! the sandbox — so we stream cursor positions instead.
//!
//! Each `Peer` owns:
//!   - `RtcPeerConnection` (one STUN server),
//!   - one ordered datachannel "cursor" for low-latency text frames,
//!   - the closure handles backing JS-side callbacks.
//!
//! On caller side: createDataChannel before createOffer (so it ends up
//! in the SDP). On callee side: ondatachannel fires when remote
//! description lands. Both sides wire the same `on_remote_message`
//! callback so the app receives every inbound frame uniformly.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::{closure::Closure, JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    MessageEvent, RtcConfiguration, RtcDataChannel, RtcDataChannelEvent, RtcDataChannelInit,
    RtcDataChannelState, RtcIceCandidate, RtcIceCandidateInit, RtcIceServer, RtcPeerConnection,
    RtcPeerConnectionIceEvent, RtcSdpType, RtcSessionDescriptionInit,
};

#[derive(Debug, Clone)]
pub struct IceCandidate {
    pub candidate: String,
    pub sdp_mid: Option<String>,
    pub sdp_m_line_index: Option<u16>,
}

/// What the app receives on inbound datachannel messages.
pub type RemoteMsgHandler = Rc<dyn Fn(String)>;

/// Fired once when the datachannel transitions to `Open`. Useful for
/// triggering a replay of historical state to a freshly-connected peer
/// without polling the readyState.
pub type OpenHandler = Rc<dyn Fn()>;

pub struct Peer {
    pc: RtcPeerConnection,
    /// On caller, set immediately (channel we created).
    /// On callee, set when ondatachannel fires.
    dc: Rc<RefCell<Option<RtcDataChannel>>>,
    pending_remote_ice: Rc<RefCell<Vec<IceCandidate>>>,
    has_remote_desc: Rc<RefCell<bool>>,
    _handlers: Vec<Box<dyn std::any::Any>>,
}

impl Peer {
    /// Caller-side: build pc, create the local datachannel, wire ICE
    /// + ondatachannel (no-op for caller; the channel is the one we
    /// created). `on_remote_msg` is invoked once per inbound text frame.
    /// `on_local_ice` reports each gathered local candidate to the app
    /// for forwarding through the signaling outbox.
    /// `on_open` fires once when the datachannel becomes Open.
    pub fn new_caller<F>(
        on_remote_msg: RemoteMsgHandler,
        on_local_ice: F,
        on_open: OpenHandler,
    ) -> Result<Self, JsValue>
    where
        F: FnMut(Option<IceCandidate>) + 'static,
    {
        let pc = make_pc()?;
        let dc_init = RtcDataChannelInit::new();
        let raw_dc = pc.create_data_channel_with_data_channel_dict("draw", &dc_init);

        let dc_slot: Rc<RefCell<Option<RtcDataChannel>>> = Rc::new(RefCell::new(None));
        let mut handlers = wire_common_handlers(
            &pc,
            dc_slot.clone(),
            on_remote_msg.clone(),
            on_local_ice,
            on_open.clone(),
        );
        wire_dc_onmessage(&raw_dc, on_remote_msg.clone(), &mut handlers);
        wire_dc_onopen(&raw_dc, on_open, &mut handlers);
        *dc_slot.borrow_mut() = Some(raw_dc);

        Ok(Self {
            pc,
            dc: dc_slot,
            pending_remote_ice: Rc::new(RefCell::new(Vec::new())),
            has_remote_desc: Rc::new(RefCell::new(false)),
            _handlers: handlers,
        })
    }

    /// Callee-side: build pc, install ondatachannel handler that wires
    /// the remote-created channel when it arrives. Don't create a
    /// local channel — that would make two parallel channels.
    pub fn new_callee<F>(
        on_remote_msg: RemoteMsgHandler,
        on_local_ice: F,
        on_open: OpenHandler,
    ) -> Result<Self, JsValue>
    where
        F: FnMut(Option<IceCandidate>) + 'static,
    {
        let pc = make_pc()?;
        let dc_slot: Rc<RefCell<Option<RtcDataChannel>>> = Rc::new(RefCell::new(None));
        let handlers = wire_common_handlers(
            &pc,
            dc_slot.clone(),
            on_remote_msg.clone(),
            on_local_ice,
            on_open,
        );

        Ok(Self {
            pc,
            dc: dc_slot,
            pending_remote_ice: Rc::new(RefCell::new(Vec::new())),
            has_remote_desc: Rc::new(RefCell::new(false)),
            _handlers: handlers,
        })
    }

    /// Send a text frame down the datachannel. Drops the message
    /// silently if the channel isn't open yet (caller can re-send when
    /// connection state advances; for cursor frames, dropping is
    /// always preferable to queueing — stale positions are useless).
    pub fn send(&self, msg: &str) -> Result<(), JsValue> {
        let guard = self.dc.borrow();
        let Some(dc) = guard.as_ref() else { return Ok(()); };
        if dc.ready_state() != RtcDataChannelState::Open {
            return Ok(());
        }
        dc.send_with_str(msg)
    }

    pub async fn create_offer(&self) -> Result<String, JsValue> {
        let offer = JsFuture::from(self.pc.create_offer()).await?;
        let sdp = js_sys::Reflect::get(&offer, &JsValue::from_str("sdp"))?
            .as_string()
            .ok_or_else(|| JsValue::from_str("offer missing sdp"))?;
        let init = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        init.set_sdp(&sdp);
        JsFuture::from(self.pc.set_local_description(&init)).await?;
        Ok(sdp)
    }

    pub async fn accept_offer(&self, offer_sdp: &str) -> Result<String, JsValue> {
        let remote = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        remote.set_sdp(offer_sdp);
        JsFuture::from(self.pc.set_remote_description(&remote)).await?;
        *self.has_remote_desc.borrow_mut() = true;
        self.flush_pending_ice().await;

        let answer = JsFuture::from(self.pc.create_answer()).await?;
        let sdp = js_sys::Reflect::get(&answer, &JsValue::from_str("sdp"))?
            .as_string()
            .ok_or_else(|| JsValue::from_str("answer missing sdp"))?;
        let local = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        local.set_sdp(&sdp);
        JsFuture::from(self.pc.set_local_description(&local)).await?;
        Ok(sdp)
    }

    pub async fn accept_answer(&self, answer_sdp: &str) -> Result<(), JsValue> {
        let remote = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        remote.set_sdp(answer_sdp);
        JsFuture::from(self.pc.set_remote_description(&remote)).await?;
        *self.has_remote_desc.borrow_mut() = true;
        self.flush_pending_ice().await;
        Ok(())
    }

    pub async fn add_remote_ice(&self, c: IceCandidate) -> Result<(), JsValue> {
        if !*self.has_remote_desc.borrow() {
            self.pending_remote_ice.borrow_mut().push(c);
            return Ok(());
        }
        self.add_ice_now(c).await
    }

    async fn flush_pending_ice(&self) {
        let drained: Vec<_> = self.pending_remote_ice.borrow_mut().drain(..).collect();
        for c in drained {
            if let Err(e) = self.add_ice_now(c).await {
                web_sys::console::warn_1(&format!("addIceCandidate failed: {e:?}").into());
            }
        }
    }

    async fn add_ice_now(&self, c: IceCandidate) -> Result<(), JsValue> {
        if c.candidate.is_empty() {
            JsFuture::from(self.pc.add_ice_candidate_with_opt_rtc_ice_candidate(None)).await?;
            return Ok(());
        }
        let init = RtcIceCandidateInit::new(&c.candidate);
        if let Some(m) = &c.sdp_mid {
            init.set_sdp_mid(Some(m));
        }
        if let Some(idx) = c.sdp_m_line_index {
            init.set_sdp_m_line_index(Some(idx));
        }
        let cand = RtcIceCandidate::new(&init)?;
        JsFuture::from(
            self.pc
                .add_ice_candidate_with_opt_rtc_ice_candidate(Some(&cand)),
        )
        .await?;
        Ok(())
    }

    pub fn close(&self) {
        if let Some(dc) = self.dc.borrow_mut().take() {
            dc.close();
        }
        self.pc.close();
    }

    #[allow(dead_code)]
    pub fn ice_state(&self) -> String {
        format!("{:?}", self.pc.ice_connection_state())
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.pc.set_ondatachannel(None);
        self.pc.set_onicecandidate(None);
        if let Some(dc) = self.dc.borrow_mut().take() {
            dc.set_onmessage(None);
            dc.close();
        }
        self.pc.close();
    }
}

// -- internal helpers --------------------------------------------------

fn make_pc() -> Result<RtcPeerConnection, JsValue> {
    let config = RtcConfiguration::new();
    let ice_server = RtcIceServer::new();
    let urls = js_sys::Array::new();
    urls.push(&JsValue::from_str("stun:stun.l.google.com:19302"));
    ice_server.set_urls(&urls);
    let servers = js_sys::Array::new();
    servers.push(&ice_server);
    config.set_ice_servers(&servers);
    RtcPeerConnection::new_with_configuration(&config)
}

/// Install the always-needed handlers: onicecandidate forwards local
/// candidates upstream; ondatachannel slots the callee's remote-created
/// datachannel into `dc_slot` when it arrives.
fn wire_common_handlers<F>(
    pc: &RtcPeerConnection,
    dc_slot: Rc<RefCell<Option<RtcDataChannel>>>,
    on_remote_msg: RemoteMsgHandler,
    mut on_local_ice: F,
    on_open: OpenHandler,
) -> Vec<Box<dyn std::any::Any>>
where
    F: FnMut(Option<IceCandidate>) + 'static,
{
    let mut handlers: Vec<Box<dyn std::any::Any>> = Vec::new();

    let on_ice = Closure::<dyn FnMut(RtcPeerConnectionIceEvent)>::new(
        move |ev: RtcPeerConnectionIceEvent| {
            let cand = ev.candidate().map(|c| IceCandidate {
                candidate: c.candidate(),
                sdp_mid: c.sdp_mid(),
                sdp_m_line_index: c.sdp_m_line_index(),
            });
            on_local_ice(cand);
        },
    );
    pc.set_onicecandidate(Some(on_ice.as_ref().unchecked_ref()));
    handlers.push(Box::new(on_ice));

    // Datachannel pushed by remote (callee path).
    let dc_slot_for_chan = dc_slot.clone();
    let on_remote_msg_chan = on_remote_msg.clone();
    let on_open_chan = on_open.clone();
    let on_dc = Closure::<dyn FnMut(RtcDataChannelEvent)>::new(move |ev: RtcDataChannelEvent| {
        let raw_dc = ev.channel();
        let mut local_handlers: Vec<Box<dyn std::any::Any>> = Vec::new();
        wire_dc_onmessage(&raw_dc, on_remote_msg_chan.clone(), &mut local_handlers);
        wire_dc_onopen(&raw_dc, on_open_chan.clone(), &mut local_handlers);
        // Move closures into a leaked container so they live for the
        // datachannel lifetime — we don't have a way to push back into
        // outer `_handlers` from this nested closure.
        for h in local_handlers {
            Box::leak(h);
        }
        *dc_slot_for_chan.borrow_mut() = Some(raw_dc);
    });
    pc.set_ondatachannel(Some(on_dc.as_ref().unchecked_ref()));
    handlers.push(Box::new(on_dc));

    handlers
}

/// Install onmessage on a datachannel, parking the closure into
/// `handlers` so it survives.
fn wire_dc_onmessage(
    dc: &RtcDataChannel,
    on_remote_msg: RemoteMsgHandler,
    handlers: &mut Vec<Box<dyn std::any::Any>>,
) {
    let on_msg = Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| {
        let data = ev.data();
        if let Some(s) = data.as_string() {
            on_remote_msg(s);
        }
    });
    dc.set_onmessage(Some(on_msg.as_ref().unchecked_ref()));
    handlers.push(Box::new(on_msg));
}

/// Install onopen on a datachannel. If the channel is already open by
/// the time we wire this up (race when callee receives a channel that
/// transitioned to Open before we attached our handler), fire the
/// callback synchronously so the replay still happens.
fn wire_dc_onopen(
    dc: &RtcDataChannel,
    on_open: OpenHandler,
    handlers: &mut Vec<Box<dyn std::any::Any>>,
) {
    if dc.ready_state() == RtcDataChannelState::Open {
        on_open();
        return;
    }
    let on_open_for_event = on_open.clone();
    let on_evt = Closure::<dyn FnMut()>::new(move || on_open_for_event());
    dc.set_onopen(Some(on_evt.as_ref().unchecked_ref()));
    handlers.push(Box::new(on_evt));
}
