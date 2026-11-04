use super::url;

pub struct Client {
    base: String,
}

impl Client {
    pub fn new(host: &str) -> Client {
        Client { base: url(host) }
    }

    pub fn get(&self, path: &str) -> String {
        // TODO: real HTTP
        format!("{}{}", self.base, path)
    }

    pub fn port_of(&self) -> u16 {
        self.base.rsplit(':').next().unwrap().parse().unwrap()
    }
}
