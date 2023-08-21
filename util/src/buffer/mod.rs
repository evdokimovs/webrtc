#[cfg(test)]
mod buffer_test;

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Mutex, Notify};
use tokio::time::{timeout, Duration};

use crate::error::{Error, Result};

const MIN_SIZE: usize = 2048;
const CUTOFF_SIZE: usize = 128 * 1024;
const MAX_SIZE: usize = 4 * 1024 * 1024;

/// Buffer allows writing packets to an intermediate buffer, which can then be read form.
/// This is verify similar to bytes.Buffer but avoids combining multiple writes into a single read.
#[derive(Debug)]
struct BufferInternal {
    data: Vec<u8>,
    head: usize,
    tail: usize,

    closed: bool,
    subs: bool,

    count: usize,
    limit_count: usize,
    limit_size: usize,

    wait_for_notified: AtomicU32,
}

impl BufferInternal {
    /// available returns true if the buffer is large enough to fit a packet
    /// of the given size, taking overhead into account.
    fn available(&self, size: usize) -> bool {
        let mut available = self.head as isize - self.tail as isize;
        if available <= 0 {
            available += self.data.len() as isize;
        }
        // we interpret head=tail as empty, so always keep a byte free
        size as isize + 2 < available
    }

    /// grow increases the size of the buffer.  If it returns nil, then the
    /// buffer has been grown.  It returns ErrFull if hits a limit.
    fn grow(&mut self) -> Result<()> {
        let mut newsize = if self.data.len() < CUTOFF_SIZE {
            2 * self.data.len()
        } else {
            5 * self.data.len() / 4
        };

        if newsize < MIN_SIZE {
            newsize = MIN_SIZE
        }
        if (self.limit_size == 0/*|| sizeHardlimit*/) && newsize > MAX_SIZE {
            newsize = MAX_SIZE
        }

        // one byte slack
        if self.limit_size > 0 && newsize > self.limit_size + 1 {
            newsize = self.limit_size + 1
        }

        // newsize = 100000;
        if newsize <= self.data.len() {
            return Err(Error::ErrBufferFull);
        }

        let mut newdata: Vec<u8> = vec![0; newsize];

        let mut n;
        if self.head <= self.tail {
            // data was contiguous
            n = self.tail - self.head;
            newdata[..n].copy_from_slice(&self.data[self.head..self.tail]);
        } else {
            // data was discontiguous
            n = self.data.len() - self.head;
            newdata[..n].copy_from_slice(&self.data[self.head..]);
            newdata[n..n + self.tail].copy_from_slice(&self.data[..self.tail]);
            n += self.tail;
        }
        self.head = 0;
        self.tail = n;
        self.data = newdata;

        Ok(())
    }

    fn size(&self) -> usize {
        let mut size = self.tail as isize - self.head as isize;
        if size < 0 {
            size += self.data.len() as isize;
        }
        size as usize
    }
}

#[derive(Debug, Clone)]
pub struct Buffer {
    buffer: Arc<Mutex<BufferInternal>>,
    notify: Arc<Notify>,
    name: &'static str,
    read_cnt: Arc<AtomicU64>,
    write_cnt: Arc<AtomicU64>,
    last_report_at: Arc<Mutex<Instant>>,
}

impl Buffer {
    pub fn new(limit_count: usize, limit_size: usize, name: &'static str) -> Self {
        Buffer {
            buffer: Arc::new(Mutex::new(BufferInternal {
                data: vec![],
                head: 0,
                tail: 0,

                closed: false,
                subs: false,

                count: 0,
                limit_count,
                limit_size,
                wait_for_notified: AtomicU32::new(0),
            })),
            notify: Arc::new(Notify::new()),
            name,
            read_cnt: Arc::new(AtomicU64::new(0)),
            write_cnt: Arc::new(AtomicU64::new(0)),
            last_report_at: Arc::new(Mutex::new(Instant::now())),
        }
    }

