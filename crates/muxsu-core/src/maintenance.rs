//! Restarting a display that is on the right input and showing nothing.
//!
//! What is *not* here is the other half of that recovery — sending the display
//! out through another input and back, to re-bind a built-in USB hub or KVM
//! that follows the active input. That cannot live at this layer: a display
//! answers DDC/CI on the input it is showing and no other, so the host that
//! leaves cannot write to the display again, and the way back has to be asked
//! of the host that is then on screen. It belongs where the paired hosts are.

use std::{thread, time::Duration};

use crate::{DisplayMuxError, MonitorControl, MonitorId, PowerState};

/// How long the panel stays dark before it is told to come back. Long enough
/// for the display's own controller to drop the link it is stuck on, which is
/// the whole point of the exercise.
const POWER_OFF_DWELL: Duration = Duration::from_millis(2_500);

/// How long the panel is given to come back before its input is re-asserted.
const POWER_ON_SETTLE: Duration = Duration::from_millis(1_500);

/// Why a power cycle stopped, and what it left on the display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MaintenanceError {
    /// The display refused the off command, so nothing was changed.
    Refused(DisplayMuxError),
    /// The display accepted the off command and would not come back on. Its
    /// own power button is the way back.
    StuckOff(DisplayMuxError),
}

/// Turns the display off and back on over DDC/CI (VCP 0xD6), leaving every
/// input selection as it was, then names its input again on the way out.
///
/// The re-assert is for the display's own USB hub or KVM, which binds itself
/// to the *active input* rather than to the panel. Observed on an MSI MPG
/// 274U: the picture comes back from a power cycle and the USB devices do
/// not, because nothing told the display its input again. Writing the input
/// the display was already on costs one command and asks for that binding
/// back; a display that ignores a write of the value it already holds is no
/// worse off. A display that re-seats its own USB needs a real input change,
/// and a real input change needs the host that ends up on screen to undo it,
/// so that recovery lives with the paired hosts rather than here.
pub fn power_cycle<C: MonitorControl>(
    controller: &C,
    monitor: &MonitorId,
) -> Result<(), MaintenanceError> {
    power_cycle_with(controller, monitor, thread::sleep)
}

