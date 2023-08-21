//! Register of information from received packets for generating [NACK]s.
//!
//! [NACK]: https://webrtcglossary.com/nack

use std::num::TryFromIntError;

// use itertools::Itertools;
// use webrtc::rtcp::transport_feedbacks::transport_layer_nack::{
//     NackPair, TransportLayerNack,
// };

/// Range of packets we are gathering feedback for.
const MAX_DROPOUT: usize = 3000;

/// Range of packets we consider OK to be disordered.
const MAX_DISORDER: u64 = 100;

/// Minimal number of sequential packets to let us consider it's a consistent
/// stream.
const MIN_SEQUENTIAL: u64 = 2;

/// Range of packets before the last of packets in [`MAX_DROPOUT`] window that
/// has a "grace period". [NACK]s are not generated for them.
///
/// [NACK]: https://webrtcglossary.com/nack
const MISORDER_DELAY: u64 = 4;

/// The max number of [NACK]s we might send for a single packet.
///
/// [NACK]: https://webrtcglossary.com/nack
const MAX_NACKS: u8 = 5;

/// Status of a packet.
#[derive(Clone, Copy, Debug, Default)]
struct PacketStatus {
    /// Indicator whether this packet is received.
    received: bool,

    /// Quantity of [NACK]s sent for this packet.
    ///
    /// [NACK]: https://webrtcglossary.com/nack
    nack_count: u8,
}

impl PacketStatus {
    /// Indicates whether we should generate a [NACK] for this packet.
    ///
    /// [NACK]: https://webrtcglossary.com/nack
    const fn should_nack(self) -> bool {
        !self.received && self.nack_count < MAX_NACKS
    }
}

/// Register of information from received packets for generating [NACK]s.
///
/// [NACK]: https://webrtcglossary.com/nack
#[derive(Debug, Clone)]
pub struct NackRegister {
    /// Track of [`PacketStatus`]s.
    packet_statuses: [PacketStatus; MAX_DROPOUT],

    /// Max ever registered sequence number.
    max_seq: u64,

    /// Sequence number of received packet that seems abnormal. When the stream
    /// become normal it is [`None`].
    jump_seq: Option<u64>,

    /// Sequential packets remaining until source is valid.
    probation: u64,

    /// The sequence number from which we should generate the next [NACK]s
    /// feedback.
    ///
    /// [NACK]: https://webrtcglossary.com/nack
    check_from: u64,
}

impl NackRegister {
    /// Create a new [`NackRegister`].
    pub fn new(base_seq: u16) -> Self {
        Self {
            packet_statuses: [PacketStatus::default(); MAX_DROPOUT],
            max_seq: base_seq.saturating_sub(1).into(),
            jump_seq: None,
            probation: MIN_SEQUENTIAL,
            check_from: u64::from(base_seq),
        }
    }

    /// Resets this [`NackRegister`] and inits with the provided sequence
    /// number.
    fn init_seq(&mut self, seq: u64) {
        self.max_seq = seq;
        self.jump_seq = None;
        self.packet_statuses.fill(PacketStatus::default());
        self.record_received(seq, 0);
        self.check_from = seq;
    }