    /// Write appends a copy of the packet data to the buffer.
    /// Returns ErrFull if the packet doesn't fit.
    /// Note that the packet size is limited to 65536 bytes since v0.11.0
    /// due to the internal data structure.
    pub async fn write(&self, packet: &[u8]) -> Result<usize> {
        self.write_cnt.fetch_add(1, Ordering::SeqCst);
        // if self.last_report_at.lock().await.elapsed() > Duration::from_secs(10) {
        //     println!("RW count ({}): R: {}, W: {}", self.name, self.read_cnt.load(Ordering::SeqCst), self.write_cnt.load(Ordering::SeqCst));
        //     *self.last_report_at.lock().await = Instant::now();
        // }
        if packet.len() >= 0x10000 {
            return Err(Error::ErrPacketTooBig);
        }

        let mut b = self.buffer.lock().await;

        if b.closed {
            println!("Buffer is closed");
            return Err(Error::ErrBufferClosed);
        }

        if (b.limit_count > 0 && b.count >= b.limit_count)
            || (b.limit_size > 0 && b.size() + 2 + packet.len() > b.limit_size)
        {
            println!("Error buffer full");
            return Err(Error::ErrBufferFull);
        }

        // grow the buffer until the packet fits
        // TODO(evdokimovs): What the fuck?
        while !b.available(packet.len()) {
            // println!("Growing buffer");
            b.grow()?;
        }

        // store the length of the packet
        let tail = b.tail;
        b.data[tail] = (packet.len() >> 8) as u8;
        b.tail += 1;
        if b.tail >= b.data.len() {
            b.tail = 0;
        }

        let tail = b.tail;
        b.data[tail] = packet.len() as u8;
        b.tail += 1;
        if b.tail >= b.data.len() {
            b.tail = 0;
        }

        // store the packet
        let end = std::cmp::min(b.data.len(), b.tail + packet.len());
        let n = end - b.tail;
        let tail = b.tail;
        b.data[tail..end].copy_from_slice(&packet[..n]);
        b.tail += n;
        if b.tail >= b.data.len() {
            // println!("Reached end of buffer, wrap around ({})", self.name);
            // we reached the end, wrap around
            let m = packet.len() - n;
            b.data[..m].copy_from_slice(&packet[n..]);
            b.tail = m;
        }
        b.count += 1;

        if b.subs {
            // we have other are waiting data
            self.notify.notify_one();
            b.subs = false;
        }

        // println!(" read copied");
        if self.last_report_at.lock().await.elapsed() > Duration::from_secs(3) {
            // println!("RW count ({}): R: {}, W: {}", self.name, self.read_cnt.load(Ordering::SeqCst), self.write_cnt.load(Ordering::SeqCst));
            *self.last_report_at.lock().await = Instant::now();
        }
        Ok(packet.len())
    }

