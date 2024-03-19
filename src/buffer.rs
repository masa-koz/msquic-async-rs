use bytes::Bytes;

use libc::c_void;

pub(crate) struct StreamRecvBuffer {
    handle: Option<msquic::Handle>,
    buffers: Vec<msquic::Buffer>,
    total_length: usize,
    read: usize,
    fin: bool,
}

impl StreamRecvBuffer {
    pub(crate) unsafe fn new<T: AsRef<[msquic::Buffer]>>(
        buffers: &T,
        fin: bool,
        handle: Option<msquic::Handle>,
    ) -> Self {
        let total_length: u32 = buffers.as_ref().iter().map(|x| x.length).sum();
        Self {
            handle,
            buffers: buffers.as_ref().to_vec(),
            total_length: total_length as usize,
            read: 0,
            fin,
        }
    }

    pub(crate) fn next<'a>(&mut self, max_length: usize) -> Option<&'a [u8]> {
        if self.read >= self.total_length {
            return None;
        }
        let mut skip = self.read;
        for buffer in &self.buffers {
            if (buffer.length as usize) <= skip {
                skip -= buffer.length as usize;
                continue;
            }
            let len = std::cmp::min(buffer.length as usize - skip, max_length);
            self.read += len;
            let slice = unsafe { std::slice::from_raw_parts(buffer.buffer.add(skip), len) };
            return Some(slice);
        }
        None
    }

    pub(crate) fn fin(&self) -> bool {
        self.fin
    }
}

impl Drop for StreamRecvBuffer {
    fn drop(&mut self) {
        println!("StreamRecvBuffer dropped");
        if let Some(handle) = self.handle.take() {
            println!(
                "stream_receive_complete self.total_length: {}",
                self.total_length
            );
            crate::MSQUIC_API.stream_receive_complete(handle, self.total_length as u64);
        }
    }
}

pub(crate) struct WriteBuffer(Box<WriteBufferInner>);

struct WriteBufferInner {
    internal: Vec<u8>,
    zerocopy: Vec<Bytes>,
    msquic_buffer: Vec<msquic::Buffer>,
}

impl WriteBuffer {
    pub(crate) fn new() -> Self {
        WriteBuffer(Box::new(WriteBufferInner {
            internal: Vec::new(),
            zerocopy: Vec::new(),
            msquic_buffer: Vec::new(),
        }))
    }

    pub(crate) unsafe fn from_raw(inner: *const c_void) -> Self {
        WriteBuffer(unsafe { Box::from_raw(inner as *mut WriteBufferInner) })
    }

    pub(crate) fn put_zerocopy(&mut self, buf: &Bytes) -> usize {
        self.0.zerocopy.push(buf.clone());
        buf.len()
    }

    pub(crate) fn put_slice(&mut self, slice: &[u8]) -> usize {
        self.0.internal.extend_from_slice(slice);
        slice.len()
    }

    pub(crate) fn get_buffer(&mut self) -> (*const msquic::Buffer, u32) {
        if !self.0.zerocopy.is_empty() {
            for buf in &self.0.zerocopy {
                self.0.msquic_buffer.push(buf[..].into());
            }
        } else {
            self.0.msquic_buffer.push((&self.0.internal).into());
        }
        let ptr = self.0.msquic_buffer.as_ptr();
        let len = self.0.msquic_buffer.len() as u32;
        (ptr, len)
    }

    pub(crate) fn into_raw(self) -> *mut c_void {
        Box::into_raw(self.0) as *mut c_void
    }

    pub(crate) fn reset(&mut self) {
        self.0.internal.clear();
        self.0.zerocopy.clear();
        self.0.msquic_buffer.clear();
    }
}
