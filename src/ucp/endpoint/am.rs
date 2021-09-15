use tokio::sync::Notify;

use super::*;
use std::{
    collections::VecDeque,
    io::{IoSlice, IoSliceMut},
    slice,
    sync::{atomic::AtomicBool, Mutex},
};

#[derive(Debug, PartialEq, Eq)]
pub enum AmDataType {
    Eager,
    Data,
    Rndv,
}

enum AmData {
    Eager(Vec<u8>),
    Data(&'static [u8]),
    Rndv(&'static [u8]),
}

impl AmData {
    fn from_raw(data: &'static [u8], attr: u64) -> Option<AmData> {
        if data.len() == 0 {
            None
        } else if attr & ucp_am_recv_attr_t::UCP_AM_RECV_ATTR_FLAG_DATA as u64 != 0 {
            Some(AmData::Data(data))
        } else if attr & ucp_am_recv_attr_t::UCP_AM_RECV_ATTR_FLAG_RNDV as u64 != 0 {
            Some(AmData::Rndv(data))
        } else {
            Some(AmData::Eager(data.to_owned()))
        }
    }

    #[inline]
    fn data_type(&self) -> AmDataType {
        match self {
            AmData::Eager(_) => AmDataType::Eager,
            AmData::Data(_) => AmDataType::Data,
            AmData::Rndv(_) => AmDataType::Rndv,
        }
    }

    #[inline]
    fn data(&self) -> Option<&[u8]> {
        match self {
            AmData::Eager(data) => Some(data.as_slice()),
            AmData::Data(data) => Some(*data),
            _ => None,
        }
    }

    #[inline]
    fn len(&self) -> usize {
        match self {
            AmData::Eager(data) => data.len(),
            AmData::Data(data) => data.len(),
            AmData::Rndv(desc) => desc.len(),
        }
    }
}

struct RawMsg {
    id: u32,
    header: Vec<u8>,
    data: Option<AmData>,
    reply_ep: ucp_ep_h,
    attr: u64,
}

impl RawMsg {
    fn from_raw(
        id: u32,
        header: &[u8],
        data: &'static [u8],
        reply_ep: ucp_ep_h,
        attr: u64,
    ) -> Self {
        RawMsg {
            id,
            header: header.to_owned(),
            data: AmData::from_raw(data, attr),
            reply_ep,
            attr,
        }
    }
}

pub struct AmMsg<'a> {
    worker: &'a Worker,
    msg: RawMsg,
}

impl<'a> AmMsg<'a> {
    fn from_raw(worker: &'a Worker, msg: RawMsg) -> Self {
        AmMsg { worker, msg }
    }

    #[inline]
    pub fn id(&self) -> u32 {
        self.msg.id
    }

    #[inline]
    pub fn header(&self) -> &[u8] {
        self.msg.header.as_ref()
    }

    #[inline]
    pub fn contains_data(&self) -> bool {
        self.data_type().is_some()
    }

    pub fn data_type(&self) -> Option<AmDataType> {
        self.msg.data.as_ref().map(|data| data.data_type())
    }

    #[inline]
    pub fn get_data(&self) -> Option<&[u8]> {
        self.msg.data.as_ref().and_then(|data| data.data())
    }

    #[inline]
    pub fn data_len(&self) -> usize {
        self.msg.data.as_ref().map_or(0, |data| data.len())
    }

    pub async fn recv_data(&mut self) -> Result<Vec<u8>, ()> {
        match self.msg.data.take() {
            None => Ok(Vec::new()),
            Some(AmData::Eager(vec)) => Ok(vec),
            Some(data) => {
                self.msg.data = Some(data);
                let mut buf = Vec::with_capacity(self.data_len());
                unsafe {
                    buf.set_len(self.data_len());
                }
                let recv_size = self.recv_data_single(&mut buf).await?;
                buf.truncate(recv_size);
                Ok(buf)
            }
        }
    }

    pub async fn recv_data_single(&mut self, buf: &mut [u8]) -> Result<usize, ()> {
        if !self.contains_data() {
            Ok(0)
        } else {
            let iov = [IoSliceMut::new(buf)];
            self.recv_data_vectored(&iov).await
        }
    }

