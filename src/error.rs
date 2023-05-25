use tracing::error;

#[derive(Debug)]
pub struct AppError(pub &'static str);

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for AppError {
    fn description(&self) -> &str {
        self.0
    }
}

pub async fn handle_error(e: impl std::error::Error) {
    // TODO: We may want to do more than just print the message...
    error!("ERROR: {e}")
}