    // Read populates the given byte slice, returning the number of bytes read.
    // Blocks until data is available or the buffer is closed.
    // Returns io.ErrShortBuffer is the packet is too small to copy the Write.
    // Returns io.EOF if the buffer is closed.
    pub async fn read(&self, packet: &mut [u8], duration: Option<Duration>) -> Result<usize> {
        // if self.last_report_at.lock().await.elapsed() > Duration::from_secs(3) {
        //     println!("RW count ({}): R: {}, W: {}", self.name, self.read_cnt.load(Ordering::SeqCst), self.write_cnt.load(Ordering::SeqCst));
        //     *self.last_report_at.lock().await = Instant::now();
        // }
        // const PKT: &[u8] = &[144, 137, 169, 60, 126, 6, 130, 30, 113, 61, 188, 96, 190, 222, 0, 1, 16, 48, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95, 96, 97, 98, 99, 100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115, 116, 117, 118, 119, 120, 121, 122, 123, 124, 125, 126, 127, 128, 129, 130, 131, 132, 133, 134, 135, 136, 137, 138, 139, 140, 141, 142, 143, 144, 145, 146, 147, 148, 149, 150, 151, 152, 153, 154, 155, 156, 157, 158, 159, 160, 161, 162, 163, 164, 165, 166, 167, 168, 169, 170, 171, 172, 173, 174, 175, 176, 177, 178, 179, 180, 181, 182, 183, 184, 185, 186, 187, 188, 189, 190, 191, 192, 193, 194, 195, 196, 197, 198, 199, 200, 201, 202, 203, 204, 205, 206, 207, 208, 209, 210, 211, 212, 213, 214, 215, 216, 217, 218, 219, 220, 221, 222, 223, 224, 225, 226, 227, 228, 229, 230, 231, 232, 233, 234, 235, 236, 237, 238, 239, 240, 241, 242, 243, 244, 245, 246, 247, 248, 249, 250, 251, 252, 253, 254, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95, 96, 97, 98, 99, 100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115, 116, 117, 118, 119, 120, 121, 122, 123, 124, 125, 126, 127, 128, 129, 130, 131, 132, 133, 134, 135, 136, 137, 138, 139, 140, 141, 142, 143, 144, 145, 146, 147, 148, 149, 150, 151, 152, 153, 154, 155, 156, 157, 158, 159, 160, 161, 162, 163, 164, 165, 166, 167, 168, 169, 170, 171, 172, 173, 174, 175, 176, 177, 178, 179, 180, 181, 182, 183, 184, 185, 186, 187, 188, 189, 190, 191, 192, 193, 194, 195, 196, 197, 198, 199, 200, 201, 202, 203, 204, 205, 206, 207, 208, 209, 210, 211, 212, 213, 214, 215, 216, 217, 218, 219, 220, 221, 222, 223, 224, 225, 226, 227, 228, 229, 230, 231, 232, 233, 234, 235, 236, 237, 238, 239, 240, 241, 242, 243, 244, 245, 246, 247, 248, 249, 250, 251, 252, 253, 254, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95, 96, 97, 98, 99, 100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115, 116, 117, 118, 119, 120, 121, 122, 123, 124, 125, 126, 127, 128, 129, 130, 131, 132, 133, 134, 135, 136, 137, 138, 139, 140, 141, 142, 143, 144, 145, 146, 147, 148, 149, 150, 151, 152, 153, 154, 155, 156, 157, 158, 159, 160, 161, 162, 163, 164, 165, 166, 167, 168, 169, 170, 171, 172, 173, 174, 175, 176, 177, 178, 179, 180, 181, 182, 183, 184, 185, 186, 187, 188, 189, 190, 191, 192, 193, 194, 195, 196, 197, 198, 199, 200, 201, 202, 203, 204, 205, 206, 207, 208, 209, 210, 211, 212, 213, 214, 215, 216, 217, 218, 219, 220, 221, 222, 223, 224, 225, 226, 227, 228, 229, 230, 231, 232, 233, 234, 235, 236, 237, 238, 239, 240, 241, 242, 243, 244, 245, 246, 247, 248, 249, 250, 251, 252, 253, 254, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95, 96, 97, 98, 99, 100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115, 116, 117, 118, 119, 120, 121, 122, 123, 124, 125, 126, 127, 128, 129, 130, 131, 132, 133, 134, 135, 136, 137, 138, 139, 140, 141, 142, 143, 144, 145, 146, 147, 148, 149, 150, 151, 152, 153, 154, 155, 156, 157, 158, 159, 160, 161, 162, 163, 164, 165, 166, 167, 168, 169, 170, 171, 172, 173, 174, 175, 176, 177, 178, 179, 180, 181, 182, 183, 184, 185, 186, 187, 188, 189, 190, 191, 192, 193, 194, 195, 196, 197, 198, 199, 200, 201, 202, 203, 204, 205, 206, 207, 208, 209, 210, 211, 212, 213, 214, 215, 216, 217, 218, 219, 220, 221, 222, 223, 224, 225, 226, 227, 228, 229, 230, 231, 232, 233, 234, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        // if packet.len() >= PKT.len() {
        //     packet[..PKT.len()].copy_from_slice(PKT);
        //     return Ok(PKT.len());
        // } else {
        //     return Ok(0);
        // }

        self.read_cnt.fetch_add(1, Ordering::SeqCst);
        loop {
            {
                // use {} to let LockGuard RAII
                let mut b = self.buffer.lock().await;

                if b.head != b.tail {
                    // decode the packet size
                    let n1 = b.data[b.head];
                    b.head += 1;
                    if b.head >= b.data.len() {
                        b.head = 0;
                    }
                    let n2 = b.data[b.head];
                    b.head += 1;
                    if b.head >= b.data.len() {
                        b.head = 0;
                    }
                    let count = ((n1 as usize) << 8) | n2 as usize;

                    // determine the number of bytes we'll actually copy
                    let mut copied = count;
                    if copied > packet.len() {
                        copied = packet.len();
                    }

                    // copy the data
                    if b.head + copied < b.data.len() {
                        packet[..copied].copy_from_slice(&b.data[b.head..b.head + copied]);
                    } else {
                        let k = b.data.len() - b.head;
                        packet[..k].copy_from_slice(&b.data[b.head..]);
                        packet[k..copied].copy_from_slice(&b.data[..copied - k]);
                    }

                    // advance head, discarding any data that wasn't copied
                    b.head += count;
                    if b.head >= b.data.len() {
                        b.head -= b.data.len();
                    }

                    if b.head == b.tail {
                        // the buffer is empty, reset to beginning
                        // in order to improve cache locality.
                        b.head = 0;
                        b.tail = 0;
                    }

                    b.count -= 1;

                    if copied < count {
                        // println!("buffer short while reading");
                        return Err(Error::ErrBufferShort);
                    }
                    // println!(" read copied");
                    if self.last_report_at.lock().await.elapsed() > Duration::from_secs(3) {
                        // println!("RW count ({}): R: {}, W: {}", self.name, self.read_cnt.load(Ordering::SeqCst), self.write_cnt.load(Ordering::SeqCst));
                        *self.last_report_at.lock().await = Instant::now();
                    }
                    // println!("{:?}", packet);
                    return Ok(copied);
                } else {
                    // Dont have data -> need wait
                    b.subs = true;
                }

                if b.closed {
                    // println!("buffer closed error");
                    return Err(Error::ErrBufferClosed);
                }
            }

            // Wait for signal.
            if let Some(d) = duration {
                if timeout(d, self.notify.notified()).await.is_err() {
                    return Err(Error::ErrTimeout);
                }
            } else {
                self.notify.notified().await;
            }
        }
    }