    /// Sets an indication that the packet was received with the provided
    /// sequence number and `forward_delta`.
    #[allow(clippy::as_conversions)]
    fn record_received(&mut self, seq: u64, forward_delta: u64) {
        if seq < self.check_from {
            return;
        }

        let did_wrap = (seq / self.packet_statuses.len() as u64)
            != (seq.saturating_sub(forward_delta)
            / self.packet_statuses.len() as u64);

        let pos = self.packet_index(seq);
        self.packet_statuses[pos].received = true;

        if did_wrap {
            let start = seq + 1;
            let end = self.check_from;

            self.reset_receceived(start, end);
        }

        let check_up_to = (self.max_seq).saturating_sub(MISORDER_DELAY);
        let new_nack_check_from = {
            let consecutive_unil = (self.check_from..=check_up_to)
                .take_while(|sn| {
                    self.packet_statuses[self.packet_index(*sn)].received
                })
                .last()
                .map(Into::into);

            #[allow(clippy::if_then_some_else_none)]
            match consecutive_unil {
                Some(new) if new != self.check_from => {
                    log::trace!(
                        "Moving nack_check_from forward from {} to {} on \
                         consecutive packet run",
                        self.check_from,
                        new,
                    );

                    Some(new)
                }
                _ => {
                    // No consecutive run, make sure we don't let
                    // nack_check_from fall too far
                    if check_up_to.saturating_sub(self.check_from)
                        > MAX_DISORDER
                    {
                        // If nack_check_from is falling too far behind bring it
                        // forward by discarding
                        // older packets.
                        let forced_nack_check_from = check_up_to - MAX_DISORDER;
                        log::trace!(
                            "Forcing nack_check_from forward from {} to {} on \
                             non-consecutive packet run",
                            self.check_from,
                            forced_nack_check_from,
                        );

                        Some(forced_nack_check_from)
                    } else {
                        None
                    }
                }
            }
        };

        if let Some(new_nack_check_from) = new_nack_check_from {
            self.reset_receceived(self.check_from, new_nack_check_from);
            self.check_from = new_nack_check_from;
        }
    }

    /// Resets packet statuses in the provided range.
    fn reset_receceived(&mut self, start: u64, end: u64) {
        for seq in start..end {
            let index = self.packet_index(seq);
            self.packet_statuses[index] = PacketStatus::default();
        }
    }

    /// Register the last received packet.
    pub fn update_seq(&mut self, seq: u16) {
        let seq = extend_u16(self.max_seq, seq);

        if self.probation > 0 {
            if seq == self.max_seq.wrapping_add(1) {
                self.probation -= 1;
                self.max_seq = seq;
                if self.probation == 0 {
                    self.init_seq(seq);
                }
            } else {
                self.probation = MIN_SEQUENTIAL - 1;
                self.max_seq = seq;
            }
        } else if self.max_seq < seq {
            let udelta = seq - self.max_seq;

            #[allow(clippy::as_conversions)]
            if udelta < MAX_DROPOUT as u64 {
                self.max_seq = seq;
                self.jump_seq = None;
                self.record_received(seq, udelta);
            } else {
                self.maybe_seq_jump(seq);
            }
        } else {
            let udelta = self.max_seq - seq;

            if udelta < MAX_DISORDER {
                self.record_received(seq, 0);
            } else {
                self.maybe_seq_jump(seq);
            }
        }
    }

    /// Register the provided sequence number as "jump" and calls
    /// [`NackRegister::init_seq()`], if needed.
    fn maybe_seq_jump(&mut self, seq: u64) {
        if self.jump_seq == Some(seq) {
            self.init_seq(seq);
        } else {
            self.jump_seq = Some(seq + 1);
        }
    }

    /// Tries to generate [`TransportLayerNack`]s.
    #[allow(clippy::else_if_without_else)]
    pub fn nack_reports(
        &mut self,
    ) -> Result<Option<Vec<Vec<(u16, u16)>>>, TryFromIntError> {
        if self.probation > 0 {
            return Ok(None);
        }

        let start = self.check_from;
        let stop = self.max_seq.saturating_sub(MISORDER_DELAY);
        let u16max = u64::from(u16::MAX) + 1_u64;

        if stop < start {
            return Ok(None);
        }

        let mut nacks = vec![];
        let mut first_missing = None;
        let mut bitmask = 0;

        for i in start..stop {
            let j = self.packet_index(i);

            let should_nack = self.packet_statuses[j].should_nack();

            if let Some(first) = first_missing {
                if should_nack {
                    let o = u16::try_from(i - (first + 1))?;
                    bitmask |= 1 << o;
                    self.packet_statuses[j].nack_count += 1;
                }

                if i - first == 16 {
                    nacks.push((
                        u16::try_from(first % u16max)?,
                        bitmask,
                    ));
                    bitmask = 0;
                    first_missing = None;
                }
            } else if should_nack {
                self.packet_statuses[j].nack_count += 1;
                first_missing = Some(i);
            }
        }

        if let Some(first) = first_missing {
            nacks.push((
                u16::try_from(first % u16max)?,
                bitmask,
            ));
        }

        if nacks.is_empty() {
            return Ok(None);
        }

        let reports = {
            let mut result = vec![];
            let mut current = vec![];

            for (i, item) in nacks.into_iter().enumerate() {
                if i % 31 == 0 && i != 0 {
                    result.push(current);
                    current = vec![];
                }

                current.push(item);
            }

            if !current.is_empty() {
                result.push(current);
            }

            result
        };

        Ok(Some(
            reports
        ))
    }