    pub async fn recv_data_vectored(&mut self, iov: &[IoSliceMut<'_>]) -> Result<usize, ()> {
        let data = self.msg.data.take();
        if let Some(data) = data {
            if let AmData::Eager(mut data) = data {
                // return error if buffer size < data length, same with ucx
                let cap = iov.iter().fold(0_usize, |cap, buf| cap + buf.len());
                assert!(cap >= data.len());

                let mut copyed = 0_usize;
                for buf in iov {
                    let len = std::cmp::min(copyed, buf.len());
                    if len == 0 {
                        break;
                    }

                    let buf = &buf[..len];
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            data[copyed..].as_mut_ptr(),
                            buf.as_ptr() as _,
                            len,
                        )
                    }
                    copyed += len;
                }
                return Ok(copyed);
            }

            let (data_desc, data_len) = match data {
                AmData::Data(data) => (data.as_ptr(), data.len()),
                AmData::Rndv(data) => (data.as_ptr(), data.len()),
                _ => unreachable!(),
            };

            unsafe extern "C" fn callback(
                request: *mut c_void,
                status: ucs_status_t,
                _length: u64,
                _data: *mut c_void,
            ) {
                // todo: handle error & fix real data length
                trace!(
                    "recv_data_vectored: complete, req={:?}, status={:?}",
                    request,
                    status
                );
                let request = &mut *(request as *mut Request);
                request.waker.wake();
            }
            trace!(
                "recv_data_vectored: worker={:?} iov.len={}",
                self.worker.handle,
                iov.len()
            );
            let mut param = MaybeUninit::<ucp_request_param_t>::uninit();
            let (buffer, count) = unsafe {
                let param = &mut *param.as_mut_ptr();
                param.op_attr_mask = ucp_op_attr_t::UCP_OP_ATTR_FIELD_CALLBACK as u32
                    | ucp_op_attr_t::UCP_OP_ATTR_FIELD_DATATYPE as u32;
                param.cb = ucp_request_param_t__bindgen_ty_1 {
                    recv_am: Some(callback),
                };

                if iov.len() == 1 {
                    param.datatype = ucp_dt_make_contig(1);
                    (iov[0].as_ptr(), iov[0].len())
                } else {
                    param.datatype = ucp_dt_type::UCP_DATATYPE_IOV as _;
                    (iov.as_ptr() as _, iov.len())
                }
            };

            let status = unsafe {
                ucp_am_recv_data_nbx(
                    self.worker.handle,
                    data_desc as _,
                    buffer as _,
                    count as _,
                    param.as_ptr(),
                )
            };
            if status.is_null() {
                trace!("recv_data_vectored: complete");
                Ok(data_len)
            } else if UCS_PTR_IS_PTR(status) {
                RequestHandle {
                    ptr: status,
                    poll_fn: poll_recv,
                }
                .await;
                Ok(data_len)
            } else {
                panic!("failed to recv data: {:?}", UCS_PTR_RAW_STATUS(status));
            }
        } else {
            // no data
            Ok(0)
        }
    }

    #[inline]
    pub fn need_reply(&self) -> bool {
        self.msg.attr & (ucp_am_recv_attr_t::UCP_AM_RECV_ATTR_FIELD_REPLY_EP as u64) != 0
            && !self.msg.reply_ep.is_null()
    }

    /// Send reply
    pub async fn reply(
        &self,
        id: u32,
        header: &[u8],
        data: &[u8],
        need_reply: bool,
        proto: Option<AmProto>,
    ) -> Result<(), ()> {
        // todo: we should prevent endpoint from being freed
        //       currently, ucx doesn't provide such function.
        assert_eq!(self.need_reply(), true);
        self.reply_vectorized(id, header, &[IoSlice::new(data)], need_reply, proto)
            .await
    }

    /// Send reply
    pub async fn reply_vectorized(
        &self,
        id: u32,
        header: &[u8],
        data: &[IoSlice<'_>],
        need_reply: bool,
        proto: Option<AmProto>,
    ) -> Result<(), ()> {
        assert_eq!(self.need_reply(), true);
        am_send(self.msg.reply_ep, id, header, data, need_reply, proto).await
    }
}

impl<'a> Drop for AmMsg<'a> {
    fn drop(&mut self) {
        match self.msg.data.take() {
            Some(AmData::Data(desc)) => unsafe {
                ucp_am_data_release(self.worker.handle, desc.as_ptr() as _);
            },
            Some(AmData::Rndv(desc)) => unsafe {
                ucp_am_data_release(self.worker.handle, desc.as_ptr() as _);
            },
            _ => (),
        }
    }
}

pub(crate) struct AmHandler {
    id: u32,
    msgs: Mutex<VecDeque<RawMsg>>,
    notify: Notify,
    unregistered: AtomicBool,
}

impl AmHandler {
    // new active message handler
    fn new(id: u32) -> Self {
        Self {
            id,
            msgs: Mutex::new(VecDeque::with_capacity(512)),
            notify: Notify::new(),
            unregistered: AtomicBool::new(false),
        }
    }