    // Close will unblock any readers and prevent future writes.
    // Data in the buffer can still be read, returning io.EOF when fully depleted.
    pub async fn close(&self) {
        // note: We don't use defer so we can close the notify channel after unlocking.
        // This will unblock goroutines that can grab the lock immediately, instead of blocking again.
        let mut b = self.buffer.lock().await;

        if b.closed {
            return;
        }

        b.closed = true;
        self.notify.notify_waiters();
    }

    pub async fn is_closed(&self) -> bool {
        let b = self.buffer.lock().await;

        b.closed
    }

    // Count returns the number of packets in the buffer.
    pub async fn count(&self) -> usize {
        let b = self.buffer.lock().await;

        b.count
    }

    // set_limit_count controls the maximum number of packets that can be buffered.
    // Causes Write to return ErrFull when this limit is reached.
    // A zero value will disable this limit.
    pub async fn set_limit_count(&self, limit: usize) {
        let mut b = self.buffer.lock().await;

        b.limit_count = limit
    }

    // Size returns the total byte size of packets in the buffer.
    pub async fn size(&self) -> usize {
        let b = self.buffer.lock().await;

        b.size()
    }

    // set_limit_size controls the maximum number of bytes that can be buffered.
    // Causes Write to return ErrFull when this limit is reached.
    // A zero value means 4MB since v0.11.0.
    //
    // User can set packetioSizeHardlimit build tag to enable 4MB hardlimit.
    // When packetioSizeHardlimit build tag is set, set_limit_size exceeding
    // the hardlimit will be silently discarded.
    pub async fn set_limit_size(&self, limit: usize) {
        let mut b = self.buffer.lock().await;

        b.limit_size = limit
    }
}
