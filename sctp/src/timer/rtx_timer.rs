use crate::association::RtxTimerId;
use async_trait::async_trait;
use std::sync::{Arc, Weak};
use tokio::sync::{mpsc, Mutex};
use tokio::time::Duration;

pub(crate) const RTO_INITIAL: u64 = 3000; // msec
pub(crate) const RTO_MIN: u64 = 1000; // msec
pub(crate) const RTO_MAX: u64 = 60000; // msec
pub(crate) const RTO_ALPHA: u64 = 1;
pub(crate) const RTO_BETA: u64 = 2;
pub(crate) const RTO_BASE: u64 = 8;
pub(crate) const MAX_INIT_RETRANS: usize = 8;
pub(crate) const PATH_MAX_RETRANS: usize = 5;
pub(crate) const NO_MAX_RETRANS: usize = 0;

/// rtoManager manages Rtx timeout values.
/// This is an implementation of RFC 4960 sec 6.3.1.
#[derive(Default, Debug)]
pub(crate) struct RtoManager {
    pub(crate) srtt: u64,
    pub(crate) rttvar: f64,
    pub(crate) rto: u64,
    pub(crate) no_update: bool,
}

impl RtoManager {
    /// newRTOManager creates a new rtoManager.
    pub(crate) fn new() -> Self {
        RtoManager {
            rto: RTO_INITIAL,
            ..Default::default()
        }
    }

    /// set_new_rtt takes a newly measured RTT then adjust the RTO in msec.
    pub(crate) fn set_new_rtt(&mut self, rtt: u64) -> u64 {
        if self.no_update {
            return self.srtt;
        }

        if self.srtt == 0 {
            // First measurement
            self.srtt = rtt;
            self.rttvar = rtt as f64 / 2.0;
        } else {
            // Subsequent rtt measurement
            self.rttvar = ((RTO_BASE - RTO_BETA) as f64 * self.rttvar
                + RTO_BETA as f64 * (self.srtt as i64 - rtt as i64).abs() as f64)
                / RTO_BASE as f64;
            self.srtt = ((RTO_BASE - RTO_ALPHA) * self.srtt + RTO_ALPHA * rtt) / RTO_BASE;
        }

        self.rto = (self.srtt + (4.0 * self.rttvar) as u64).clamp(RTO_MIN, RTO_MAX);

        self.srtt
    }

    /// get_rto simply returns the current RTO in msec.
    pub(crate) fn get_rto(&self) -> u64 {
        self.rto
    }

    /// reset resets the RTO variables to the initial values.
    pub(crate) fn reset(&mut self) {
        if self.no_update {
            return;
        }

        self.srtt = 0;
        self.rttvar = 0.0;
        self.rto = RTO_INITIAL;
    }

    /// set RTO value for testing
    pub(crate) fn set_rto(&mut self, rto: u64, no_update: bool) {
        self.rto = rto;
        self.no_update = no_update;
    }
}

pub(crate) fn calculate_next_timeout(rto: u64, n_rtos: usize) -> u64 {
    // RFC 4096 sec 6.3.3.  Handle T3-rtx Expiration
    //   E2)  For the destination address for which the timer expires, set RTO
    //        <- RTO * 2 ("back off the timer").  The maximum value discussed
    //        in rule C7 above (RTO.max) may be used to provide an upper bound
    //        to this doubling operation.
    if n_rtos < 31 {
        std::cmp::min(rto << n_rtos, RTO_MAX)
    } else {
        RTO_MAX
    }
}

/// rtxTimerObserver is the inteface to a timer observer.
/// NOTE: Observers MUST NOT call start() or stop() method on rtxTimer
/// from within these callbacks.
#[async_trait]
pub(crate) trait RtxTimerObserver {
    async fn on_retransmission_timeout(&mut self, timer_id: RtxTimerId, n: usize);
    async fn on_retransmission_failure(&mut self, timer_id: RtxTimerId);
}

/// rtxTimer provides the retnransmission timer conforms with RFC 4960 Sec 6.3.1
#[derive(Default, Debug)]
pub(crate) struct RtxTimer<T: 'static + RtxTimerObserver + Send> {
    pub(crate) timeout_observer: Weak<Mutex<T>>,
    pub(crate) id: RtxTimerId,
    pub(crate) max_retrans: usize,
    pub(crate) close_tx: Arc<Mutex<Option<mpsc::Sender<()>>>>,
}

impl<T: 'static + RtxTimerObserver + Send> RtxTimer<T> {
    /// newRTXTimer creates a new retransmission timer.
    /// if max_retrans is set to 0, it will keep retransmitting until stop() is called.
    /// (it will never make on_retransmission_failure() callback.
    pub(crate) fn new(
        timeout_observer: Weak<Mutex<T>>,
        id: RtxTimerId,
        max_retrans: usize,
    ) -> Self {
        RtxTimer {
            timeout_observer,
            id,
            max_retrans,
            close_tx: Arc::new(Mutex::new(None)),
        }
    }

    /// start starts the timer.
    pub(crate) async fn start(&self, rto: u64) -> bool {
        // Note: rto value is intentionally not capped by RTO.Min to allow
        // fast timeout for the tests. Non-test code should pass in the
        // rto generated by rtoManager get_rto() method which caps the
        // value at RTO.Min or at RTO.Max.

        // this timer is already closed
        let mut close_rx = {
            let mut close = self.close_tx.lock().await;
            if close.is_some() {
                return false;
            }

            let (close_tx, close_rx) = mpsc::channel(1);
            *close = Some(close_tx);
            close_rx
        };

        let id = self.id;
        let max_retrans = self.max_retrans;
        let close_tx = Arc::clone(&self.close_tx);
        let timeout_observer = self.timeout_observer.clone();

        tokio::spawn(async move {
            let mut n_rtos = 0;

            loop {
                let interval = calculate_next_timeout(rto, n_rtos);
                let timer = tokio::time::sleep(Duration::from_millis(interval));
                tokio::pin!(timer);

                tokio::select! {
                    _ = timer.as_mut() => {
                        n_rtos+=1;

                        let failure = {
                            if let Some(observer) = timeout_observer.upgrade(){
                                let mut observer = observer.lock().await;
                                if max_retrans == 0 || n_rtos <= max_retrans {
                                    observer.on_retransmission_timeout(id, n_rtos).await;
                                    false
                                } else {
                                    observer.on_retransmission_failure(id).await;
                                    true
                                }
                            }else{
                                true
                            }
                        };
                        if failure {
                            let mut close = close_tx.lock().await;
                            *close = None;
                            break;
                        }
                    }
                    _ = close_rx.recv() => break,
                }
            }
        });

        true
    }

    /// stop stops the timer.
    pub(crate) async fn stop(&self) {
        let mut close_tx = self.close_tx.lock().await;
        close_tx.take();
    }

    /// isRunning tests if the timer is running.
    /// Debug purpose only
    pub(crate) async fn is_running(&self) -> bool {
        let close_tx = self.close_tx.lock().await;
        close_tx.is_some()
    }
}