    // unregister
    fn unregister(&self) {
        self.unregistered
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    // callback function
    fn callback(&self, header: &[u8], data: &'static [u8], reply: ucp_ep_h, attr: u64) {
        let msg = RawMsg::from_raw(self.id, header, data, reply, attr);
        self.msgs.lock().unwrap().push_back(msg);
        self.notify.notify_one();
    }

    // wait active message
    async fn wait_msg<'a>(&self, worker: &'a Worker) -> Option<AmMsg<'a>> {
        while !self.unregistered.load(std::sync::atomic::Ordering::Relaxed) {
            if let Some(msg) = self.msgs.lock().unwrap().pop_front() {
                return Some(AmMsg::from_raw(worker, msg));
            }

            self.notify.notified().await;
        }

        self.msgs
            .lock()
            .unwrap()
            .pop_front()
            .map(|msg| AmMsg::from_raw(worker, msg))
    }
}

impl Worker {
    pub fn am_register(&self, id: u32) {
        unsafe extern "C" fn callback(
            arg: *mut c_void,
            header: *const c_void,
            header_len: u64,
            data: *mut c_void,
            data_len: u64,
            param: *const ucp_am_recv_param_t,
        ) -> ucs_status_t {
            let handler = &*(arg as *const AmHandler);
            let header = slice::from_raw_parts(header as *const u8, header_len as usize);
            let data = slice::from_raw_parts(data as *const u8, data_len as usize);

            assert!(!param.is_null());
            let param = &*param;
            handler.callback(header, data, param.reply_ep, param.recv_attr);

            const DATA_FLAG: u64 = ucp_am_recv_attr_t::UCP_AM_RECV_ATTR_FLAG_DATA as u64
                | ucp_am_recv_attr_t::UCP_AM_RECV_ATTR_FLAG_RNDV as u64;
            if param.recv_attr & DATA_FLAG != 0 {
                ucs_status_t::UCS_INPROGRESS
            } else {
                ucs_status_t::UCS_OK
            }
        }

        if self.am_handlers.read().unwrap().contains_key(&id) {
            return;
        }

        let handler = Rc::new(AmHandler::new(id));
        let mut guard = self.am_handlers.write().unwrap();
        if guard.contains_key(&id) {
            return;
        }
        let param = ucp_am_handler_param_t {
            id,
            cb: Some(callback),
            arg: Rc::into_raw(handler.clone()) as _,
            field_mask: (ucp_am_handler_param_field::UCP_AM_HANDLER_PARAM_FIELD_ID
                | ucp_am_handler_param_field::UCP_AM_HANDLER_PARAM_FIELD_CB
                | ucp_am_handler_param_field::UCP_AM_HANDLER_PARAM_FIELD_ARG)
                .0 as _,
            flags: 0,
        };
        let status = unsafe { ucp_worker_set_am_recv_handler(self.handle, &param as _) };
        assert!(status == ucs_status_t::UCS_OK);
        guard.insert(id, handler);
    }

    // Unregister active message handler
    pub fn am_unregister(&self, id: u32) {
        let handler = self.am_handlers.write().unwrap().remove(&id);
        if let Some(handler) = handler {
            handler.unregister();
        }
    }

    pub async fn am_recv<'a>(&'a self, id: u32) -> Option<AmMsg<'a>> {
        let handler = self.am_handlers.read().unwrap().get(&id).cloned();
        if let Some(handler) = handler {
            handler.wait_msg(self).await
        } else {
            None
        }
    }
}

impl Endpoint {
    pub async fn am_send(
        &self,
        id: u32,
        header: &[u8],
        data: &[u8],
        need_reply: bool,
        proto: Option<AmProto>,
    ) -> Result<(), ()> {
        let data = [IoSlice::new(data)];
        self.am_send_vectorized(id, header, &data, need_reply, proto)
            .await
    }

