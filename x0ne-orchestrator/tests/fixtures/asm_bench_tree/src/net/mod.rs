pub mod client;

pub const DEFAULT_PORT: u16 = 8080;

pub fn url(host: &str) -> String {
    format!("http://{host}:{DEFAULT_PORT}")
}