    /// Returns an index of [`PacketStatus`] for the provided sequence number.
    #[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
    const fn packet_index(&self, seq: u64) -> usize {
        (seq % self.packet_statuses.len() as u64) as usize
    }
}

/// "extend"s a less than 64 bit sequence number into a 64 bit by using the
/// knowledge of the previous such sequence number.
pub fn extend_u16(prev_ext_seq: u64, seq: u16) -> u64 {
    const MAX: u64 = 1 << 16;
    const HALF: u64 = MAX / 2;
    #[allow(clippy::as_conversions, clippy::cast_possible_wrap)]
    const ROC_MASK: i64 = (u64::MAX >> 16) as i64;

    let seq = u64::from(seq);
    #[allow(clippy::as_conversions, clippy::cast_possible_wrap)]
        let roc = (prev_ext_seq >> 16) as i64;
    let prev_seq = prev_ext_seq & (MAX - 1);

    let v = if prev_seq < HALF {
        if seq > HALF + prev_seq {
            (roc - 1) & ROC_MASK
        } else {
            roc
        }
    } else if prev_seq > seq + HALF {
        (roc + 1) & ROC_MASK
    } else {
        roc
    };

    u64::try_from(v).map_or(0, |s| s * MAX + seq)
}

/// Recorder for [NACK] stats.
///
/// [NACK]: https://webrtcglossary.com/nack
#[derive(Debug)]
struct Recorder {
    /// [SSRC] of sender for which this [`Recorder`] calculates [NACK] stats.
    ///
    /// [NACK]: https://webrtcglossary.com/nack
    /// [SSRC]: https://w3.org/TR/webrtc-stats#dom-rtcrtpstreamstats-ssrc
    sender_ssrc: u32,

    /// [SSRC] of media source for which this [`Recorder`] calculates [NACK]
    /// stats.
    ///
    /// [NACK]: https://webrtcglossary.com/nack
    /// [SSRC]: https://w3.org/TR/webrtc-stats#dom-rtcrtpstreamstats-ssrc
    media_ssrc: Option<u32>,

    /// Calculator of the [NACK] stats.
    ///
    /// [NACK]: https://webrtcglossary.com/nack
    nack_register: Option<NackRegister>,
}

impl Recorder {
    /// Creates a new [`Recorder`] which uses the given `sender_ssrc` in the
    /// created feedback packets.
    #[must_use]
    pub const fn new(sender_ssrc: u32) -> Self {
        Self {
            sender_ssrc,
            nack_register: None,
            media_ssrc: None,
        }
    }

    /// Marks a packet with the provided `media_ssrc` and transport wide
    /// `sequence_number` as received.
    pub fn record(&mut self, media_ssrc: u32, seq: u16) {
        self.nack_register
            .get_or_insert(NackRegister::new(seq))
            .update_seq(seq);
        if self.media_ssrc.is_none() {
            self.media_ssrc = Some(media_ssrc);
        }
    }

    /// Creates a new [RTCP] packet containing a [NACK] feedback report.
    ///
    /// [NACK]: https://webrtcglossary.com/nack
    /// [RTCP]: https://webrtcglossary.com/rtcp
    pub fn build_feedback(&mut self) -> Option<Vec<Vec<(u16, u16)>>> {
        match (self.media_ssrc, &mut self.nack_register) {
            (Some(media_ssrc), Some(nacker)) => nacker
                .nack_reports()
                .unwrap_or_else(|e| {
                    log::error!("Failed to gather NACKs: {e}");
                    None
                })
                .map(|mut fbs| {
                    fbs
                }),
            _ => None,
        }
    }
}

