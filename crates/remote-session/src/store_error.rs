//! Enrollment storage errors shared by desktop and CLI presentation.

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct StoreError {
    pub code: &'static str,
    pub message: String,
    pub next_steps: Vec<String>,
}

impl StoreError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), next_steps: Vec::new() }
    }

    pub fn with_steps(mut self, steps: impl IntoIterator<Item = String>) -> Self {
        self.next_steps = steps.into_iter().collect();
        self
    }
}