fn power_cycle_with<C: MonitorControl>(
    controller: &C,
    monitor: &MonitorId,
    mut wait: impl FnMut(Duration),
) -> Result<(), MaintenanceError> {
    // Read before the panel goes dark: a display that is off answers nothing.
    let previous = controller.read_input(monitor).ok();
    controller
        .write_power_state(monitor, PowerState::Off)
        .map_err(MaintenanceError::Refused)?;
    wait(POWER_OFF_DWELL);
    controller
        .write_power_state(monitor, PowerState::On)
        .map_err(MaintenanceError::StuckOff)?;

    // The panel is back, which is what was asked for. Whether the display
    // also took its input again decides only whether its USB came with it,
    // so a refusal here is logged rather than reported as a failed restart.
    if let Some(input) = previous {
        wait(POWER_ON_SETTLE);
        if let Err(error) = controller.write_input(monitor, input) {
            tracing::info!(
                monitor_id = monitor.as_str(),
                input = input.value(),
                error = %error,
                "display would not take its input again after a power cycle; a built-in USB hub or KVM may stay detached"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::{DisplayInput, MonitorDescriptor};

    fn input(value: u32) -> DisplayInput {
        DisplayInput::new(value).unwrap()
    }

    fn monitor() -> MonitorId {
        MonitorId::new("test:display")
    }

    fn backend() -> DisplayMuxError {
        DisplayMuxError::Backend("display did not answer".to_owned())
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Power(PowerState),
        Write(DisplayInput),
        Read,
    }

    #[derive(Default)]
    struct FakeController {
        calls: RefCell<Vec<Call>>,
        /// The input each read reports, oldest first; the last one repeats.
        reads: RefCell<Vec<Result<DisplayInput, DisplayMuxError>>>,
        /// Whether each write is accepted, oldest first; the last one repeats.
        writes: RefCell<Vec<Result<(), DisplayMuxError>>>,
        powers: RefCell<Vec<Result<(), DisplayMuxError>>>,
    }

    impl FakeController {
        fn reading(inputs: Vec<Result<DisplayInput, DisplayMuxError>>) -> Self {
            Self {
                reads: RefCell::new(inputs),
                ..Self::default()
            }
        }

        fn with_writes(self, writes: Vec<Result<(), DisplayMuxError>>) -> Self {
            *self.writes.borrow_mut() = writes;
            self
        }

        fn with_powers(self, powers: Vec<Result<(), DisplayMuxError>>) -> Self {
            *self.powers.borrow_mut() = powers;
            self
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.take()
        }
    }

    fn next<T: Clone>(queue: &RefCell<Vec<T>>, fallback: T) -> T {
        let mut queue = queue.borrow_mut();
        if queue.is_empty() {
            return fallback;
        }
        if queue.len() == 1 {
            return queue[0].clone();
        }
        queue.remove(0)
    }

    impl MonitorControl for FakeController {
        fn enumerate(&self) -> Result<Vec<MonitorDescriptor>, DisplayMuxError> {
            Ok(Vec::new())
        }

        fn read_input(&self, _monitor: &MonitorId) -> Result<DisplayInput, DisplayMuxError> {
            self.calls.borrow_mut().push(Call::Read);
            next(&self.reads, Ok(input(0x11)))
        }

        fn supported_inputs(
            &self,
            _monitor: &MonitorId,
        ) -> Result<Vec<DisplayInput>, DisplayMuxError> {
            Ok(Vec::new())
        }

        fn write_input(
            &self,
            _monitor: &MonitorId,
            value: DisplayInput,
        ) -> Result<(), DisplayMuxError> {
            self.calls.borrow_mut().push(Call::Write(value));
            next(&self.writes, Ok(()))
        }

        fn write_power_state(
            &self,
            _monitor: &MonitorId,
            state: PowerState,
        ) -> Result<(), DisplayMuxError> {
            self.calls.borrow_mut().push(Call::Power(state));
            next(&self.powers, Ok(()))
        }
    }

    /// The input is named again on the way out for the display's own USB hub
    /// or KVM, which follows the active input rather than the panel.
    #[test]
    fn a_power_cycle_turns_the_panel_off_and_back_on_and_names_its_input_again() {
        let controller = FakeController::reading(vec![Ok(input(0x11))]);
        let mut waits = Vec::new();

        let result = power_cycle_with(&controller, &monitor(), |delay| waits.push(delay));

        assert_eq!(result, Ok(()));
        assert_eq!(
            controller.calls(),
            vec![
                Call::Read,
                Call::Power(PowerState::Off),
                Call::Power(PowerState::On),
                Call::Write(input(0x11)),
            ]
        );
        assert_eq!(waits, vec![POWER_OFF_DWELL, POWER_ON_SETTLE]);
    }

    /// Only the input the display itself reported is ever written back, so a
    /// display that could not be read is restarted and left alone.
    #[test]
    fn an_unreadable_display_is_restarted_without_being_told_an_input() {
        let controller = FakeController::reading(vec![Err(backend())]);

        let result = power_cycle_with(&controller, &monitor(), |_| {});

        assert_eq!(result, Ok(()));
        assert_eq!(
            controller.calls(),
            vec![
                Call::Read,
                Call::Power(PowerState::Off),
                Call::Power(PowerState::On),
            ]
        );
    }

    /// The panel came back, which is what the button promised. A display that
    /// will not take its input again has kept its USB detached, and that is
    /// worth a log line, not a failed restart.
    #[test]
    fn a_refused_input_re_assert_does_not_fail_the_restart() {
        let controller =
            FakeController::reading(vec![Ok(input(0x11))]).with_writes(vec![Err(backend())]);

        assert_eq!(power_cycle_with(&controller, &monitor(), |_| {}), Ok(()));
    }

    #[test]
    fn a_display_that_refuses_to_turn_off_is_left_alone() {
        let controller = FakeController::default().with_powers(vec![Err(backend())]);

        let result = power_cycle_with(&controller, &monitor(), |_| {});

        assert_eq!(result, Err(MaintenanceError::Refused(backend())));
        assert_eq!(
            controller.calls(),
            vec![Call::Read, Call::Power(PowerState::Off)]
        );
    }

    /// The display took the off command, so the panel is dark and only its own
    /// power button can undo that. Reporting this as an ordinary refusal would
    /// send the user looking for a picture that is not coming back on its own.
    #[test]
    fn a_display_that_will_not_wake_is_reported_as_left_off() {
        let controller = FakeController::default().with_powers(vec![Ok(()), Err(backend())]);

        let result = power_cycle_with(&controller, &monitor(), |_| {});

        assert_eq!(result, Err(MaintenanceError::StuckOff(backend())));
    }
}
