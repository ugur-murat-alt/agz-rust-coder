use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Non-blocking admission for all tool calls.
#[derive(Clone)]
pub struct AdmissionController {
    permits: Arc<Semaphore>,
    closing: Arc<AtomicBool>,
}

impl fmt::Debug for AdmissionController {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdmissionController")
            .field("available_permits", &self.permits.available_permits())
            .field("closing", &self.is_closing())
            .finish()
    }
}

impl AdmissionController {
    pub fn new(max_in_flight: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(max_in_flight)),
            closing: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn try_acquire(&self) -> Result<OwnedSemaphorePermit, ToolAdmissionError> {
        if self.is_closing() {
            return Err(ToolAdmissionError::Closing);
        }
        self.permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| ToolAdmissionError::InFlightLimit)
    }

    pub fn close(&self) {
        if !self.closing.swap(true, Ordering::AcqRel) {
            self.permits.close();
        }
    }

    pub fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolAdmissionError {
    Closing,
    InFlightLimit,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_never_waits_and_closing_is_terminal() {
        let admission = AdmissionController::new(1);
        let permit = admission.try_acquire().expect("first permit");
        assert_eq!(
            admission.try_acquire().unwrap_err(),
            ToolAdmissionError::InFlightLimit
        );
        drop(permit);
        admission.close();
        assert_eq!(
            admission.try_acquire().unwrap_err(),
            ToolAdmissionError::Closing
        );
    }
}
