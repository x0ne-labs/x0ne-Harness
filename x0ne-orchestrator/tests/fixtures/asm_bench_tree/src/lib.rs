//! Toy crate.
pub mod config;
pub mod parser;
pub mod store;
pub mod net;

pub fn version() -> &'static str {
    "0.1.0"
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_version_is_set() {
        assert_eq!(super::version(), "0.1.0");
    }
}
