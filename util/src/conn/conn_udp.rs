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
        let inst = Instant::now();
        // println!("BT: {:#?}", std::backtrace::Backtrace::capture());
        let foo = Ok(self.recv_from(buf).await?);
        if inst.elapsed().as_micros() > 100 {
            println!("Elapsed in recv_from: {}", inst.elapsed().as_micros());
        }
        return foo;
    }

    async fn send(&self, buf: &[u8]) -> Result<usize> {
        {
            let mut calculator = CALCULATOR.lock().unwrap();
            if calculator.last_report_at.elapsed() > Duration::from_secs(1) {
                println!("Send (send) packets in 1 second: {}", calculator.send_pkt);
                calculator.send_pkt = 0;
                calculator.last_report_at = Instant::now();
            }
            calculator.send_pkt += 1;
        }
        Ok(self.send(buf).await.map_err(|e| {
            println!("Error fired while sending: {:?}", e);
            e
        })?)
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> Result<usize> {
        {
            let mut calculator = CALCULATOR.lock().unwrap();
            if calculator.last_report_at.elapsed() > Duration::from_secs(1) {
                println!("Send (sendto) packets in 1 second: {}", calculator.send_pkt);
                calculator.send_pkt = 0;
                calculator.last_report_at = Instant::now();
            }
            calculator.send_pkt += 1;
        }
        Ok(self.send_to(buf, target).await.map_err(|e| {
            println!("Error fired while sending: {:?}", e);
            e
        })?)
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
