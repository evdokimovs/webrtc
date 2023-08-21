use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use lazy_static::lazy_static;
use tokio::net::UdpSocket;
use tokio::time::Instant;

use super::*;

struct Calculator {
    send_pkt: u32,
    last_report_at: Instant,
}

lazy_static! {
    static ref CALCULATOR: Mutex<Calculator> = Mutex::new(Calculator {
        send_pkt: 0,
        last_report_at: Instant::now(),
    });

    static ref ASD: Mutex<HashSet<u16>> = Mutex::new(HashSet::new());
}

#[async_trait]
impl Conn for UdpSocket {
    async fn connect(&self, addr: SocketAddr) -> Result<()> {
        Ok(self.connect(addr).await?)
    }

    async fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        Ok(self.recv(buf).await?)
    }

    async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        Ok(self.recv_from(buf).await?)
    }

    async fn send(&self, buf: &[u8]) -> Result<usize> {
        Ok(self.send(buf).await?)
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> Result<usize> {
        Ok(self.send_to(buf, target).await?)
    }

    fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.local_addr()?)
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        None
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }
}
