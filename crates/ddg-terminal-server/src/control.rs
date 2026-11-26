//! Explicit single-controller lease and monotonic epoch tracking per PTY.

use ddg_agents_proto::terminal::{ControlLease, ControlMode};

#[derive(Clone, Debug, Default)]
pub struct ControlState {
    controller: Option<ControlLease>,
    next_epoch: u64,
}

impl ControlState {
    pub fn current(&self) -> Option<ControlLease> {
        self.controller
    }

    /// Evaluates an attach request. Grants control only if `want_control` is true
    /// and no other connection currently holds control.
    pub fn on_attach(&mut self, conn_id: u64, want_control: bool) -> (ControlMode, u64, bool) {
        if !want_control {
            return (ControlMode::Readonly, self.next_epoch, false);
        }
        match self.controller {
            None => {
                self.next_epoch += 1;
                self.controller = Some(ControlLease {
                    conn_id,
                    epoch: self.next_epoch,
                });
                (ControlMode::Control, self.next_epoch, true)
            }
            Some(lease) if lease.conn_id == conn_id => (ControlMode::Control, lease.epoch, false),
            Some(_) => (ControlMode::Readonly, self.next_epoch, false),
        }
    }

    /// Explicitly claims control. Fails with an error when another connection
    /// holds control and `force` is false.
    pub fn claim(&mut self, conn_id: u64, force: bool) -> Result<(u64, bool), String> {
        match self.controller {
            Some(lease) if lease.conn_id == conn_id => Ok((lease.epoch, false)),
            Some(lease) if !force => Err(format!(
                "PTY is currently controlled by connection {} (pass force=true to take over)",
                lease.conn_id
            )),
            _ => {
                self.next_epoch += 1;
                self.controller = Some(ControlLease {
                    conn_id,
                    epoch: self.next_epoch,
                });
                Ok((self.next_epoch, true))
            }
        }
    }

    pub fn release(&mut self, conn_id: u64) -> bool {
        if self.controller.is_some_and(|l| l.conn_id == conn_id) {
            self.controller = None;
            true
        } else {
            false
        }
    }

    pub fn verify(&self, conn_id: u64, epoch: u64) -> Result<(), String> {
        match self.controller {
            Some(lease) if lease.conn_id == conn_id && lease.epoch == epoch => Ok(()),
            Some(lease) if lease.conn_id == conn_id => Err(format!(
                "stale control epoch {epoch} (current is {})",
                lease.epoch
            )),
            Some(lease) => Err(format!(
                "connection {conn_id} is read-only (controlled by connection {})",
                lease.conn_id
            )),
            None => Err("no active controller for PTY".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_single_controller_and_rejects_stale_epochs() {
        let mut ctrl = ControlState::default();
        let (mode, ep1, changed) = ctrl.on_attach(1, true);
        assert_eq!((mode, ep1, changed), (ControlMode::Control, 1, true));
        assert!(ctrl.verify(1, ep1).is_ok());

        // Second viewer attaching with want_control=true starts read-only.
        let (mode2, _, changed2) = ctrl.on_attach(2, true);
        assert_eq!((mode2, changed2), (ControlMode::Readonly, false));
        assert!(ctrl.verify(2, ep1).is_err());

        // Non-forced claim fails while conn 1 controls.
        assert!(ctrl.claim(2, false).is_err());

        // Forced claim bumps epoch and invalidates conn 1's old epoch.
        let (ep2, changed3) = ctrl.claim(2, true).unwrap();
        assert!(changed3 && ep2 > ep1);
        assert!(ctrl.verify(2, ep2).is_ok());
        assert!(ctrl.verify(1, ep1).is_err());

        // Releasing clears control so conn 1 can claim cleanly without force.
        assert!(ctrl.release(2));
        assert!(!ctrl.release(2));
        let (ep3, _) = ctrl.claim(1, false).unwrap();
        assert!(ep3 > ep2);
        assert!(ctrl.verify(1, ep1).is_err());
        assert!(ctrl.verify(1, ep3).is_ok());
    }
}