    pub async fn am_send_vectorized(
        &self,
        id: u32,
        header: &[u8],
        data: &[IoSlice<'_>],
        need_reply: bool,
        proto: Option<AmProto>,
    ) -> Result<(), ()> {
        let endpoint = self.handle;
        am_send(endpoint, id, header, data, need_reply, proto).await
    }
}

pub enum AmProto {
    /// Some UCT transport may corrupt with eager proto,
    /// don't use this!
    /// todo: why?
    Eager,
    Rndv,
}

async fn am_send(
    endpoint: ucp_ep_h,
    id: u32,
    header: &[u8],
    data: &[IoSlice<'_>],
    need_reply: bool,
    proto: Option<AmProto>,
) -> Result<(), ()> {
    unsafe extern "C" fn callback(request: *mut c_void, _status: ucs_status_t, _data: *mut c_void) {
        trace!("am_send: complete");
        let request = &mut *(request as *mut Request);
        request.waker.wake();
    }

    let mut param = MaybeUninit::<ucp_request_param_t>::uninit();
    // let mut buffer = vec![0_u8; 1000];
    let (buffer, count) = unsafe {
        let param = &mut *param.as_mut_ptr();
        param.op_attr_mask = ucp_op_attr_t::UCP_OP_ATTR_FIELD_CALLBACK as u32
            | ucp_op_attr_t::UCP_OP_ATTR_FIELD_DATATYPE as u32
            | ucp_op_attr_t::UCP_OP_ATTR_FIELD_FLAGS as u32;
        param.flags = 0;
        param.cb = ucp_request_param_t__bindgen_ty_1 {
            send: Some(callback),
        };

        match proto {
            Some(AmProto::Eager) => param.flags |= ucp_send_am_flags::UCP_AM_SEND_FLAG_EAGER.0,
            Some(AmProto::Rndv) => param.flags |= ucp_send_am_flags::UCP_AM_SEND_FLAG_RNDV.0,
            _ => (),
        }

        if need_reply {
            param.flags |= ucp_send_am_flags::UCP_AM_SEND_FLAG_REPLY.0;
        }

        if data.len() == 1 {
            param.datatype = ucp_dt_make_contig(1);
            (data[0].as_ptr(), data[0].len())
        } else {
            param.datatype = ucp_dt_type::UCP_DATATYPE_IOV as _;
            (data.as_ptr() as _, data.len())
        }
    };

    let status = unsafe {
        ucp_am_send_nbx(
            endpoint,
            id,
            header.as_ptr() as _,
            header.len() as _,
            buffer as _,
            count as _,
            param.as_mut_ptr(),
        )
    };
    if status.is_null() {
        trace!("am_send: complete");
        Ok(())
    } else if UCS_PTR_IS_PTR(status) {
        RequestHandle {
            ptr: status,
            poll_fn: poll_normal,
        }
        .await;
        Ok(())
    } else {
        panic!("failed to send am: {:?}", UCS_PTR_RAW_STATUS(status));
    }
}

unsafe fn poll_recv(ptr: ucs_status_ptr_t) -> Poll<()> {
    let status = ucp_request_check_status(ptr as _);
    if status == ucs_status_t::UCS_INPROGRESS {
        Poll::Pending
    } else {
        Poll::Ready(())
    }
}

#[cfg(test)]
#[cfg(feature = "am")]
mod tests {
    use super::*;

    #[test]
    fn am() {
        spawn_thread!(send_recv()).join().unwrap();
    }

    async fn send_recv() {
        let context1 = Context::new();
        let worker1 = context1.create_worker();
        let context2 = Context::new();
        let worker2 = context2.create_worker();
        tokio::task::spawn_local(worker1.clone().polling());
        tokio::task::spawn_local(worker2.clone().polling());

        // connect with each other
        let mut listener = worker1.create_listener("0.0.0.0:0".parse().unwrap());
        let listen_port = listener.socket_addr().port();
        let mut addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        addr.set_port(listen_port);
        let endpoint2 = worker2.connect(addr);
        let conn1 = listener.next().await;
        let endpoint1 = worker1.accept(conn1);

        worker1.am_register(16);
        worker2.am_register(12);

        let header = vec![1, 2, 3, 4];
        let data = vec![1 as u8; 1 << 10];
        let (_, msg) = tokio::join!(
            async {
                // send msg
                let result = endpoint2.am_send(16, &header, &data, true, None).await;
                assert!(result.is_ok());
            },
            async {
                // recv msg
                let msg = worker1.am_recv(16).await;
                let mut msg = msg.expect("no msg");
                assert_eq!(msg.header(), &header);
                assert_eq!(msg.contains_data(), true);
                assert_eq!(msg.data_len(), data.len());
                let recv_data = msg.recv_data().await.unwrap();
                assert_eq!(data, recv_data);
                assert_eq!(msg.contains_data(), false);
                msg
            }
        );

        let header = vec![1, 3, 9, 10];
        let data = vec![9 as u8; 1 << 20];
        tokio::join!(
            async {
                // send reply
                let result = msg
                    .reply(12, &header, &data, false, Some(AmProto::Rndv))
                    .await;
                assert!(result.is_ok());
            },
            async {
                // recv reply
                let reply = worker2.am_recv(12).await;
                let mut reply = reply.expect("no reply");
                assert_eq!(reply.header(), &header);
                assert_eq!(reply.contains_data(), true);
                assert_eq!(reply.data_len(), data.len());
                let recv_data = reply.recv_data().await.unwrap();
                assert_eq!(data, recv_data);
                assert_eq!(reply.contains_data(), false);
            }
        );

        endpoint1.close().await;
        endpoint2.close().await;
    }
}
